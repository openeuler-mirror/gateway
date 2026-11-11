use crate::entry::{error_code, PromptLogEntry};
use futures::Stream;
use futures::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// What the stream wrapper accumulated when it ended. `Finished` means the
/// upstream sent a terminal chunk with `finish_reason`; `Truncated` means the
/// stream ended without one (timeout / client disconnect); `Failed` means the
/// producer surfaced an in-stream failure sentinel (Responses
/// `response.failed` — mid-stream upstream error) — logged as 5xx with the
/// sentinel's error code, never the 499 CLIENT_DISCONNECTED mislabel.
enum StreamEnd {
    Finished,
    Truncated,
    Failed,
}

/// Per-chunk delta extracted by a producer-supplied closure. The closure is
/// provider-specific (OpenAI: `choices[0].delta.content` /
/// `choices[0].finish_reason` / `usage`; Anthropic: `content_block_delta.delta.text`
/// etc.) — the promptlog crate stays provider-agnostic by delegating.
#[derive(Default, Debug, Clone)]
pub struct ChunkDelta {
    pub content: Option<String>,
    pub finish_reason: Option<String>,
    /// `usage` object from the final chunk (OpenAI streams it on the last
    /// data event; Anthropic sends a separate `message_delta` with `usage`).
    pub usage: Option<serde_json::Value>,
    /// In-stream failure sentinel (e.g. Responses `response.failed` event
    /// payload's `error` object). When set, the stream ended in an upstream
    /// FAILURE — the wrapper logs 5xx + the error code on Drop, regardless
    /// of any terminal marker seen before it.
    pub error: Option<serde_json::Value>,
}

/// Stream wrapper that accumulates streaming chunks into a single assembled
/// response body, written as one Response-phase log entry on Drop.
///
/// The wrapper is provider-agnostic: a `raw_data_fn` closure extracts a
/// `ChunkDelta` from each upstream chunk (the closure knows the provider's
/// SSE JSON shape; this crate doesn't). On Drop, the accumulated content is
/// packed into an OpenAI ChatCompletion-style JSON object and handed to the
/// background writer along with the duration, status, and (if the stream was
/// truncated) an `error_code`.
///
/// If the client disconnects mid-stream, Drop is still invoked — we keep what
/// we have, mark the entry with `CLIENT_DISCONNECTED`, and write a 499 status.
pub struct PromptLogStream<S, F> {
    inner: Option<S>,
    /// Prompt-log channel. `None` in trace-only mode — the wrapper then
    /// exists solely to assemble the streamed response for `response_sink`
    /// (the trace span's `llm_response`), no entry ever ships.
    sender: Option<crate::writer::PromptLogSender>,
    /// The Request-phase entry, kept here so `new_response_from` can clone its
    /// shared identity fields when we emit the Response-phase entry on Drop.
    /// `None` in trace-only mode.
    request_entry: Option<PromptLogEntry>,
    /// Standalone copies of the identity fields the assembled response needs
    /// — with the prompt log off there is no request_entry to borrow them
    /// from.
    request_id: String,
    model: String,
    start: Instant,
    /// Concatenated content deltas.
    accumulated_content: String,
    /// `finish_reason` from the terminal chunk, if we got one.
    finish_reason: Option<String>,
    /// `usage` object from the terminal chunk, if any.
    usage: Option<serde_json::Value>,
    /// Whether we saw a terminal chunk (finish_reason set or stream ended
    /// cleanly). Determines `error_code` on Drop.
    end_state: Option<StreamEnd>,
    /// The failure sentinel payload when `end_state` is `Failed`.
    stream_error: Option<serde_json::Value>,
    /// Extracts ChunkDelta from a stream item. Returns `None` for non-data
    /// events (e.g. comment lines in Anthropic SSE).
    delta_fn: F,
    /// Adds provider-owned metadata after the wrapped stream has been dropped.
    entry_enricher: Option<Arc<dyn Fn(&mut PromptLogEntry) + Send + Sync>>,
    /// Observer for the assembled response, called at Drop BEFORE the inner
    /// stream is dropped. boom-main attaches a closure that stamps the
    /// response on the still-active trace span (`llm_response`) — the inner
    /// `LoggedStream`'s Drop finalizes the span, so anything after it would
    /// land on a dead span. Sharing the Arc keeps stream paths zero-copy
    /// with the prompt log entry.
    response_sink: Option<Arc<dyn Fn(&Arc<serde_json::Value>) + Send + Sync>>,
}

impl<S: Unpin, F: Unpin> Unpin for PromptLogStream<S, F> {}

impl<S, F> PromptLogStream<S, F> {
    pub fn new(
        inner: S,
        sender: crate::writer::PromptLogSender,
        request_entry: PromptLogEntry,
        delta_fn: F,
    ) -> Self {
        let start = Instant::now();
        Self {
            request_id: request_entry.request_id.clone(),
            model: request_entry.model.clone(),
            inner: Some(inner),
            sender: Some(sender),
            request_entry: Some(request_entry),
            start,
            accumulated_content: String::new(),
            finish_reason: None,
            usage: None,
            end_state: None,
            stream_error: None,
            delta_fn,
            entry_enricher: None,
            response_sink: None,
        }
    }

    /// Trace-only construction: the prompt log is off (or the key excluded)
    /// but trace body capture is on, so the wrapper runs purely to assemble
    /// the streamed response and hand it to `with_response_sink`. No entry
    /// is ever sent.
    pub fn new_trace_only(
        inner: S,
        request_id: String,
        model: String,
        delta_fn: F,
    ) -> Self {
        let start = Instant::now();
        Self {
            inner: Some(inner),
            sender: None,
            request_entry: None,
            request_id,
            model,
            start,
            accumulated_content: String::new(),
            finish_reason: None,
            usage: None,
            end_state: None,
            stream_error: None,
            delta_fn,
            entry_enricher: None,
            response_sink: None,
        }
    }

    pub fn with_entry_enricher(
        mut self,
        enricher: Arc<dyn Fn(&mut PromptLogEntry) + Send + Sync>,
    ) -> Self {
        self.entry_enricher = Some(enricher);
        self
    }

    /// Attach a response observer (see `response_sink`). Builder form to
    /// sit next to `with_entry_enricher`.
    pub fn with_response_sink(
        mut self,
        sink: Arc<dyn Fn(&Arc<serde_json::Value>) + Send + Sync>,
    ) -> Self {
        self.response_sink = Some(sink);
        self
    }
}

impl<S, F> Drop for PromptLogStream<S, F> {
    fn drop(&mut self) {
        let req_entry = self.request_entry.take();

        // Build a ChatCompletion-style assembled response. This isn't a chunk
        // array — it's the message a client would have received if it had
        // buffered the whole stream. Lets a human reading the JSONL see the
        // final assistant text without replaying chunks. Wrap in `Arc` so
        // the same allocation can be shared with `boom_trace::RequestSpan`'s
        // `boom-gateway.llm_response` attribute (single-source-of-truth).
        //
        // ORDERING: assembled and sunk BEFORE the inner stream drop. The
        // inner `LoggedStream`'s Drop finalizes the trace span (guard Drop →
        // registry finalize → span leaves the active map), so the response
        // sink must stamp it while the span is still live — this is what
        // wires `llm_response` for streaming requests.
        let mut response = serde_json::json!({
            "id": self.request_id,
            "object": "chat.completion",
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": self.accumulated_content.clone(),
                },
                "finish_reason": self.finish_reason.clone(),
            }],
            "usage": self.usage.clone(),
        });
        if let Some(err) = &self.stream_error {
            // The failure sentinel rides on the assembled response so the
            // provider's original code/message stay diagnosable even though
            // error_code is normalized for statistics.
            response["error"] = err.clone();
        }
        let response = Arc::new(response);
        if let Some(sink) = self.response_sink.take() {
            sink(&response);
        }

        // Nested provider streams finalize their trace state on drop. Runs
        // AFTER the response sink (span stamping) and BEFORE the enricher
        // (fusion snapshot needs finalization side effects).
        self.inner.take();

        // Trace-only wrappers have no entry to ship.
        let (Some(req_entry), Some(sender)) = (req_entry, self.sender.take()) else {
            return;
        };
        let duration_ms = self.start.elapsed().as_millis() as u64;

        let mut entry = PromptLogEntry::new_response_from(&req_entry);
        entry.set_response(response);

        let (status, error_code_opt) = match &self.end_state {
            Some(StreamEnd::Finished) => (200, None),
            // Mid-stream upstream failure surfaced in-stream (Responses
            // response.failed; other providers may use the sentinel too):
            // 5xx + the sentinel's error code — same vocabulary as the
            // producer-side error path, never 499 CLIENT_DISCONNECTED and
            // never a bogus 200.
            Some(StreamEnd::Failed) => {
                // Same (status, error_code) rules as the non-stream error
                // path (see error_code::from_stream_sentinel): timeout →
                // 504/TIMEOUT, everything else merged → 502/UPSTREAM_ERROR.
                let code = self
                    .stream_error
                    .as_ref()
                    .and_then(|e| e.get("code"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                let (status, normalized) = error_code::from_stream_sentinel(code);
                (status, Some(normalized))
            }
            // Truncated = upstream ended without a terminal chunk OR client
            // disconnected. We can't reliably tell which from inside Drop, so
            // we surface CLIENT_DISCONNECTED as the conservative read — the
            // producer can override via set_status / set_error after Drop if
            // it has better info (e.g. an upstream error sentinel).
            Some(StreamEnd::Truncated) | None => (
                499,
                Some(error_code::CLIENT_DISCONNECTED.to_string()),
            ),
        };
        entry.set_status(status, duration_ms);
        if let Some(code) = error_code_opt {
            entry.error_code = Some(code);
        }

        // Let the producer inject extra fields (e.g. fusion trace snapshot,
        // raw upstream exchange captured via the provider side channel)
        // before the entry ships. Runs AFTER inner stream drop so the trace
        // can observe finalization side effects.
        if let Some(enricher) = &self.entry_enricher {
            enricher(&mut entry);
        }

        // Full is counted + warned inside the counting sender; only the
        // closed case is site-specific.
        if let Err(mpsc::error::TrySendError::Closed(e)) = sender.try_send(entry) {
            tracing::debug!(
                "Prompt log channel closed on stream drop: {}",
                e.request_id
            );
        }
    }
}

impl<S, F> Stream for PromptLogStream<S, F>
where
    S: Stream + Unpin,
    F: FnMut(&S::Item) -> Option<ChunkDelta> + Unpin,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        match inner.poll_next_unpin(cx) {
            Poll::Ready(Some(item)) => {
                if let Some(delta) = (this.delta_fn)(&item) {
                    if let Some(content) = delta.content {
                        this.accumulated_content.push_str(&content);
                    }
                    if let Some(reason) = delta.finish_reason {
                        // First terminal marker wins (OpenAI sends one
                        // chunk with finish_reason + usage together).
                        if this.finish_reason.is_none() {
                            this.finish_reason = Some(reason);
                            this.end_state = Some(StreamEnd::Finished);
                        }
                    }
                    if let Some(usage) = delta.usage {
                        this.usage = Some(usage);
                    }
                    if let Some(err) = delta.error {
                        if this.stream_error.is_none() {
                            this.stream_error = Some(err);
                            // A failure sentinel overrides any earlier terminal
                            // marker — the stream is dead either way.
                            this.end_state = Some(StreamEnd::Failed);
                        }
                    }
                }
                Poll::Ready(Some(item))
            }
            // Upstream ended the stream. If we already saw finish_reason, we
            // marked Finished above; otherwise this is a truncation.
            Poll::Ready(None) => {
                if this.end_state.is_none() {
                    this.end_state = Some(StreamEnd::Truncated);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::LogPhase;
    use futures::stream;
    use futures::StreamExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn make_request_entry(id: &str) -> PromptLogEntry {
        PromptLogEntry::new_request(
            id,
            None,
            "kh",
            None,
            None,
            "gpt-4",
            "/v1/chat/completions",
            true,
            Arc::new(serde_json::json!({})),
            None,
            None,
        )
    }

    /// Fake upstream chunk carrying the bits a ChunkDelta knows how to extract.
    #[derive(Clone)]
    struct FakeChunk {
        content: Option<String>,
        finish_reason: Option<String>,
        usage: Option<serde_json::Value>,
        error: Option<serde_json::Value>,
    }

    fn extractor(c: &FakeChunk) -> Option<ChunkDelta> {
        Some(ChunkDelta {
            content: c.content.clone(),
            finish_reason: c.finish_reason.clone(),
            usage: c.usage.clone(),
            error: c.error.clone(),
        })
    }

    fn counting_sender(
        sender: mpsc::Sender<PromptLogEntry>,
    ) -> crate::writer::PromptLogSender {
        crate::writer::PromptLogSender::new(
            sender,
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        )
    }

    #[tokio::test]
    async fn finished_stream_writes_assembled_response_with_200() {
        let (tx, mut rx) = mpsc::channel(64);
        let req = make_request_entry("req-1");
        let chunks = vec![
            FakeChunk { content: Some("Hello".to_string()), finish_reason: None, usage: None, error: None },
            FakeChunk { content: Some(", world".to_string()), finish_reason: None, usage: None, error: None },
            FakeChunk {
                content: None,
                finish_reason: Some("stop".to_string()),
                usage: Some(serde_json::json!({"prompt_tokens": 5, "completion_tokens": 3})),
                error: None,
            },
        ];
        let s = stream::iter(chunks);
        let mut wrapper = PromptLogStream::new(s, counting_sender(tx), req, extractor);
        // Drive the stream to completion so the wrapper sees the terminal chunk.
        while let Some(_) = wrapper.next().await {}
        drop(wrapper);
        let entry = rx.recv().await.expect("entry sent");
        assert_eq!(entry.phase, LogPhase::Response);
        assert_eq!(entry.status_code, Some(200));
        assert!(entry.error_code.is_none());
        let resp = entry.response.expect("response set");
        assert_eq!(resp["choices"][0]["message"]["content"], "Hello, world");
        assert_eq!(resp["choices"][0]["finish_reason"], "stop");
        assert_eq!(resp["usage"]["prompt_tokens"], 5);
    }

    #[tokio::test]
    async fn truncated_stream_records_client_disconnected_and_keeps_partial_content() {
        let (tx, mut rx) = mpsc::channel(64);
        let req = make_request_entry("req-2");
        let chunks = vec![
            FakeChunk { content: Some("partial".to_string()), finish_reason: None, usage: None, error: None },
            // No terminal chunk — stream just ends.
        ];
        let s = stream::iter(chunks);
        let mut wrapper = PromptLogStream::new(s, counting_sender(tx), req, extractor);
        // Drive the stream to completion — but it ends without a finish_reason
        // marker, so Drop should record a truncation.
        while let Some(_) = wrapper.next().await {}
        drop(wrapper);
        let entry = rx.recv().await.expect("entry sent");
        assert_eq!(entry.phase, LogPhase::Response);
        assert_eq!(entry.status_code, Some(499));
        assert_eq!(entry.error_code.as_deref(), Some(error_code::CLIENT_DISCONNECTED));
        let resp = entry.response.expect("response set");
        assert_eq!(resp["choices"][0]["message"]["content"], "partial");
        assert!(resp["choices"][0]["finish_reason"].is_null());
    }

    #[tokio::test]
    async fn in_stream_failure_sentinel_logs_upstream_error_not_499() {
        // Mid-stream upstream failure (Responses response.failed): the
        // sentinel must win over both truncation (499 mislabel) and any
        // terminal marker (bogus 200) — 502 + the sentinel's code.
        let (tx, mut rx) = mpsc::channel(64);
        let req = make_request_entry("req-fail");
        let chunks = vec![
            FakeChunk { content: Some("partial ".to_string()), finish_reason: None, usage: None, error: None },
            FakeChunk { content: Some("answer".to_string()), finish_reason: None, usage: None, error: None },
            FakeChunk {
                content: None,
                finish_reason: None,
                usage: None,
                error: Some(serde_json::json!({ "code": "upstream_error", "message": "connection reset" })),
            },
        ];
        let s = stream::iter(chunks);
        let mut wrapper = PromptLogStream::new(s, counting_sender(tx), req, extractor);
        while let Some(_) = wrapper.next().await {}
        drop(wrapper);
        let entry = rx.recv().await.expect("entry sent");
        assert_eq!(entry.phase, LogPhase::Response);
        assert_eq!(entry.status_code, Some(502));
        assert_eq!(entry.error_code.as_deref(), Some("UPSTREAM_ERROR"));
        let resp = entry.response.expect("response set");
        assert_eq!(resp["choices"][0]["message"]["content"], "partial answer");
    }

    #[tokio::test]
    async fn failure_sentinel_overrides_earlier_finish_reason() {
        // The sentinel arrives AFTER a terminal marker (producers may emit
        // both when a stream degrades late) — failure wins.
        let (tx, mut rx) = mpsc::channel(64);
        let req = make_request_entry("req-fail2");
        let chunks = vec![
            FakeChunk { content: Some("x".to_string()), finish_reason: Some("stop".to_string()), usage: None, error: None },
            FakeChunk { content: None, finish_reason: None, usage: None,
                error: Some(serde_json::json!({ "code": "timeout" })) },
        ];
        let s = stream::iter(chunks);
        let mut wrapper = PromptLogStream::new(s, counting_sender(tx), req, extractor);
        while let Some(_) = wrapper.next().await {}
        drop(wrapper);
        let entry = rx.recv().await.expect("entry sent");
        assert_eq!(entry.status_code, Some(504));
        assert_eq!(entry.error_code.as_deref(), Some("TIMEOUT"));
    }

    #[tokio::test]
    async fn provider_reported_codes_merge_to_upstream_error() {
        // Non-stream rules merge UpstreamError/ProviderError/UpstreamParseError
        // into UPSTREAM_ERROR — the stream sentinel must not split that class
        // (nor admit provider vocab like server_error/overloaded_error).
        for code in ["provider_error", "upstream_parse_error", "server_error", "overloaded_error"] {
            let (tx, mut rx) = mpsc::channel(64);
            let req = make_request_entry("req-merge");
            let chunks = vec![FakeChunk {
                content: None,
                finish_reason: None,
                usage: None,
                error: Some(serde_json::json!({ "code": code, "message": "boom" })),
            }];
            let s = stream::iter(chunks);
            let mut wrapper = PromptLogStream::new(s, counting_sender(tx), req, extractor);
            while let Some(_) = wrapper.next().await {}
            drop(wrapper);
            let entry = rx.recv().await.expect("entry sent");
            assert_eq!(entry.status_code, Some(502), "code {code}");
            assert_eq!(entry.error_code.as_deref(), Some("UPSTREAM_ERROR"), "code {code}");
            let resp = entry.response.expect("response set");
            assert_eq!(resp["error"]["code"], code, "original code preserved on the response");
        }
    }

    #[tokio::test]
    async fn dropped_without_draining_records_truncation() {
        // A wrapper that's dropped before any chunks are consumed should be
        // treated as truncated (we never saw a terminal marker).
        let (tx, mut rx) = mpsc::channel(64);
        let req = make_request_entry("req-3");
        let chunks = vec![FakeChunk {
            content: Some("partial".to_string()),
            finish_reason: None,
            usage: None,
            error: None,
        }];
        let s = stream::iter(chunks);
        let _wrapper = PromptLogStream::new(s, counting_sender(tx), req, extractor);
        drop(_wrapper);
        let entry = rx.recv().await.expect("entry sent");
        assert_eq!(entry.phase, LogPhase::Response);
        assert_eq!(entry.status_code, Some(499));
        assert_eq!(entry.error_code.as_deref(), Some(error_code::CLIENT_DISCONNECTED));
        // Content not accumulated because we never polled.
        let resp = entry.response.expect("response set");
        assert_eq!(resp["choices"][0]["message"]["content"], "");
    }

    /// Stream that never produces an item but signals on Drop. Used to verify
    /// the inner stream's Drop runs BEFORE the entry_enricher callback — the
    /// fusion trace snapshot must observe finalization side effects.
    struct DropSignalStream {
        dropped: Arc<AtomicBool>,
    }

    impl Stream for DropSignalStream {
        type Item = serde_json::Value;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Drop for DropSignalStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn entry_enricher_runs_after_inner_stream_drop() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (sender, mut receiver) = mpsc::channel(64);
        let entry = PromptLogEntry::new_request(
            "request-1",
            None,
            "key-1",
            None,
            None,
            "test-model",
            "/v1/chat/completions",
            true,
            Arc::new(serde_json::json!({"model": "test-model"})),
            None,
            None,
        );
        let snapshot_dropped = dropped.clone();
        let stream = PromptLogStream::new(
            DropSignalStream {
                dropped: dropped.clone(),
            },
            counting_sender(sender),
            entry,
            |_item: &serde_json::Value| None::<ChunkDelta>,
        )
        .with_entry_enricher(Arc::new(move |entry| {
            entry.set_raw_upstream_response(Arc::new(serde_json::json!({
                "inner_dropped": snapshot_dropped.load(Ordering::SeqCst)
            })));
        }));

        drop(stream);

        // Inner stream's Drop ran first — enricher observed the side effect.
        assert!(dropped.load(Ordering::SeqCst));
        let entry = receiver.try_recv().expect("prompt log entry was not sent");
        assert_eq!(
            entry.raw_upstream_response.unwrap()["inner_dropped"],
            serde_json::Value::Bool(true)
        );
        // The wrapper was dropped without ever being polled, so it's recorded
        // as a truncation (CLIENT_DISCONNECTED), not a clean 200. This is the
        // otlp-refactor behavior: no terminal chunk seen ⇒ 499.
        assert_eq!(entry.status_code, Some(499));
        assert_eq!(entry.error_code.as_deref(), Some(error_code::CLIENT_DISCONNECTED));
        assert!(entry.error_message.is_none());
    }

    #[test]
    fn response_sink_runs_before_inner_stream_drop() {
        // The sink must observe the span still ACTIVE — i.e. run before the
        // inner stream's Drop finalizes the trace. DropSignalStream records
        // its own drop; the sink asserts the flag is still false at call
        // time and captures the response Arc identity for equality checks.
        let dropped = Arc::new(AtomicBool::new(false));
        let (sender, mut receiver) = mpsc::channel(64);
        let entry = make_request_entry("req-sink");
        let sink_inner_dropped = Arc::new(AtomicBool::new(true));
        let observed_inner_dropped = sink_inner_dropped.clone();
        let sink_saw_response = Arc::new(AtomicBool::new(false));
        let observed_response = sink_saw_response.clone();
        let sink_dropped_flag = dropped.clone();

        let stream = PromptLogStream::new(
            DropSignalStream {
                dropped: dropped.clone(),
            },
            counting_sender(sender),
            entry,
            |_item: &serde_json::Value| None::<ChunkDelta>,
        )
        .with_response_sink(Arc::new(move |resp: &Arc<serde_json::Value>| {
            observed_inner_dropped.store(sink_dropped_flag.load(Ordering::SeqCst), Ordering::SeqCst);
            // Sink sees the assembled response object (content is empty —
            // the wrapper was never polled).
            observed_response.store(resp.get("choices").is_some(), Ordering::SeqCst);
        }));

        drop(stream);

        // Inner drop did run (eventually), and the sink saw it not-yet-dropped.
        assert!(dropped.load(Ordering::SeqCst));
        assert!(
            !sink_inner_dropped.load(Ordering::SeqCst),
            "sink must run before inner stream drop"
        );
        assert!(sink_saw_response.load(Ordering::SeqCst));
        // The prompt log entry carries the same assembled response.
        let entry = receiver.try_recv().expect("prompt log entry was not sent");
        let resp = entry.response.expect("response set");
        assert!(resp["choices"][0]["message"]["content"].is_string());
    }

    #[test]
    fn trace_only_wrapper_runs_sink_and_ships_nothing() {
        // Trace-only construction (prompt log off, trace capture on): Drop
        // must still assemble the response and run the sink — that is the
        // whole reason the wrapper exists in this mode — while shipping no
        // prompt log entry anywhere.
        let dropped = Arc::new(AtomicBool::new(false));
        let sink_ran = Arc::new(AtomicBool::new(false));
        let sink_ran2 = sink_ran.clone();
        let stream = PromptLogStream::new_trace_only(
            DropSignalStream {
                dropped: dropped.clone(),
            },
            "req-trace-only".to_string(),
            "gpt-4".to_string(),
            |_item: &serde_json::Value| None::<ChunkDelta>,
        )
        .with_response_sink(Arc::new(move |resp: &Arc<serde_json::Value>| {
            assert_eq!(resp["id"], "req-trace-only");
            assert_eq!(resp["model"], "gpt-4");
            assert!(resp.get("choices").is_some());
            sink_ran2.store(true, Ordering::SeqCst);
        }));

        drop(stream);

        assert!(dropped.load(Ordering::SeqCst), "inner stream still drops");
        assert!(sink_ran.load(Ordering::SeqCst), "sink must run");
    }
}
