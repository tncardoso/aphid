//! The contiguous buffers that back every string and blob in a conversation.
//!
//! Both arenas are append-only. The streaming path relies on that: a delta is
//! appended to the tail once, and the open block's [`Span`] simply grows, so a
//! token travels from the socket into its final resting place with a single
//! copy.

use crate::span::{BlobSpan, Span};

/// Arenas address their contents with `u32` offsets, so a single conversation
/// is capped at 4 GiB of text (and the same of binary payloads). Exceeding it
/// panics rather than silently truncating.
pub(crate) const MAX_ARENA_BYTES: usize = u32::MAX as usize;

/// Append-only UTF-8 buffer holding every text payload in a conversation.
#[derive(Debug, Default, Clone)]
pub(crate) struct TextArena {
    buf: String,
}

impl TextArena {
    pub(crate) fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: String::with_capacity(bytes),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.buf
    }

    pub(crate) fn len(&self) -> u32 {
        self.buf.len() as u32
    }

    /// Append `s` and return the span covering it.
    pub(crate) fn push(&mut self, s: &str) -> Span {
        let start = self.len();
        self.reserve(s.len());
        self.buf.push_str(s);
        Span::new(start, s.len() as u32)
    }

    /// Open a zero-length span at the tail, ready to be grown by [`Self::extend`].
    pub(crate) fn open(&mut self) -> Span {
        Span::new(self.len(), 0)
    }

    /// Append `s` to a span that currently ends at the tail of the arena.
    ///
    /// This is the zero-copy streaming path. Callers must guarantee the span is
    /// still at the tail; [`MessageBuffer`](crate::MessageBuffer) relocates a
    /// block first when a provider interleaves content.
    pub(crate) fn extend(&mut self, span: &mut Span, s: &str) {
        debug_assert!(
            self.ends_at_tail(*span),
            "extend() on a span that is not at the arena tail; relocate the block first"
        );
        self.reserve(s.len());
        self.buf.push_str(s);
        span.grow(s.len() as u32);
    }

    pub(crate) fn ends_at_tail(&self, span: Span) -> bool {
        span.end() == self.len()
    }

    pub(crate) fn get(&self, span: Span) -> &str {
        &self.buf[span.range()]
    }

    /// Drop everything from `watermark` onwards. Only valid at a char boundary,
    /// which every span end is.
    pub(crate) fn truncate(&mut self, watermark: u32) {
        self.buf.truncate(watermark as usize);
    }

    fn reserve(&mut self, additional: usize) {
        assert!(
            self.buf.len() + additional <= MAX_ARENA_BYTES,
            "aphid text arena exceeded {MAX_ARENA_BYTES} bytes"
        );
        self.buf.reserve(additional);
    }
}

/// Append-only byte buffer holding binary payloads (image data).
#[derive(Debug, Default, Clone)]
pub(crate) struct BlobArena {
    buf: Vec<u8>,
}

impl BlobArena {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub(crate) fn len(&self) -> u32 {
        self.buf.len() as u32
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> BlobSpan {
        assert!(
            self.buf.len() + bytes.len() <= MAX_ARENA_BYTES,
            "aphid blob arena exceeded {MAX_ARENA_BYTES} bytes"
        );
        let start = self.len();
        self.buf.extend_from_slice(bytes);
        BlobSpan::new(start, bytes.len() as u32)
    }

    pub(crate) fn get(&self, span: BlobSpan) -> &[u8] {
        &self.buf[span.range()]
    }

    pub(crate) fn truncate(&mut self, watermark: u32) {
        self.buf.truncate(watermark as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_returns_the_span_it_wrote() {
        let mut arena = TextArena::default();
        let a = arena.push("hello");
        let b = arena.push(" world");
        assert_eq!(arena.get(a), "hello");
        assert_eq!(arena.get(b), " world");
        assert_eq!(arena.as_str(), "hello world");
    }

    #[test]
    fn extend_grows_a_span_in_place() {
        let mut arena = TextArena::default();
        let mut span = arena.open();
        for delta in ["str", "eam", "ed"] {
            arena.extend(&mut span, delta);
        }
        assert_eq!(arena.get(span), "streamed");
        assert_eq!(arena.len(), 8);
    }

    #[test]
    fn a_span_is_at_the_tail_only_until_something_follows_it() {
        let mut arena = TextArena::default();
        let first = arena.push("a");
        assert!(arena.ends_at_tail(first));
        arena.push("b");
        assert!(!arena.ends_at_tail(first));
    }

    // The guard is a `debug_assert!`, so it only exists in debug builds.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "not at the arena tail")]
    fn extending_a_buried_span_is_caught_in_debug() {
        let mut arena = TextArena::default();
        let mut buried = arena.push("first");
        arena.push("second");
        arena.extend(&mut buried, "!");
    }

    #[test]
    fn truncate_rewinds_to_a_watermark() {
        let mut arena = TextArena::default();
        let keep = arena.push("keep");
        let watermark = arena.len();
        arena.push("discard");
        arena.truncate(watermark);
        assert_eq!(arena.as_str(), "keep");
        assert_eq!(arena.get(keep), "keep");
    }

    #[test]
    fn blob_arena_round_trips_bytes() {
        let mut arena = BlobArena::default();
        let a = arena.push(&[1, 2, 3]);
        let b = arena.push(&[4]);
        assert_eq!(arena.get(a), &[1, 2, 3]);
        assert_eq!(arena.get(b), &[4]);
    }
}
