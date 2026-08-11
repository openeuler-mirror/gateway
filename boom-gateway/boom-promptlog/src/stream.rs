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
/// stream ended without one (timeout / client disconnect / upstream error).
enum StreamEnd {
    Finished,
    Truncated,
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
    sender: mpsc::UnboundedSender<PromptLogEntry>,
    /// The Request-phase entry, kept here so `new_response_from` can clone its
    /// shared identity fields when we emit the Response-phase entry on Drop.
    request_entry: Option<PromptLogEntry>,
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
    /// Extracts ChunkDelta from a stream item. Returns `None` for non-data
    /// events (e.g. comment lines in Anthropic SSE).
    delta_fn: F,
    /// Shared buffer for raw upstream SSE chunks (before format conversion).
    /// Only set when `capture_raw_upstream` is enabled for Anthropic-format endpoints.
    raw_upstream_chunks: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// Adds provider-owned metadata after the wrapped stream has been dropped.
    entry_enricher: Option<Arc<dyn Fn(&mut PromptLogEntry) + Send + Sync>>,
}

impl<S: Unpin, F: Unpin> Unpin for PromptLogStream<S, F> {}

impl<S, F> PromptLogStream<S, F> {
    pub fn new(
        inner: S,
        sender: mpsc::UnboundedSender<PromptLogEntry>,
        request_entry: PromptLogEntry,
        delta_fn: F,
        raw_upstream_chunks: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    ) -> Self {
        let start = Instant::now();
        Self {
            inner: Some(inner),
            sender,
            request_entry: Some(request_entry),
            start,
            accumulated_content: String::new(),
            finish_reason: None,
            usage: None,
            end_state: None,
            delta_fn,
            raw_upstream_chunks,
            entry_enricher: None,
        }
    }

    pub fn with_entry_enricher(
        mut self,
        enricher: Arc<dyn Fn(&mut PromptLogEntry) + Send + Sync>,
    ) -> Self {
        self.entry_enricher = Some(enricher);
        self
    }
}

impl<S, F> Drop for PromptLogStream<S, F> {
    fn drop(&mut self) {
        // Nested provider streams finalize their trace state on drop.
        self.inner.take();

        let Some(req_entry) = self.request_entry.take() else {
            return;
        };
        let duration_ms = self.start.elapsed().as_millis() as u64;

        // Build a ChatCompletion-style assembled response. This isn't a chunk
        // array — it's the message a client would have received if it had
        // buffered the whole stream. Lets a human reading the JSONL see the
        // final assistant text without replaying chunks.
        let response = serde_json::json!({
            "id": req_entry.request_id,
            "object": "chat.completion",
            "model": req_entry.model,
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

        let mut entry = PromptLogEntry::new_response_from(&req_entry);
        entry.set_response(response);

        let (status, error_code_opt) = match &self.end_state {
            Some(StreamEnd::Finished) => (200, None),
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

        // Let the producer inject extra fields (e.g. fusion trace snapshot)
        // before the entry ships. Runs AFTER inner stream drop so the trace
        // can observe finalization side effects.
        if let Some(enricher) = &self.entry_enricher {
            enricher(&mut entry);
        }

        // Capture raw upstream chunks if available (before format conversion).
        if let Some(ref raw_chunks) = self.raw_upstream_chunks {
            if let Ok(guard) = raw_chunks.lock() {
                if !guard.is_empty() {
                    let raw_values: Vec<serde_json::Value> = guard
                        .iter()
                        .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                        .collect();
                    entry.set_raw_upstream_response(serde_json::json!({
                        "stream": true,
                        "raw_chunk_count": raw_values.len(),
                        "raw_chunks": raw_values,
                    }));
                }
            }
        }

        if let Err(e) = self.sender.send(entry) {
            tracing::debug!(
                "Prompt log channel closed on stream drop: {}",
                e.0.request_id
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
            serde_json::json!({}),
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
    }

    fn extractor(c: &FakeChunk) -> Option<ChunkDelta> {
        Some(ChunkDelta {
            content: c.content.clone(),
            finish_reason: c.finish_reason.clone(),
            usage: c.usage.clone(),
        })
    }

    #[tokio::test]
    async fn finished_stream_writes_assembled_response_with_200() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = make_request_entry("req-1");
        let chunks = vec![
            FakeChunk { content: Some("Hello".to_string()), finish_reason: None, usage: None },
            FakeChunk { content: Some(", world".to_string()), finish_reason: None, usage: None },
            FakeChunk {
                content: None,
                finish_reason: Some("stop".to_string()),
                usage: Some(serde_json::json!({"prompt_tokens": 5, "completion_tokens": 3})),
            },
        ];
        let s = stream::iter(chunks);
        let mut wrapper = PromptLogStream::new(s, tx, req, extractor, None);
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
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = make_request_entry("req-2");
        let chunks = vec![
            FakeChunk { content: Some("partial".to_string()), finish_reason: None, usage: None },
            // No terminal chunk — stream just ends.
        ];
        let s = stream::iter(chunks);
        let mut wrapper = PromptLogStream::new(s, tx, req, extractor, None);
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
    async fn dropped_without_draining_records_truncation() {
        // A wrapper that's dropped before any chunks are consumed should be
        // treated as truncated (we never saw a terminal marker).
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = make_request_entry("req-3");
        let chunks = vec![FakeChunk {
            content: Some("partial".to_string()),
            finish_reason: None,
            usage: None,
        }];
        let s = stream::iter(chunks);
        let _wrapper = PromptLogStream::new(s, tx, req, extractor, None);
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
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let entry = PromptLogEntry::new_request(
            "request-1",
            None,
            "key-1",
            None,
            None,
            "test-model",
            "/v1/chat/completions",
            true,
            serde_json::json!({"model": "test-model"}),
            None,
            None,
        );
        let snapshot_dropped = dropped.clone();
        let stream = PromptLogStream::new(
            DropSignalStream {
                dropped: dropped.clone(),
            },
            sender,
            entry,
            |_item: &serde_json::Value| None::<ChunkDelta>,
            None,
        )
        .with_entry_enricher(Arc::new(move |entry| {
            entry.set_raw_upstream_response(serde_json::json!({
                "inner_dropped": snapshot_dropped.load(Ordering::SeqCst)
            }));
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
}
