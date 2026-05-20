use crate::entry::PromptLogEntry;
use futures::Stream;
use futures::StreamExt;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Stream wrapper that transparently passes through items while accumulating
/// response content for prompt logging.
///
/// On `Drop`, it sends a log entry with the accumulated response content
/// to the writer channel.
pub struct PromptLogStream<S, F> {
    inner: S,
    sender: mpsc::UnboundedSender<PromptLogEntry>,
    entry: Option<PromptLogEntry>,
    /// Start time for duration calculation.
    start: Instant,
    /// Count of events that passed through.
    event_count: u64,
    /// Accumulated text content from all SSE events.
    content_buf: String,
    /// Finish reason from the last event that carries one.
    finish_reason: Option<String>,
    /// Extractor closure: receives &S::Item, returns (text, finish_reason).
    extractor: F,
}

// Safety: PromptLogStream does not use pin projection for any field.
// All fields are either Unpin or accessed without pin projection.
impl<S: Unpin, F: Unpin> Unpin for PromptLogStream<S, F> {}

impl<S, F> PromptLogStream<S, F> {
    pub fn new(
        inner: S,
        sender: mpsc::UnboundedSender<PromptLogEntry>,
        entry: PromptLogEntry,
        extractor: F,
    ) -> Self {
        let start = Instant::now();
        Self {
            inner,
            sender,
            entry: Some(entry),
            start,
            event_count: 0,
            content_buf: String::new(),
            finish_reason: None,
            extractor,
        }
    }
}

impl<S, F> Drop for PromptLogStream<S, F> {
    fn drop(&mut self) {
        if let Some(mut entry) = self.entry.take() {
            let duration_ms = self.start.elapsed().as_millis() as u64;

            let response = if self.content_buf.is_empty() {
                serde_json::json!({
                    "stream": true,
                    "event_count": self.event_count,
                })
            } else {
                serde_json::json!({
                    "stream": true,
                    "event_count": self.event_count,
                    "content": self.content_buf,
                    "finish_reason": self.finish_reason,
                })
            };

            entry.set_response(response);
            entry.set_status(200, duration_ms);

            if let Err(e) = self.sender.send(entry) {
                tracing::debug!("Prompt log channel closed on stream drop: {}", e.0.request_id);
            }
        }
    }
}

/// Implement Stream that accumulates content and passes through items.
impl<S, F> Stream for PromptLogStream<S, F>
where
    S: Stream + Unpin,
    F: FnMut(&S::Item) -> (Option<String>, Option<String>) + Unpin,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(item)) => {
                this.event_count += 1;
                let (text, finish) = (this.extractor)(&item);
                if let Some(t) = text {
                    this.content_buf.push_str(&t);
                }
                if finish.is_some() {
                    this.finish_reason = finish;
                }
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
