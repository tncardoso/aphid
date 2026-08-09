//! Typed indices and the crate-wide timestamp type.

use std::fmt;

/// The single datetime type used across aphid.
///
/// [`Copy`] and [`Ord`], so it is free to store inline in a message header and
/// messages sort chronologically without a projection. Provider decoders turn
/// epoch-millisecond wire values into one of these.
pub type Timestamp = chrono::DateTime<chrono::Utc>;

/// Handle to a message inside a [`Transcript`](crate::Transcript).
///
/// In debug builds the id carries the transcript's generation, so reusing an id
/// against a compacted or different transcript panics instead of quietly
/// resolving to the wrong message. The tag is compiled out in release.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct MessageId {
    index: u32,
    #[cfg(debug_assertions)]
    generation: u32,
}

impl MessageId {
    #[cfg(debug_assertions)]
    pub(crate) const fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    #[cfg(not(debug_assertions))]
    pub(crate) const fn new(index: u32, _generation: u32) -> Self {
        Self { index }
    }

    /// Position of the message in its transcript.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    #[cfg(debug_assertions)]
    pub(crate) const fn generation(self) -> u32 {
        self.generation
    }
}

impl fmt::Debug for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MessageId({})", self.index)
    }
}

/// Index into a transcript's out-of-line tool-call table.
///
/// Tool calls carry two `CompactString`s and would triple the size of the
/// [`Content`](crate::Content) enum if stored inline, so they live in their own
/// array and the content block holds only this index.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ToolCallIdx(pub(crate) u32);

impl ToolCallIdx {
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}
