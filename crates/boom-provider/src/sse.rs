//! Spec-compliant Server-Sent Events line parser (WHATWG HTML §9.2).
//!
//! Replaces per-provider hand-rolled `buffer.find("\n\n")` splitting, which
//! only understood LF framing and silently hung on CRLF upstreams. This
//! parser implements the framing rules the spec actually defines:
//!
//! - Lines end with `\r\n`, `\r`, or `\n` — any mixture thereof.
//! - An empty line dispatches the pending event.
//! - Field syntax is `field: value`; one optional leading space after the
//!   colon is stripped (`data:x` ≡ `data: x`). Beyond that single space the
//!   value is kept verbatim — callers trim if they want more.
//! - Multiple `data` lines within one event join with `\n` (multi-line
//!   JSON split across data lines is spec-legal and used to be dropped).
//! - `:` prefix = comment line, ignored. `id`/`retry`/unknown fields are
//!   ignored (the gateway has no use for them).
//!
//! Robustness details the old splitters got wrong:
//!
//! - The buffer is **bytes**, not a lossy String: a TCP segment splitting a
//!   multi-byte UTF-8 character no longer corrupts it. Line terminators are
//!   pure ASCII and UTF-8 continuation bytes can never be 0x0A/0x0D, so a
//!   line boundary is always a safe conversion point.
//! - A trailing `\r` at the end of the buffer is **not** consumed until the
//!   next byte arrives — guessing `\r` vs `\r\n` early would fabricate a
//!   line break inside a `\r\n` pair and split one event into two.
//! - A leading BOM is tolerated once at stream start.
//! - `finish()` dispatches a pending event at end-of-stream, per the spec's
//!   "once the end of the file is reached, any pending event must be
//!   dispatched" — upstreams that omit the final blank line still deliver.
//!
//! `SseEvent::raw` preserves the original frame bytes (line endings exactly
//! as sent) for wire-level capture; `take_pending_raw` exposes bytes that
//! never completed an event when the stream dies mid-frame.

/// One dispatched SSE event: the (possibly absent) `event:` field value,
/// the joined `data` payload, and the original frame text.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event_type: String,
    pub data: String,
    /// Original frame text with line endings exactly as received.
    pub raw: String,
}

pub struct SseParser {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
    event_type: Option<String>,
    /// Raw text of the event currently being accumulated (for capture).
    raw_frame: String,
    bom_seen: bool,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            data_lines: Vec::new(),
            event_type: None,
            raw_frame: String::new(),
            bom_seen: false,
        }
    }

    /// Feed raw stream bytes; returns events completed by this input.
    pub fn push(&mut self, mut bytes: &[u8]) -> Vec<SseEvent> {
        if !self.bom_seen {
            self.bom_seen = true;
            if bytes.starts_with(b"\xEF\xBB\xBF") {
                bytes = &bytes[3..];
            }
        }
        self.buffer.extend_from_slice(bytes);

        let mut events = Vec::new();
        while let Some((line, terminator)) = self.next_line() {
            self.raw_frame.push_str(&line);
            self.raw_frame.push_str(terminator);
            self.process_line(&line, &mut events);
        }
        events
    }

    /// End of stream: the final line counts as terminated by EOF, then any
    /// pending event is dispatched (spec end-of-file rule). Returns events
    /// plus any leftover raw bytes for wire capture.
    pub fn finish(&mut self) -> (Vec<SseEvent>, String) {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            self.raw_frame
                .push_str(&String::from_utf8_lossy(&self.buffer));
            // A lone trailing \r was held by next_line awaiting a possible
            // \n; at EOF it *is* the terminator — strip it from the line.
            let mut tail_len = self.buffer.len();
            if self.buffer[tail_len - 1] == b'\r' {
                tail_len -= 1;
            }
            let tail = String::from_utf8_lossy(&self.buffer[..tail_len]).into_owned();
            self.buffer.clear();
            self.process_line(&tail, &mut events);
        }
        if !self.data_lines.is_empty() {
            events.push(SseEvent {
                event_type: self.event_type.take().unwrap_or_default(),
                data: self.data_lines.join("\n"),
                raw: std::mem::take(&mut self.raw_frame),
            });
            self.data_lines.clear();
        }
        self.event_type = None;
        let leftover = self.take_pending_raw();
        (events, leftover)
    }

    /// Raw bytes accumulated for an event that never completed (stream died
    /// mid-frame, or an `event:`-only frame with no data was skipped).
    /// Used by wire capture on abnormal stream end.
    pub fn take_pending_raw(&mut self) -> String {
        if !self.buffer.is_empty() {
            self.raw_frame
                .push_str(&String::from_utf8_lossy(&self.buffer));
            self.buffer.clear();
        }
        std::mem::take(&mut self.raw_frame)
    }

    /// Pop the next complete line and its terminator as received.
    /// Returns None when no complete line is available. A trailing lone `\r`
    /// waits for the next byte — it may be the first half of a `\r\n`.
    fn next_line(&mut self) -> Option<(String, &'static str)> {
        let pos = {
            let bytes = &self.buffer;
            let nl = bytes.iter().position(|&b| b == b'\n');
            let cr = bytes.iter().position(|&b| b == b'\r');
            match (nl, cr) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => return None,
            }
        };
        let terminator: &'static str = if self.buffer[pos] == b'\r' {
            if pos + 1 == self.buffer.len() {
                return None; // lone trailing \r — may pair with a following \n
            }
            if self.buffer[pos + 1] == b'\n' {
                "\r\n"
            } else {
                "\r"
            }
        } else {
            "\n"
        };
        let line = String::from_utf8_lossy(&self.buffer[..pos]).into_owned();
        self.buffer.drain(..pos + terminator.len());
        Some((line, terminator))
    }

    fn process_line(&mut self, line: &str, events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if !self.data_lines.is_empty() {
                events.push(SseEvent {
                    event_type: self.event_type.take().unwrap_or_default(),
                    data: self.data_lines.join("\n"),
                    raw: std::mem::take(&mut self.raw_frame),
                });
                self.data_lines.clear();
            } else {
                // A frame with no data (keep-alive, `event:`-only ping) is
                // not dispatched — drop its raw text too so raw stays 1:1
                // with dispatched events.
                self.raw_frame.clear();
            }
            self.event_type = None;
            return;
        }
        if line.starts_with(':') {
            return; // comment line
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "data" => self.data_lines.push(value.to_string()),
            "event" => self.event_type = Some(value.to_string()),
            _ => {} // id / retry / unknown — not consumed by the gateway
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datas(events: &[SseEvent]) -> Vec<&str> {
        events.iter().map(|e| e.data.as_str()).collect()
    }

    #[test]
    fn lf_crlf_and_cr_line_endings_all_frame() {
        let mut p = SseParser::new();
        let events = p.push(b"data: a\n\ndata: b\r\n\r\ndata: c\r\rdata: d\n\n");
        assert_eq!(datas(&events), ["a", "b", "c", "d"]);
    }

    /// A trailing lone `\r` at stream end waits for bytes that never come;
    /// `finish()` must treat EOF as its terminator.
    #[test]
    fn trailing_lone_cr_at_stream_end_is_closed_by_finish() {
        let mut p = SseParser::new();
        let events = p.push(b"data: c\r\rdata: d\n\ndata: e\r");
        assert_eq!(datas(&events), ["c", "d"], "only the \\r-held event stays pending");
        let (events, _) = p.finish();
        assert_eq!(datas(&events), ["e"]);
    }

    /// The bug this parser exists for: a segment ending in `\r` must NOT
    /// fabricate a line break — `\r\n` split across pushes is one terminator.
    #[test]
    fn crlf_split_across_pushes_is_one_terminator() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"a\":1}\r").is_empty());
        let events = p.push(b"\ndata: {\"b\":2}\r\n\r\n");
        assert_eq!(
            datas(&events),
            ["{\"a\":1}\n{\"b\":2}"],
            "two data lines join into one event"
        );
    }

    #[test]
    fn data_prefix_without_space_is_equivalent() {
        let mut p = SseParser::new();
        let events = p.push(b"data:a\n\ndata:  b\n\n");
        assert_eq!(datas(&events), ["a", " b"], "only one leading space is stripped");
    }

    #[test]
    fn event_field_is_captured() {
        let mut p = SseParser::new();
        let events = p.push(b"event: content_block_delta\ndata: {\"x\":1}\n\n");
        assert_eq!(events[0].event_type, "content_block_delta");
        assert_eq!(events[0].data, "{\"x\":1}");
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored() {
        let mut p = SseParser::new();
        let events = p.push(b": keep-alive\nid: 42\nretry: 3000\ndata: a\n\n");
        assert_eq!(datas(&events), ["a"]);
    }

    #[test]
    fn bom_is_stripped_once() {
        let mut p = SseParser::new();
        let events = p.push("\u{feff}data: a\n\n".as_bytes());
        assert_eq!(datas(&events), ["a"]);
    }

    /// A multi-byte UTF-8 character split across TCP segments must survive —
    /// the byte buffer defers conversion until the line is complete.
    #[test]
    fn utf8_split_across_pushes_is_not_corrupted() {
        let mut p = SseParser::new();
        let full = "data: 你好\n\n".as_bytes().to_vec();
        let split_at = 6 + 1; // inside the 3-byte 你
        assert!(p.push(&full[..split_at]).is_empty());
        let events = p.push(&full[split_at..]);
        assert_eq!(datas(&events), ["你好"]);
    }

    #[test]
    fn finish_dispatches_pending_event_without_final_blank_line() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: tail").is_empty());
        let (events, leftover) = p.finish();
        assert_eq!(datas(&events), ["tail"]);
        assert!(leftover.is_empty());
    }

    #[test]
    fn empty_event_with_no_data_is_not_dispatched() {
        let mut p = SseParser::new();
        let events = p.push(b"event: ping\n\n");
        assert!(events.is_empty());
    }

    #[test]
    fn raw_preserves_original_line_endings() {
        let mut p = SseParser::new();
        let events = p.push(b"data: a\r\n\r\n");
        assert_eq!(events[0].raw, "data: a\r\n\r\n");
    }

    #[test]
    fn take_pending_raw_exposes_incomplete_frame_bytes() {
        let mut p = SseParser::new();
        p.push(b"data: partial\r\n");
        let pending = p.take_pending_raw();
        assert_eq!(pending, "data: partial\r\n");
    }
}
