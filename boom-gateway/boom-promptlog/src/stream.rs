use crate::entry::PromptLogEntry;
use futures::Stream;
use futures::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Stream wrapper that accumulates raw SSE chunks and writes one log entry on Drop.
///
/// Unlike the old content-extracting approach, this stores each SSE event's raw
/// JSON in a `chunks` array — no field-level parsing, no content extraction.
pub struct PromptLogStream<S, F> {
    inner: Option<S>,
    sender: mpsc::UnboundedSender<PromptLogEntry>,
    entry: Option<PromptLogEntry>,
    start: Instant,
    event_count: u64,
    /// Accumulated raw SSE chunks (parsed JSON values).
    chunks: Vec<serde_json::Value>,
    /// Extracts raw JSON data string from a stream item.
    raw_data_fn: F,
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
        entry: PromptLogEntry,
        raw_data_fn: F,
        raw_upstream_chunks: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    ) -> Self {
        let start = Instant::now();
        Self {
            inner: Some(inner),
            sender,
            entry: Some(entry),
            start,
            event_count: 0,
            chunks: Vec::new(),
            raw_data_fn,
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

        if let Some(mut entry) = self.entry.take() {
            let duration_ms = self.start.elapsed().as_millis() as u64;

            let response = serde_json::json!({
                "stream": true,
                "event_count": self.event_count,
                "chunks": self.chunks,
            });

            entry.set_response(response);
            entry.set_status(200, duration_ms);

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
                tracing::debug!("Prompt log channel closed on stream drop: {}", e.0.request_id);
            }
        }
    }
}

impl<S, F> Stream for PromptLogStream<S, F>
where
    S: Stream + Unpin,
    F: FnMut(&S::Item) -> Option<String> + Unpin,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        match inner.poll_next_unpin(cx) {
            Poll::Ready(Some(item)) => {
                this.event_count += 1;
                if let Some(raw) = (this.raw_data_fn)(&item) {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
                        this.chunks.push(val);
                    }
                }
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

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
        let entry = PromptLogEntry::new(
            "request-1",
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
            |_item: &serde_json::Value| None::<String>,
            None,
        )
        .with_entry_enricher(Arc::new(move |entry| {
            entry.set_raw_upstream_response(serde_json::json!({
                "inner_dropped": snapshot_dropped.load(Ordering::SeqCst)
            }));
        }));

        drop(stream);

        assert!(dropped.load(Ordering::SeqCst));
        let entry = receiver.try_recv().expect("prompt log entry was not sent");
        assert_eq!(
            entry.raw_upstream_response.unwrap()["inner_dropped"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(entry.status_code, Some(200));
        assert!(entry.error_message.is_none());
    }
}
