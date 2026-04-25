use crate::entry::PromptLogEntry;
use futures::Stream;
use futures::StreamExt;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Stream wrapper that transparently passes through items while tracking
/// stream completion for prompt logging.
///
/// On `Drop`, it sends a log entry with stream metadata to the writer channel.
/// The actual SSE event content extraction is handled separately in routes.rs
/// where the raw JSON data is accessible.
pub struct PromptLogStream<S> {
    inner: S,
    sender: mpsc::UnboundedSender<PromptLogEntry>,
    entry: Option<PromptLogEntry>,
    /// Start time for duration calculation.
    start: Instant,
    /// Count of events that passed through.
    event_count: u64,
    /// Pre-captured stream response content (set from routes.rs).
    stream_response: Option<serde_json::Value>,
}

impl<S> PromptLogStream<S> {
    pub fn new(
        inner: S,
        sender: mpsc::UnboundedSender<PromptLogEntry>,
        entry: PromptLogEntry,
    ) -> Self {
        let start = Instant::now();
        Self {
            inner,
            sender,
            entry: Some(entry),
            start,
            event_count: 0,
            stream_response: None,
        }
    }

    /// Set the response data for this stream (called from routes.rs
    /// when stream content has been accumulated).
    pub fn set_stream_response(&mut self, response: serde_json::Value) {
        self.stream_response = Some(response);
    }
}

impl<S> Drop for PromptLogStream<S> {
    fn drop(&mut self) {
        if let Some(mut entry) = self.entry.take() {
            let duration_ms = self.start.elapsed().as_millis() as u64;

            let response = self.stream_response.take().unwrap_or_else(|| {
                serde_json::json!({
                    "stream": true,
                    "event_count": self.event_count,
                })
            });

            entry.set_response(response);
            entry.set_status(200, duration_ms);

            if let Err(e) = self.sender.send(entry) {
                tracing::debug!("Prompt log channel closed on stream drop: {}", e.0.request_id);
            }
        }
    }
}

/// Implement Stream that transparently passes through items.
impl<S: Stream + Unpin> Stream for PromptLogStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(item)) => {
                self.event_count += 1;
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
