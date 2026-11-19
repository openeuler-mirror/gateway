//! HTTP 测试客户端：发 chat 请求、提取 usage（期望值锚点）、收集 429 / Retry-After 证据。

use futures::StreamExt;
use serde_json::{json, Value};

#[derive(Clone)]
pub struct HttpTestClient {
    base: String,
    key: String,
    client: reqwest::Client,
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

/// 一次 chat 请求的可观察结果。usage 从响应 JSON 提取（期望值锚点 = 客户端观测 usage 之和）。
#[derive(Clone, Debug)]
pub struct ChatOutcome {
    pub status: u16,
    pub usage: Option<Usage>,
    /// 小写 header 名 → 值（retry-after 断言层自取）
    pub headers: Vec<(String, String)>,
    pub body_snippet: String,
}

impl HttpTestClient {
    pub fn new(base_url: &str, key: &str) -> Self {
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            key: key.to_string(),
            client: reqwest::Client::builder()
                // 目标全是 runner 本机 spawn 的子进程，环境代理变量会劫持 127.0.0.1 请求
                .no_proxy()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("reqwest client"),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn auth(&self) -> reqwest::RequestBuilder {
        // Bearer 是 litellm/OpenAI 通行格式；若鉴权头形式不同以调查结论为准
        let rb = self
            .client
            .post(format!("{}/v1/chat/completions", self.base));
        rb.bearer_auth(&self.key)
    }

    /// 非流式 chat
    pub async fn chat(&self, model: &str, n_requests_hint: &str) -> anyhow::Result<ChatOutcome> {
        let body = json!({
            "model": model,
            "messages": [{ "role": "user", "content": format!("e2e probe: {n_requests_hint}") }],
        });
        let resp = self.auth().json(&body).send().await?;
        outcome_from_response(resp).await
    }

    /// 流式 chat；usage 取自最后一个带 usage 的 SSE chunk
    pub async fn chat_stream(
        &self,
        model: &str,
        n_requests_hint: &str,
    ) -> anyhow::Result<ChatOutcome> {
        let body = json!({
            "model": model,
            "stream": true,
            "messages": [{ "role": "user", "content": format!("e2e probe: {n_requests_hint}") }],
        });
        let resp = self.auth().json(&body).send().await?;
        let status = resp.status().as_u16();
        let headers = collect_headers(resp.headers());
        if status != 200 {
            let text = resp.text().await.unwrap_or_default();
            return Ok(ChatOutcome {
                status,
                usage: None,
                headers,
                body_snippet: snippet(&text),
            });
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut usage: Option<Usage> = None;
        let mut body_chars = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let line = line.trim();
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.trim();
                    if data == "[DONE]" {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if let Some(u) = parse_usage(&v) {
                            usage = Some(u);
                        }
                        if let Some(delta) = v["choices"][0]["delta"]["content"].as_str() {
                            body_chars += delta.len();
                        }
                    }
                }
            }
        }
        Ok(ChatOutcome {
            status,
            usage,
            headers,
            body_snippet: format!("<stream, {body_chars} content chars>"),
        })
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<(u16, String, Vec<(String, String)>)> {
        let resp = self
            .client
            .get(format!("{}{}", self.base, path))
            .bearer_auth(&self.key)
            .send()
            .await?;
        let headers = collect_headers(resp.headers());
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text, headers))
    }
}

fn parse_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?;
    Some(Usage {
        prompt_tokens: u["prompt_tokens"].as_i64()?,
        completion_tokens: u["completion_tokens"].as_i64().unwrap_or(0),
        total_tokens: u["total_tokens"].as_i64().unwrap_or(0),
    })
}

async fn outcome_from_response(resp: reqwest::Response) -> anyhow::Result<ChatOutcome> {
    let status = resp.status().as_u16();
    let headers = collect_headers(resp.headers());
    let text = resp.text().await.unwrap_or_default();
    let usage = serde_json::from_str::<Value>(&text)
        .ok()
        .as_ref()
        .and_then(parse_usage);
    Ok(ChatOutcome {
        status,
        usage,
        headers,
        body_snippet: snippet(&text),
    })
}

fn collect_headers(h: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).to_string(),
            )
        })
        .collect()
}

fn snippet(s: &str) -> String {
    let s = s.trim();
    if s.len() <= 300 {
        s.to_string()
    } else {
        format!("{}…", &s[..300])
    }
}
