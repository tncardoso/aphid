//! The public read path over arena-backed conversation storage.
//!
//! Spans never leave this module. A [`Frame`] bundles borrowed slices of the
//! arenas and block tables, so the same view types serve both a whole
//! [`Transcript`](crate::Transcript) and a single in-flight
//! [`MessageBuffer`](crate::MessageBuffer) without a generic parameter.

use std::fmt;

use crate::content::{BlockKind, Content, ToolCallData};
use crate::error::Result;
use crate::id::Timestamp;
use crate::json::{self, Json};
use crate::message::{AssistantMeta, MessageHeader, Role, ToolResultMeta};

/// Borrowed slices of one storage's arenas and block tables.
#[derive(Copy, Clone)]
pub(crate) struct Frame<'t> {
    pub(crate) text: &'t str,
    pub(crate) blob: &'t [u8],
    pub(crate) blocks: &'t [Content],
    pub(crate) tool_calls: &'t [ToolCallData],
}

/// Role-specific metadata borrowed from whichever side table applies.
#[derive(Copy, Clone, Debug)]
pub enum MetaRef<'t> {
    /// System and user messages carry no extra metadata.
    None,
    Assistant(&'t AssistantMeta),
    ToolResult(&'t ToolResultMeta),
}

/// A borrowed view of one message.
#[derive(Copy, Clone)]
pub struct MessageRef<'t> {
    frame: Frame<'t>,
    header: &'t MessageHeader,
    meta: MetaRef<'t>,
}

impl<'t> MessageRef<'t> {
    pub(crate) fn new(frame: Frame<'t>, header: &'t MessageHeader, meta: MetaRef<'t>) -> Self {
        Self {
            frame,
            header,
            meta,
        }
    }

    #[must_use]
    pub fn role(&self) -> Role {
        self.header.role
    }

    #[must_use]
    pub fn timestamp(&self) -> Timestamp {
        self.header.timestamp
    }

    /// Number of content blocks in this message.
    #[must_use]
    pub fn len(&self) -> usize {
        (self.header.blocks.end - self.header.blocks.start) as usize
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate the message's content blocks.
    #[must_use]
    pub fn content(&self) -> ContentIter<'t> {
        ContentIter {
            frame: self.frame,
            next: self.header.blocks.start,
            end: self.header.blocks.end,
        }
    }

    /// Role-specific metadata.
    #[must_use]
    pub fn meta(&self) -> MetaRef<'t> {
        self.meta
    }

    /// Assistant metadata, or `None` for any other role.
    #[must_use]
    pub fn assistant(&self) -> Option<&'t AssistantMeta> {
        match self.meta {
            MetaRef::Assistant(m) => Some(m),
            _ => None,
        }
    }

    /// Tool-result metadata, or `None` for any other role.
    #[must_use]
    pub fn tool_result(&self) -> Option<&'t ToolResultMeta> {
        match self.meta {
            MetaRef::ToolResult(m) => Some(m),
            _ => None,
        }
    }
}

impl fmt::Debug for MessageRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageRef")
            .field("role", &self.role())
            .field("timestamp", &self.timestamp())
            .field("content", &self.content().collect::<Vec<_>>())
            .field("meta", &self.meta)
            .finish()
    }
}

/// Iterator over the content blocks of a message.
#[derive(Clone)]
pub struct ContentIter<'t> {
    frame: Frame<'t>,
    next: u32,
    end: u32,
}

impl<'t> Iterator for ContentIter<'t> {
    type Item = ContentRef<'t>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.end {
            return None;
        }
        let block = self.frame.blocks[self.next as usize];
        self.next += 1;
        Some(ContentRef::resolve(self.frame, block))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = (self.end - self.next) as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for ContentIter<'_> {}

/// A borrowed view of one content block, with every span already resolved.
#[derive(Copy, Clone, Debug)]
pub enum ContentRef<'t> {
    Text(TextRef<'t>),
    Thinking(ThinkingRef<'t>),
    Image(ImageRef<'t>),
    ToolCall(ToolCallRef<'t>),
}

impl<'t> ContentRef<'t> {
    fn resolve(frame: Frame<'t>, block: Content) -> Self {
        match block {
            Content::Text { text, signature } => ContentRef::Text(TextRef {
                text: &frame.text[text.range()],
                signature: &frame.text[signature.range()],
            }),
            Content::Thinking {
                text,
                signature,
                redacted,
            } => ContentRef::Thinking(ThinkingRef {
                text: &frame.text[text.range()],
                signature: &frame.text[signature.range()],
                redacted,
            }),
            Content::Image { data, mime } => ContentRef::Image(ImageRef {
                data: &frame.blob[data.range()],
                mime: &frame.text[mime.range()],
            }),
            Content::ToolCall(idx) => ContentRef::ToolCall(ToolCallRef {
                data: &frame.tool_calls[idx.index() as usize],
                arena: frame.text,
            }),
        }
    }

    #[must_use]
    pub fn kind(&self) -> BlockKind {
        match self {
            ContentRef::Text(_) => BlockKind::Text,
            ContentRef::Thinking(_) => BlockKind::Thinking,
            ContentRef::Image(_) => BlockKind::Image,
            ContentRef::ToolCall(_) => BlockKind::ToolCall,
        }
    }

    /// The block's text, for the kinds that have any.
    #[must_use]
    pub fn text(&self) -> Option<&'t str> {
        match self {
            ContentRef::Text(t) => Some(t.text()),
            ContentRef::Thinking(t) => Some(t.text()),
            ContentRef::Image(_) | ContentRef::ToolCall(_) => None,
        }
    }
}

/// Model-visible prose.
#[derive(Copy, Clone)]
pub struct TextRef<'t> {
    text: &'t str,
    signature: &'t str,
}

impl<'t> TextRef<'t> {
    #[must_use]
    pub fn text(&self) -> &'t str {
        self.text
    }

    /// Provider message metadata replayed on the next turn, when present.
    #[must_use]
    pub fn signature(&self) -> Option<&'t str> {
        non_empty(self.signature)
    }
}

impl fmt::Debug for TextRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Text").field("text", &self.text).finish()
    }
}

/// Reasoning the model emitted before its answer.
#[derive(Copy, Clone)]
pub struct ThinkingRef<'t> {
    text: &'t str,
    signature: &'t str,
    redacted: bool,
}

impl<'t> ThinkingRef<'t> {
    #[must_use]
    pub fn text(&self) -> &'t str {
        self.text
    }

    /// Opaque signature that must be replayed verbatim for multi-turn continuity.
    #[must_use]
    pub fn signature(&self) -> Option<&'t str> {
        non_empty(self.signature)
    }

    /// Whether a safety filter withheld the reasoning text.
    #[must_use]
    pub fn redacted(&self) -> bool {
        self.redacted
    }
}

impl fmt::Debug for ThinkingRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Thinking")
            .field("text", &self.text)
            .field("redacted", &self.redacted)
            .finish()
    }
}

/// Binary image data plus its media type.
#[derive(Copy, Clone)]
pub struct ImageRef<'t> {
    data: &'t [u8],
    mime: &'t str,
}

impl<'t> ImageRef<'t> {
    #[must_use]
    pub fn data(&self) -> &'t [u8] {
        self.data
    }

    #[must_use]
    pub fn mime(&self) -> &'t str {
        self.mime
    }
}

impl fmt::Debug for ImageRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Image")
            .field("mime", &self.mime)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// A request from the model to run a tool.
#[derive(Copy, Clone)]
pub struct ToolCallRef<'t> {
    data: &'t ToolCallData,
    arena: &'t str,
}

impl<'t> ToolCallRef<'t> {
    /// Provider-assigned call id, echoed back on the matching tool result.
    #[must_use]
    pub fn id(&self) -> &'t str {
        &self.data.id
    }

    #[must_use]
    pub fn name(&self) -> &'t str {
        &self.data.name
    }

    /// Arguments exactly as the provider sent them, unparsed.
    ///
    /// Prefer this when replaying to a provider: it is byte-identical and costs
    /// nothing.
    #[must_use]
    pub fn arguments_raw(&self) -> &'t str {
        &self.arena[self.data.arguments.range()]
    }

    /// Arguments parsed into a JSON value.
    ///
    /// # Errors
    /// Returns [`Error::Json`](crate::Error::Json) when the model produced
    /// malformed JSON, which does happen and is worth surfacing rather than
    /// papering over.
    pub fn arguments(&self) -> Result<Json> {
        Ok(json::parse(self.arguments_raw())?)
    }

    /// Google-style opaque thought signature.
    #[must_use]
    pub fn thought_signature(&self) -> Option<&'t str> {
        non_empty(&self.arena[self.data.thought_signature.range()])
    }

    /// Namespace for calls to dynamically loaded tools.
    #[must_use]
    pub fn namespace(&self) -> Option<&'t str> {
        self.data.namespace.as_deref()
    }
}

impl fmt::Debug for ToolCallRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCall")
            .field("id", &self.id())
            .field("name", &self.name())
            .field("arguments", &self.arguments_raw())
            .finish()
    }
}

fn non_empty(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}
