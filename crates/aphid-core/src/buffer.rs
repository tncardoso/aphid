//! Staging storage for a single assistant message while it streams.
//!
//! The buffer owns its own small arenas so a provider task can accumulate a
//! reply without touching (or locking) the shared transcript. When the turn
//! completes, [`Transcript::commit`](crate::Transcript::commit) moves the whole
//! thing across in one memcpy per arena.

use compact_str::CompactString;

use crate::arena::{BlobArena, TextArena};
use crate::content::{BlockKind, Content, ToolCallData};
use crate::id::{Timestamp, ToolCallIdx};
use crate::message::{AssistantMeta, MessageHeader, Role};
use crate::span::Span;
use crate::view::{Frame, MessageRef, MetaRef};

/// One in-flight assistant message.
#[derive(Debug)]
pub struct MessageBuffer {
    pub(crate) text: TextArena,
    pub(crate) blob: BlobArena,
    pub(crate) blocks: Vec<Content>,
    pub(crate) tool_calls: Vec<ToolCallData>,
    pub(crate) meta: AssistantMeta,
    /// Kept in lockstep with `blocks` so [`Self::partial`] has a header to lend.
    header: MessageHeader,
}

impl MessageBuffer {
    /// Start staging a turn described by `meta`.
    #[must_use]
    pub fn new(meta: AssistantMeta) -> Self {
        Self {
            text: TextArena::default(),
            blob: BlobArena::default(),
            blocks: Vec::new(),
            tool_calls: Vec::new(),
            meta,
            header: MessageHeader {
                role: Role::Assistant,
                timestamp: chrono::Utc::now(),
                blocks: 0..0,
                meta: 0,
            },
        }
    }

    /// Open a text block and return its index.
    pub fn begin_text(&mut self) -> u32 {
        let text = self.text.open();
        self.push_block(Content::Text {
            text,
            signature: Span::EMPTY,
        })
    }

    /// Open a thinking block and return its index.
    pub fn begin_thinking(&mut self) -> u32 {
        let text = self.text.open();
        self.push_block(Content::Thinking {
            text,
            signature: Span::EMPTY,
            redacted: false,
        })
    }

    /// Open a tool-call block and return its index.
    ///
    /// Arguments arrive later through [`Self::push_delta`], accumulating as raw
    /// JSON text in the arena.
    pub fn begin_tool_call(
        &mut self,
        id: impl Into<CompactString>,
        name: impl Into<CompactString>,
    ) -> u32 {
        let arguments = self.text.open();
        let idx = ToolCallIdx(self.tool_calls.len() as u32);
        self.tool_calls.push(ToolCallData {
            id: id.into(),
            name: name.into(),
            arguments,
            thought_signature: Span::EMPTY,
            namespace: None,
        });
        self.push_block(Content::ToolCall(idx))
    }

    /// Append a complete image block. Images do not stream.
    pub fn push_image(&mut self, data: &[u8], mime: &str) -> u32 {
        let data = self.blob.push(data);
        let mime = self.text.push(mime);
        self.push_block(Content::Image { data, mime })
    }

    /// Append `delta` to the block at `index`, returning the span covering the
    /// newly written bytes.
    ///
    /// This is the zero-copy path: the bytes go straight to the arena tail and
    /// the block's span grows. If the block is not at the tail — which happens
    /// only when a provider interleaves blocks — it is first relocated there,
    /// which is correct but costs a copy of what it has accumulated so far.
    ///
    /// # Panics
    /// Panics if `index` is out of range or names an image block.
    pub fn push_delta(&mut self, index: u32, delta: &str) -> Span {
        let block = self.blocks[index as usize];
        let mut span = match block {
            Content::Text { text, .. } | Content::Thinking { text, .. } => text,
            Content::ToolCall(idx) => self.tool_calls[idx.index() as usize].arguments,
            Content::Image { .. } => panic!("cannot stream a delta into an image block"),
        };

        if !self.text.ends_at_tail(span) {
            let existing = self.text.get(span).to_owned();
            span = self.text.push(&existing);
        }

        let delta_start = span.end();
        self.text.extend(&mut span, delta);

        match block {
            Content::Text { signature, .. } => {
                self.blocks[index as usize] = Content::Text {
                    text: span,
                    signature,
                };
            }
            Content::Thinking {
                signature,
                redacted,
                ..
            } => {
                self.blocks[index as usize] = Content::Thinking {
                    text: span,
                    signature,
                    redacted,
                };
            }
            Content::ToolCall(idx) => {
                self.tool_calls[idx.index() as usize].arguments = span;
            }
            Content::Image { .. } => unreachable!(),
        }

        Span::new(delta_start, delta.len() as u32)
    }

    /// Attach the provider's opaque signature to a text or thinking block.
    ///
    /// # Panics
    /// Panics if `index` names a block that cannot carry a signature.
    pub fn set_signature(&mut self, index: u32, signature: &str) {
        let span = self.text.push(signature);
        match &mut self.blocks[index as usize] {
            Content::Text { signature: s, .. } | Content::Thinking { signature: s, .. } => {
                *s = span
            }
            other => panic!("{:?} blocks do not carry a signature", other.kind()),
        }
    }

    /// Mark a thinking block as withheld by a safety filter.
    ///
    /// # Panics
    /// Panics if `index` does not name a thinking block.
    pub fn set_redacted(&mut self, index: u32, value: bool) {
        match &mut self.blocks[index as usize] {
            Content::Thinking { redacted, .. } => *redacted = value,
            other => panic!("{:?} blocks cannot be redacted", other.kind()),
        }
    }

    /// Attach a Google-style thought signature to a tool call.
    ///
    /// # Panics
    /// Panics if `index` does not name a tool-call block.
    pub fn set_thought_signature(&mut self, index: u32, signature: &str) {
        let span = self.text.push(signature);
        match self.blocks[index as usize] {
            Content::ToolCall(idx) => {
                self.tool_calls[idx.index() as usize].thought_signature = span
            }
            ref other => panic!("{:?} blocks do not carry a thought signature", other.kind()),
        }
    }

    /// Set the namespace of a tool call.
    ///
    /// # Panics
    /// Panics if `index` does not name a tool-call block.
    pub fn set_namespace(&mut self, index: u32, namespace: impl Into<CompactString>) {
        match self.blocks[index as usize] {
            Content::ToolCall(idx) => {
                self.tool_calls[idx.index() as usize].namespace = Some(namespace.into());
            }
            ref other => panic!("{:?} blocks do not carry a namespace", other.kind()),
        }
    }

    /// Kind of the block at `index`.
    ///
    /// # Panics
    /// Panics if `index` is out of range.
    #[must_use]
    pub fn block_kind(&self, index: u32) -> BlockKind {
        self.blocks[index as usize].kind()
    }

    /// Number of blocks staged so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Metadata for the turn.
    #[must_use]
    pub fn meta(&self) -> &AssistantMeta {
        &self.meta
    }

    /// Mutable metadata, for filling in usage and stop reason as they arrive.
    pub fn meta_mut(&mut self) -> &mut AssistantMeta {
        &mut self.meta
    }

    /// Override the message timestamp, which otherwise defaults to when the
    /// buffer was created.
    pub fn set_timestamp(&mut self, timestamp: Timestamp) {
        self.header.timestamp = timestamp;
    }

    #[must_use]
    pub fn timestamp(&self) -> Timestamp {
        self.header.timestamp
    }

    /// Resolve a span produced by [`Self::push_delta`].
    #[must_use]
    pub fn text(&self, span: Span) -> &str {
        self.text.get(span)
    }

    /// Read the message accumulated so far, through the ordinary view types.
    #[must_use]
    pub fn partial(&self) -> MessageRef<'_> {
        MessageRef::new(self.frame(), &self.header, MetaRef::Assistant(&self.meta))
    }

    pub(crate) fn frame(&self) -> Frame<'_> {
        Frame {
            text: self.text.as_str(),
            blob: self.blob.as_slice(),
            blocks: &self.blocks,
            tool_calls: &self.tool_calls,
        }
    }

    fn push_block(&mut self, block: Content) -> u32 {
        let index = self.blocks.len() as u32;
        self.blocks.push(block);
        self.header.blocks.end = self.blocks.len() as u32;
        index
    }
}
