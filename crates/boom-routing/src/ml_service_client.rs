use crate::auto_router::{ClassifyRequest, ClassificationStrategy, TierClassifier};
use crate::ml_service_stats::MlServiceStats;
use boom_core::types::{Message, Tool};
use std::collections::HashSet;
use std::sync::Arc;

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
/// # Construction
///
/// Use [`MlServiceClient::try_new`] — it validates the URL up-front and
/// returns `Err` on bad input (malformed URL, missing scheme/host). The
/// legacy [`MlServiceClient::new`] is kept for tests and panics on the
/// same conditions; production wiring in `boom-main::state` uses
/// `try_new` and logs the error instead of panicking.
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
/// # Async overhead on the `tier_classifier` path (the default)
///
/// Reviewers flagged that making the trait async forces every request —
/// including the common `strategy = "tier_classifier"` case — through
/// `async_trait`'s `Pin<Box<dyn Future>>` dispatch (one heap alloc +
/// vtable lookup per call). The cost is small and bounded:
///
/// - `TierClassifier::classify`'s future resolves on **first poll**
///   (no `.await` suspension, no scheduler involvement), so the state
///   machine never yields — the Box future is built, polled once, dropped.
/// - Per-call overhead: ~50 ns (Box alloc + vtable + drop).
/// - At 1000 QPS: 50 µs/s = 0.005% of one core. At 10 000 QPS: 500 µs/s
///   = 0.05% of one core.
///
/// This is invisible next to HTTP RTT (5–50 ms) on the `ml_service` path
/// and a non-issue for `tier_classifier` at any realistic gateway QPS.
/// The tradeoff — one uniform trait for both strategies, no special-case
/// sync dispatch — was judged worth it. If profiling ever shows the
/// allocation matters, an enum-dispatch refactor (`Strategy::{Tier,
/// MlService}`) can remove it without touching the public API.
pub struct MlServiceClient {
    http: reqwest::Client,
    url: String,
    fallback: TierClassifier,
    valid_tiers: HashSet<String>,
    stats: Arc<MlServiceStats>,
}

impl MlServiceClient {
    /// Fallible constructor. Validates `url` via [`reqwest::Url::parse`]
    /// (catches missing scheme, malformed host, etc.) before building the
    /// HTTP client. Returns `Err` with a human-readable message on bad
    /// input — the caller should log and skip ML strategy registration,
    /// letting the gateway fall back to `tier_classifier`.
    ///
    /// `timeout_ms` of 0 is rejected (would mean "no timeout" in reqwest,
    /// i.e. an ML service hang blocks every caller indefinitely).
    ///
    /// `url` is the base URL (e.g. `http://127.0.0.1:2345`); the client
    /// appends `/classify` at request time. `valid_tiers` is the set of
    /// acceptable tier names (typically built from the config's `tiers`
    /// keys).
    pub fn try_new(
        url: &str,
        timeout_ms: u64,
        valid_tiers: HashSet<String>,
    ) -> Result<Self, String> {
        if timeout_ms == 0 {
            return Err(format!(
                "ml_service.timeout_ms must be > 0 (got 0; reqwest treats 0 as \
                 'no timeout' which would let an unresponsive ML service hang \
                 every classifier call indefinitely)"
            ));
        }

        // Parse up-front so a bad URL (typo in YAML, missing scheme, etc.)
        // fails at startup, not on the first request. `reqwest::Url::parse`
        // re-exports `url::Url::parse`.
        let parsed = reqwest::Url::parse(url).map_err(|e| {
            format!("ml_service.url is not a valid URL: {url} ({e})")
        })?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(format!(
                "ml_service.url must use http or https scheme, got: {url}"
            ));
        }

        let timeout = std::time::Duration::from_millis(timeout_ms);
        let http = reqwest::Client::builder()
            .timeout(timeout)
            // The ML service is an internal sidecar (typically on 127.0.0.1).
            // Bypass any HTTP_PROXY/ALL_PROXY environment variables so local
            // traffic doesn't get routed through an external proxy.
            .no_proxy()
            .build()
            .map_err(|e| format!("reqwest::Client build failed: {e}"))?;

        let full_url = format!("{}/classify", url.trim_end_matches('/'));
        tracing::info!(
            endpoint = %full_url,
            timeout_ms,
            valid_tiers = ?valid_tiers.iter().collect::<Vec<_>>(),
            "ML service client ready"
        );

        Ok(Self {
            url: full_url,
            fallback: TierClassifier,
            http,
            valid_tiers,
            stats: Arc::new(MlServiceStats::new()),
        })
    }

    /// Legacy panicking constructor — kept for tests where the URL is
    /// known-good (e.g. mockito server URLs). Production code should
    /// use [`try_new`](Self::try_new).
    ///
    /// Panics on bad URL or non-positive timeout. The 10ms floor on
    /// `timeout_ms` was removed (review feedback: silent magic number);
    /// callers must pass a sane positive value.
    pub fn new(url: &str, timeout_ms: u64, valid_tiers: HashSet<String>) -> Self {
        Self::try_new(url, timeout_ms, valid_tiers)
            .expect("MlServiceClient::new called with invalid url or timeout")
    }

    /// Expose stats for tests and the future admin API. Production
    /// paths don't read this — the periodic summary is the export
    /// channel.
    pub fn stats(&self) -> &MlServiceStats {
        &self.stats
    }
}

#[async_trait::async_trait]
impl ClassificationStrategy for MlServiceClient {
    fn name(&self) -> &str {
        "ml_service"
    }

    async fn classify(&self, req: &ClassifyRequest<'_>) -> String {
        // 1. Maybe emit periodic summary (inline, no spawn). Runs BEFORE
        // record_attempt so the invariant `attempts == successes + failures`
        // holds in every summary — the triggering request is fully
        // accounted in the next window, not half-counted in this one.
        self.stats.maybe_emit_summary(&self.url);

        // 2. Record attempt + start latency timer.
        let attempt_start = self.stats.record_attempt();

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
                    tracing::debug!(
                        url = %self.url,
                        status = %response.status(),
                        "ML service returned non-2xx, using heuristic fallback"
                    );
                    self.stats.record_failure();
                    let tier = self.fallback.classify(req).await;
                    self.stats.record_fallback(&tier);
                    return tier;
                }
                match response.json::<MlClassifyResponse>().await {
                    Ok(parsed) if self.valid_tiers.contains(&parsed.tier) => {
                        let latency = attempt_start.elapsed();
                        self.stats.record_success(&parsed.tier, latency);
                        tracing::debug!(
                            url = %self.url,
                            tier = %parsed.tier,
                            latency_ms = latency.as_millis(),
                            "ML service classified"
                        );
                        parsed.tier
                    }
                    Ok(parsed) => {
                        tracing::debug!(
                            url = %self.url,
                            tier = %parsed.tier,
                            "ML service returned unknown tier, using heuristic fallback"
                        );
                        self.stats.record_failure();
                        let tier = self.fallback.classify(req).await;
                        self.stats.record_fallback(&tier);
                        tier
                    }
                    Err(e) => {
                        tracing::debug!(
                            url = %self.url,
                            error = %e,
                            "Failed to parse ML service response, using heuristic fallback"
                        );
                        self.stats.record_failure();
                        let tier = self.fallback.classify(req).await;
                        self.stats.record_fallback(&tier);
                        tier
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    url = %self.url,
                    error = %e,
                    "ML service unavailable, using heuristic fallback"
                );
                self.stats.record_failure();
                let tier = self.fallback.classify(req).await;
                self.stats.record_fallback(&tier);
                tier
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boom_core::types::{Message, MessageContent, MessageRole};
    use std::sync::atomic::Ordering;

    /// Helper to extract the error message from a `try_new` Result,
    /// side-stepping the `T: Debug` bound on `unwrap_err` (the struct
    /// intentionally doesn't derive Debug because `reqwest::Client`
    /// doesn't).
    fn unwrap_err(result: Result<MlServiceClient, String>) -> String {
        match result {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(msg) => msg,
        }
    }


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

    fn valid_tiers() -> HashSet<String> {
        HashSet::from(["small".to_string(), "medium".to_string(), "large".to_string()])
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

        let client = MlServiceClient::new(&server.url(), 500, valid_tiers());

        let msgs = make_messages(vec![("user", "hello")]);
        let req = ClassifyRequest {
            messages: &msgs,
            tools: &None,
            default_tier: "medium",
        };
        let tier = client.classify(&req).await;
        assert_eq!(tier, "large");
        assert_eq!(client.stats().successes.load(Ordering::Relaxed), 1);
        assert_eq!(client.stats().failures.load(Ordering::Relaxed), 0);
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

        let client = MlServiceClient::new(&server.url(), 500, valid_tiers());

        let msgs = make_messages(vec![("user", "hi")]);
        let req = ClassifyRequest {
            messages: &msgs,
            tools: &None,
            default_tier: "medium",
        };
        let tier = client.classify(&req).await;
        assert_eq!(tier, "small");
        assert_eq!(client.stats().failures.load(Ordering::Relaxed), 1);
        assert_eq!(client.stats().fallbacks.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn ml_service_falls_back_on_5xx() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/classify")
            .with_status(500)
            .create_async()
            .await;

        let client = MlServiceClient::new(&server.url(), 500, valid_tiers());

        let msgs = make_messages(vec![("user", "hi")]);
        let req = ClassifyRequest {
            messages: &msgs,
            tools: &None,
            default_tier: "medium",
        };
        let tier = client.classify(&req).await;
        assert_eq!(tier, "small");
        assert_eq!(client.stats().failures.load(Ordering::Relaxed), 1);
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

        let client = MlServiceClient::new(&server.url(), 500, valid_tiers());

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

        let client = MlServiceClient::new(&server.url(), 50, valid_tiers());

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
    async fn ml_service_falls_back_on_connection_refused() {
        // Use a port that's almost certainly closed.
        let client = MlServiceClient::new("http://127.0.0.1:1", 100, valid_tiers());

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

        let client = MlServiceClient::new(&server.url(), 500, valid_tiers());

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

    // ── try_new validation tests ────────────────────────────────────

    #[test]
    fn try_new_rejects_zero_timeout() {
        let result = MlServiceClient::try_new("http://127.0.0.1:2345", 0, valid_tiers());
        let err = unwrap_err(result);
        assert!(err.contains("timeout_ms"), "unexpected err: {err}");
    }

    #[test]
    fn try_new_rejects_malformed_url() {
        let result = MlServiceClient::try_new("not a url", 100, valid_tiers());
        let err = unwrap_err(result);
        assert!(err.contains("valid URL"), "unexpected err: {err}");
    }

    #[test]
    fn try_new_rejects_non_http_scheme() {
        // `ftp://...` parses fine but isn't a valid ML service endpoint.
        let result = MlServiceClient::try_new("ftp://127.0.0.1:2345", 100, valid_tiers());
        let err = unwrap_err(result);
        assert!(err.contains("http"), "unexpected err: {err}");
    }

    #[test]
    fn try_new_accepts_valid_http_url() {
        let result = MlServiceClient::try_new("http://127.0.0.1:2345", 100, valid_tiers());
        assert!(result.is_ok());
    }
}
