//! Content blocks: the arena-side representation of everything inside a message.

use compact_str::CompactString;

use crate::id::ToolCallIdx;
use crate::span::{BlobSpan, Span};

/// What kind of payload a content block holds.
///
/// Carried on stream events so a consumer can interpret a delta without having
/// to look the block up.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum BlockKind {
    Text,
    Thinking,
    Image,
    ToolCall,
}

/// One content block, stored in a transcript's block array.
///
/// Twenty-four bytes: payloads are spans into the arenas, and tool calls — the
/// only variant with owned strings — are held out of line so they cannot inflate
/// the common cases.
///
/// This type is internal. Everything outside the crate reads blocks through
/// [`ContentRef`](crate::ContentRef), which resolves spans against the arena and
/// hands back plain `&str`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Content {
    Text {
        text: Span,
        /// Provider message metadata; [`Span::EMPTY`] when absent.
        signature: Span,
    },
    Thinking {
        text: Span,
        /// Opaque provider signature, replayed verbatim on the next turn.
        signature: Span,
        /// Content was withheld by a safety filter; `signature` still carries the
        /// encrypted payload needed for multi-turn continuity.
        redacted: bool,
    },
    Image {
        data: BlobSpan,
        mime: Span,
    },
    ToolCall(ToolCallIdx),
}

impl Content {
    pub(crate) const fn kind(&self) -> BlockKind {
        match self {
            Content::Text { .. } => BlockKind::Text,
            Content::Thinking { .. } => BlockKind::Thinking,
            Content::Image { .. } => BlockKind::Image,
            Content::ToolCall(_) => BlockKind::ToolCall,
        }
    }

    /// Rebase every arena reference by the given offsets.
    ///
    /// Used when a staged message is committed into a transcript whose arenas
    /// already hold bytes, and when compaction rebuilds an arena from scratch.
    pub(crate) fn shifted(self, text: u32, blob: u32, tool_calls: u32) -> Self {
        match self {
            Content::Text { text: t, signature } => Content::Text {
                text: t.shifted(text),
                signature: signature.shifted(text),
            },
            Content::Thinking {
                text: t,
                signature,
                redacted,
            } => Content::Thinking {
                text: t.shifted(text),
                signature: signature.shifted(text),
                redacted,
            },
            Content::Image { data, mime } => Content::Image {
                data: data.shifted(blob),
                mime: mime.shifted(text),
            },
            Content::ToolCall(idx) => Content::ToolCall(ToolCallIdx(idx.0 + tool_calls)),
        }
    }
}

/// The out-of-line payload of a [`Content::ToolCall`] block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolCallData {
    pub(crate) id: CompactString,
    pub(crate) name: CompactString,
    /// Raw JSON text as the provider sent it, held in the text arena.
    ///
    /// Keeping the bytes rather than a parsed tree means argument deltas append
    /// to the arena tail exactly like text, replay to the provider is
    /// byte-identical, and parsing is lazy and optional.
    pub(crate) arguments: Span,
    /// Google-style opaque thought signature; [`Span::EMPTY`] when absent.
    pub(crate) thought_signature: Span,
    /// Namespace for calls to dynamically loaded tools.
    pub(crate) namespace: Option<CompactString>,
}

impl ToolCallData {
    pub(crate) fn shifted(mut self, text: u32) -> Self {
        self.arguments = self.arguments.shifted(text);
        self.thought_signature = self.thought_signature.shifted(text);
        self
    }
}
