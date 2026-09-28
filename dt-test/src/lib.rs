//! BooMGateway DT 测试 harness — 进程内代码覆盖测试形态。
//!
//! DT 用例**直接调用 boom-\* lib crate 的 pub API**（path 依赖源码级链接），
//! 不拉子进程：所有被测代码与用例同进程，cargo-llvm-cov 能完整统计
//! 功能源码覆盖率。boom-main 是纯 bin crate（组装层），不在 DT 覆盖范围。
//!
//! 提供的基础设施：
//!
//! - [`MockUpstream`] — wiremock 模拟 OpenAI 协议上游（可定制响应/延迟/错误）
//! - [`TestServer`]   — 进程内把任意 axum `Router` 起在随机端口（测 HTTP 层组件时用）
//! - [`free_port`]    — 随机空闲端口
//! - [`chat_request`] — 从 JSON 构造 `ChatCompletionRequest`（字段多，走反序列化省手写）

use std::net::TcpListener;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 分配一个空闲 TCP 端口（bind :0 后立刻释放，存在极小竞态窗口）。
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind 127.0.0.1:0 for free port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// 从 JSON 值构造 `ChatCompletionRequest`。
///
/// 该结构体字段二十余个且多数 `Option`，手写构造冗长；它实现了
/// `Deserialize`，从 JSON 反序列化最省事，也与请求的真实来源（HTTP body）一致。
pub fn chat_request(body: serde_json::Value) -> boom_core::types::ChatCompletionRequest {
    serde_json::from_value(body).expect("deserialize ChatCompletionRequest")
}

/// 构造一条最小 chat 请求（单条 user 消息）。
pub fn simple_chat_request(model: &str, content: &str) -> boom_core::types::ChatCompletionRequest {
    chat_request(serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": content}]
    }))
}

/// 模拟的 OpenAI 协议上游。
///
/// [`MockUpstream::start_chat_ok`] 覆盖最常见的"正常回复"场景；
/// 其他场景（错误码、慢响应、断流）直接拿 `server()` 挂自定义 wiremock Mock。
pub struct MockUpstream {
    server: MockServer,
}

impl MockUpstream {
    /// 起一个 `/v1/chat/completions` 返回 200 + 指定 content 的 mock 上游。
    pub async fn start_chat_ok(content: &str) -> Self {
        let server = MockServer::start().await;
        Self::mount_chat_ok(&server, content).await;
        Self { server }
    }

    /// 只起服务器不挂默认 Mock，用例自行 `mount`。
    pub async fn start_empty() -> Self {
        Self {
            server: MockServer::start().await,
        }
    }

    async fn mount_chat_ok(server: &MockServer, content: &str) {
        let body = serde_json::json!({
            "id": "chatcmpl-dt-mock",
            "object": "chat.completion",
            "created": 1_700_000_000_u64,
            "model": "dt-mock-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 9,
                "total_tokens": 18
            }
        });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// 重新挂一个"正常回复" Mock（比如想覆盖前面挂的错误 Mock）。
    pub async fn remount_chat_ok(&self, content: &str) {
        Self::mount_chat_ok(&self.server, content).await;
    }

    /// 上游基地址（作为 provider 的 `api_base` 时取 `format!("{}/v1", uri)")`）。
    pub fn uri(&self) -> String {
        self.server.uri()
    }

    pub fn server(&self) -> &MockServer {
        &self.server
    }
}

/// 进程内 HTTP 测试服务器：把一个 axum `Router` 起在随机端口。
///
/// 用于测试 boom-* 各 crate 暴露的 HTTP 组件（如 dashboard 的 Router、
/// trace 的 OTLP 接收端）。Drop 时自动停止服务。
pub struct TestServer {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub async fn serve(router: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let port = listener.local_addr().expect("local_addr").port();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { port, task }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    pub fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("build reqwest client")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
