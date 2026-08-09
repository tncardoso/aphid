//! Server-sent events framing.
//!
//! Chunks arrive from the socket split at arbitrary byte offsets, so the
//! decoder buffers until it sees a blank line and only then hands back a
//! complete event payload.

/// Accumulates response bytes and yields the `data:` payload of each event.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buf: String,
}

impl SseDecoder {
    /// Feed bytes straight off the wire.
    ///
    /// Invalid UTF-8 is replaced rather than rejected: a provider mangling one
    /// character should not abort a turn that is otherwise fine.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
    }

    /// Take the next complete event, if one has arrived.
    ///
    /// Returns the concatenated `data:` lines. Comments and other fields are
    /// ignored, as is any event carrying no data at all.
    pub(crate) fn next_event(&mut self) -> Option<String> {
        loop {
            let (block, rest) = split_event(&self.buf)?;
            let data = collect_data(block);
            self.buf = rest;
            if let Some(data) = data {
                return Some(data);
            }
            // A keep-alive comment or a field-only event: keep looking.
        }
    }

    /// Whatever is left when the body ends, in case a provider omits the final
    /// blank line.
    pub(crate) fn flush(&mut self) -> Option<String> {
        let block = std::mem::take(&mut self.buf);
        collect_data(&block)
    }
}

/// Split off the first event block, returning it and the remaining buffer.
fn split_event(buf: &str) -> Option<(&str, String)> {
    let (end, skip) = match (buf.find("\n\n"), buf.find("\r\n\r\n")) {
        (Some(a), Some(b)) if b < a => (b, 4),
        (Some(a), _) => (a, 2),
        (None, Some(b)) => (b, 4),
        (None, None) => return None,
    };
    Some((&buf[..end], buf[end + skip..].to_owned()))
}

fn collect_data(block: &str) -> Option<String> {
    let mut data: Option<String> = None;
    for line in block.lines() {
        let Some(value) = line.strip_prefix("data:") else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match &mut data {
            Some(acc) => {
                acc.push('\n');
                acc.push_str(value);
            }
            None => data = Some(value.to_owned()),
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yields_one_event_per_blank_line() {
        let mut sse = SseDecoder::default();
        sse.push(b"data: one\n\ndata: two\n\n");
        assert_eq!(sse.next_event().as_deref(), Some("one"));
        assert_eq!(sse.next_event().as_deref(), Some("two"));
        assert_eq!(sse.next_event(), None);
    }

    #[test]
    fn reassembles_events_split_across_chunks() {
        let mut sse = SseDecoder::default();
        sse.push(b"data: {\"cho");
        assert_eq!(sse.next_event(), None);
        sse.push(b"ices\":[]}");
        assert_eq!(sse.next_event(), None, "no blank line yet");
        sse.push(b"\n\n");
        assert_eq!(sse.next_event().as_deref(), Some(r#"{"choices":[]}"#));
    }

    #[test]
    fn handles_crlf_framing() {
        let mut sse = SseDecoder::default();
        sse.push(b"data: one\r\n\r\ndata: two\r\n\r\n");
        assert_eq!(sse.next_event().as_deref(), Some("one"));
        assert_eq!(sse.next_event().as_deref(), Some("two"));
    }

    #[test]
    fn skips_comments_and_keeps_going() {
        let mut sse = SseDecoder::default();
        sse.push(b": keep-alive\n\ndata: real\n\n");
        assert_eq!(sse.next_event().as_deref(), Some("real"));
    }

    #[test]
    fn joins_multi_line_data() {
        let mut sse = SseDecoder::default();
        sse.push(b"event: message\ndata: line one\ndata: line two\n\n");
        assert_eq!(sse.next_event().as_deref(), Some("line one\nline two"));
    }

    #[test]
    fn flush_recovers_a_trailing_event_without_its_blank_line() {
        let mut sse = SseDecoder::default();
        sse.push(b"data: [DONE]");
        assert_eq!(sse.next_event(), None);
        assert_eq!(sse.flush().as_deref(), Some("[DONE]"));
        assert_eq!(sse.flush(), None);
    }
}
