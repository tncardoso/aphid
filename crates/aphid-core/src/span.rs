//! Byte ranges into the arenas owned by a [`Transcript`] or [`MessageBuffer`].
//!
//! Spans are how aphid avoids one allocation per content block: every string
//! payload in a conversation lives in a single contiguous buffer, and a block
//! records only where its bytes start and how many there are.
//!
//! [`Transcript`]: crate::Transcript
//! [`MessageBuffer`]: crate::MessageBuffer

use std::fmt;

/// A byte range into a text arena.
///
/// Eight bytes and [`Copy`]. The empty span doubles as "absent" for optional
/// payloads such as signatures, which keeps [`Content`](crate::Content) free of
/// `Option<Span>` and its extra discriminant.
#[derive(Copy, Clone, PartialEq, Eq, Default, Hash)]
pub struct Span {
    start: u32,
    len: u32,
}

impl Span {
    /// The absent / zero-length span.
    pub const EMPTY: Span = Span { start: 0, len: 0 };

    pub(crate) const fn new(start: u32, len: u32) -> Self {
        Self { start, len }
    }

    /// Length of the range in bytes.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.len
    }

    /// Whether the range is empty, which also encodes "no value".
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub(crate) const fn end(self) -> u32 {
        self.start + self.len
    }

    pub(crate) const fn range(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }

    /// Shift a span by `offset`, used when staged content is committed into a
    /// transcript arena that already holds bytes.
    pub(crate) const fn shifted(self, offset: u32) -> Self {
        if self.len == 0 {
            Span::EMPTY
        } else {
            Span {
                start: self.start + offset,
                len: self.len,
            }
        }
    }

    pub(crate) fn grow(&mut self, by: u32) {
        self.len += by;
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            f.write_str("Span::EMPTY")
        } else {
            write!(f, "Span({}..{})", self.start, self.end())
        }
    }
}

/// A byte range into a blob arena, holding binary payloads such as image data.
#[derive(Copy, Clone, PartialEq, Eq, Default, Hash)]
pub struct BlobSpan {
    start: u32,
    len: u32,
}

impl BlobSpan {
    /// The absent / zero-length span.
    pub const EMPTY: BlobSpan = BlobSpan { start: 0, len: 0 };

    pub(crate) const fn new(start: u32, len: u32) -> Self {
        Self { start, len }
    }

    /// Length of the range in bytes.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.len
    }

    /// Whether the range is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub(crate) const fn range(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }

    pub(crate) const fn shifted(self, offset: u32) -> Self {
        if self.len == 0 {
            BlobSpan::EMPTY
        } else {
            BlobSpan {
                start: self.start + offset,
                len: self.len,
            }
        }
    }
}

impl fmt::Debug for BlobSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            f.write_str("BlobSpan::EMPTY")
        } else {
            write!(f, "BlobSpan({}..{})", self.start, self.start + self.len)
        }
    }
}
