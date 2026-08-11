use crate::hybrid_router::{ClassifyRequest, ClassificationStrategy, TierClassifier};
use boom_core::types::{Message, Tool};
use std::collections::HashSet;

/// Request body sent to the ML service's `/classify` endpoint.
#[derive(serde::Serialize)]
struct MlClassifyBody<'a> {
    messages: &'a [Message],
    tools: &'a Option<Vec<Tool>>,
    default_tier: &'a str,
}

/// Response body expected from the ML service.
#[derive(serde::Deserialize)]
struct MlClassifyResponse {
    tier: String,
    #[allow(dead_code)]
    confidence: Option<f64>,
    #[allow(dead_code)]
    reason: Option<String>,
}

/// Classification strategy that delegates to an external HTTP service.
///
/// POSTs `{messages, tools, default_tier}` to `{url}/classify` and reads
/// `{"tier": "..."}` back. On any failure (timeout, HTTP error, JSON
/// parse error, unknown tier), falls back to [`TierClassifier`] so the
/// gateway never fails a request due to ML service outage.
///
/// # Why the trait is `async`
///
/// `ClassificationStrategy::classify` is an `async fn` (via `#[async_trait]`)
/// rather than a sync `fn` that internally does HTTP. Three alternatives were
/// considered and rejected:
///
/// - **Sync trait + `Handle::block_on`** — panics when the caller is itself
///   inside an axum worker (no fresh runtime to block on), and at medium QPS
///   it saturates the worker pool: every in-flight classify call pins a
///   tokio worker for the full HTTP RTT (5–50 ms), causing head-of-line
///   blocking that can make the whole gateway unresponsive during an ML
///   service timeout.
/// - **Sync trait + dedicated OS thread** — safe but caps concurrency at the
///   thread-pool size; one `reqwest::Client` per thread also defeats HTTP
///   keep-alive across requests.
/// - **Async trait (chosen)** — `reqwest::Client`'s connection pool scales
///   request concurrency to the pool/connection limit, not the worker count.
///   A 30 s ML-service timeout only accumulates pending tasks; other
///   requests still get served. `TierClassifier` (pure CPU) is zero-cost as
///   an `async fn` — the future resolves synchronously on first poll.
///
/// The per-call trait overhead (ns–µs) is invisible next to HTTP RTT
/// (5–50 ms); behavior under load and during ML outage is what decided it.
pub struct MlServiceClient {
    http: reqwest::Client,
    url: String,
    fallback: TierClassifier,
    valid_tiers: HashSet<String>,
}

impl MlServiceClient {
    /// Construct a new client. `url` is the base URL (e.g. `http://127.0.0.1:2345`);
    /// the gateway appends `/classify`. `timeout_ms` covers connect + write + read.
    /// `valid_tiers` is the set of acceptable tier names (typically built from
    /// the config's `tiers` keys).
    pub fn new(url: &str, timeout_ms: u64, valid_tiers: HashSet<String>) -> Self {
        let timeout = std::time::Duration::from_millis(timeout_ms.max(10));
        let http = reqwest::Client::builder()
            .timeout(timeout)
            // The ML service is an internal sidecar (typically on 127.0.0.1).
            // Bypass any HTTP_PROXY/ALL_PROXY environment variables so local
            // traffic doesn't get routed through an external proxy.
            .no_proxy()
            .build()
            .expect("reqwest::Client build with sane defaults");
        Self {
            url: format!("{}/classify", url.trim_end_matches('/')),
            fallback: TierClassifier,
            http,
            valid_tiers,
        }
    }
}

#[async_trait::async_trait]
impl ClassificationStrategy for MlServiceClient {
    fn name(&self) -> &str {
        "ml_service"
    }

    async fn classify(&self, req: &ClassifyRequest<'_>) -> String {
        let body = MlClassifyBody {
            messages: req.messages,
            tools: req.tools,
            default_tier: req.default_tier,
        };

        let result = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await;

        match result {
            Ok(response) => {
                if !response.status().is_success() {
                    tracing::warn!(
                        status = %response.status(),
                        "ML service returned non-2xx, using heuristic fallback"
                    );
                    return self.fallback.classify(req).await;
                }
                match response.json::<MlClassifyResponse>().await {
                    Ok(parsed) if self.valid_tiers.contains(&parsed.tier) => {
                        tracing::debug!(
                            tier = %parsed.tier,
                            confidence = ?parsed.confidence,
                            "ML service classified"
                        );
                        parsed.tier
                    }
                    Ok(parsed) => {
                        tracing::warn!(
                            tier = %parsed.tier,
                            "ML service returned unknown tier, using heuristic fallback"
                        );
                        self.fallback.classify(req).await
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Failed to parse ML service response, using heuristic fallback"
                        );
                        self.fallback.classify(req).await
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "ML service unavailable, using heuristic fallback"
                );
                self.fallback.classify(req).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boom_core::types::{Message, MessageContent, MessageRole};

    fn make_messages(texts: Vec<(&str, &str)>) -> Vec<Message> {
        texts
            .into_iter()
            .map(|(role, text)| Message {
                role: match role {
                    "user" => MessageRole::User,
                    "system" => MessageRole::System,
                    "assistant" => MessageRole::Assistant,
                    _ => MessageRole::User,
                },
                content: MessageContent::Text(text.to_string()),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            })
            .collect()
    }

    #[tokio::test]
    async fn ml_service_returns_valid_tier() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/classify")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"tier": "large"}"#)
            .create_async()
            .await;

        let client = MlServiceClient::new(
            &server.url(),
            500,
            HashSet::from(["small".to_string(), "medium".to_string(), "large".to_string()]),
        );

        let msgs = make_messages(vec![("user", "hello")]);
        let req = ClassifyRequest {
            messages: &msgs,
            tools: &None,
            default_tier: "medium",
        };
        let tier = client.classify(&req).await;
        assert_eq!(tier, "large");
    }

    #[tokio::test]
    async fn ml_service_falls_back_on_invalid_tier() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/classify")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"tier": "unknown"}"#)
            .create_async()
            .await;

        let client = MlServiceClient::new(
            &server.url(),
            500,
            HashSet::from(["small".to_string(), "medium".to_string(), "large".to_string()]),
        );

        let msgs = make_messages(vec![("user", "hi")]);
        let req = ClassifyRequest {
            messages: &msgs,
            tools: &None,
            default_tier: "medium",
        };
        let tier = client.classify(&req).await;
        // Heuristic fallback for a short greeting returns "small".
        assert_eq!(tier, "small");
    }

    #[tokio::test]
    async fn ml_service_falls_back_on_5xx() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/classify")
            .with_status(500)
            .create_async()
            .await;

        let client = MlServiceClient::new(
            &server.url(),
            500,
            HashSet::from(["small".to_string(), "medium".to_string(), "large".to_string()]),
        );

        let msgs = make_messages(vec![("user", "hi")]);
        let req = ClassifyRequest {
            messages: &msgs,
            tools: &None,
            default_tier: "medium",
        };
        let tier = client.classify(&req).await;
        assert_eq!(tier, "small");
    }

    #[tokio::test]
    async fn ml_service_falls_back_on_malformed_json() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/classify")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"not json at all"#)
            .create_async()
            .await;

        let client = MlServiceClient::new(
            &server.url(),
            500,
            HashSet::from(["small".to_string(), "medium".to_string(), "large".to_string()]),
        );

        let msgs = make_messages(vec![("user", "hi")]);
        let req = ClassifyRequest {
            messages: &msgs,
            tools: &None,
            default_tier: "medium",
        };
        let tier = client.classify(&req).await;
        assert_eq!(tier, "small");
    }

    #[tokio::test]
    async fn ml_service_falls_back_on_timeout() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/classify")
            .with_status(200)
            .with_chunked_body(|_w| {
                // Simulate a slow response — much longer than the 50ms client timeout.
                std::thread::sleep(std::time::Duration::from_secs(2));
                Ok(())
            })
            .create_async()
            .await;

        let client = MlServiceClient::new(
            &server.url(),
            50,
            HashSet::from(["small".to_string(), "medium".to_string(), "large".to_string()]),
        );

        let msgs = make_messages(vec![("user", "hi")]);
        let req = ClassifyRequest {
            messages: &msgs,
            tools: &None,
            default_tier: "medium",
        };
        let tier = client.classify(&req).await;
        // Falls back to heuristic; short greeting → small.
        assert_eq!(tier, "small");
    }

    #[tokio::test]
    async fn ml_service_falls_back_on_connection_refused() {
        // Use a port that's almost certainly closed.
        let client = MlServiceClient::new(
            "http://127.0.0.1:1",
            100,
            HashSet::from(["small".to_string(), "medium".to_string(), "large".to_string()]),
        );

        let msgs = make_messages(vec![("user", "hi")]);
        let req = ClassifyRequest {
            messages: &msgs,
            tools: &None,
            default_tier: "medium",
        };
        let tier = client.classify(&req).await;
        assert_eq!(tier, "small");
    }

    #[tokio::test]
    async fn ml_service_passes_messages_and_tools_in_body() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/classify")
            .match_body(mockito::Matcher::JsonString(
                r#"{"messages":[{"role":"user","content":"debug this"}],"tools":null,"default_tier":"medium"}"#.to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"tier": "large"}"#)
            .create_async()
            .await;

        let client = MlServiceClient::new(
            &server.url(),
            500,
            HashSet::from(["small".to_string(), "medium".to_string(), "large".to_string()]),
        );

        let msgs = make_messages(vec![("user", "debug this")]);
        let req = ClassifyRequest {
            messages: &msgs,
            tools: &None,
            default_tier: "medium",
        };
        let tier = client.classify(&req).await;
        assert_eq!(tier, "large");
        mock.assert_async().await;
    }
}
