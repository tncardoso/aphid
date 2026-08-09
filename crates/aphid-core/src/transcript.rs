//! The session-owned conversation store.
//!
//! A transcript is a flat list of messages backed by two append-only arenas. It
//! is a single owned value: `Send`, droppable, and freed in one go, so the
//! memory of a conversation is tied to the conversation's own lifetime without
//! a lifetime parameter appearing anywhere in the API.
//!
//! Nothing here privileges the system prompt — it is a [`Role::System`] message
//! like any other. Mapping it onto a wire format belongs to provider encoders.

use crate::arena::{BlobArena, TextArena};
use crate::buffer::MessageBuffer;
use crate::content::{Content, ToolCallData};
use crate::error::{Error, Result};
use crate::id::{MessageId, ToolCallIdx};
use crate::message::{AssistantMeta, MessageHeader, Role, ToolResultMeta};
use crate::view::{Frame, MessageRef, MetaRef};

/// Content supplied when appending a message that is not streamed.
#[derive(Copy, Clone, Debug)]
pub enum ContentInput<'a> {
    Text(&'a str),
    Image { data: &'a [u8], mime: &'a str },
}

/// Sizes of every arena and table just before a message was appended.
///
/// Recorded per message so [`Transcript::truncate`] can rewind exactly, giving
/// last-in-first-out removal for free.
#[derive(Copy, Clone, Debug)]
struct Watermark {
    text: u32,
    blob: u32,
    blocks: u32,
    tool_calls: u32,
    assistant_meta: u32,
    tool_result_meta: u32,
}

/// Arena occupancy, for debug views and compaction decisions.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ArenaStats {
    pub messages: usize,
    pub blocks: usize,
    pub tool_calls: usize,
    /// Total bytes held by the text arena.
    pub text_bytes: u32,
    /// Bytes actually reachable from a live block.
    pub live_text_bytes: u32,
    pub blob_bytes: u32,
    pub live_blob_bytes: u32,
}

impl ArenaStats {
    /// Text bytes no live block refers to. Reclaimed by
    /// [`Transcript::compact_into`].
    #[must_use]
    pub const fn text_garbage_bytes(&self) -> u32 {
        self.text_bytes - self.live_text_bytes
    }
}

#[cfg(debug_assertions)]
static NEXT_GENERATION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// An ordered conversation and the arenas holding its contents.
#[derive(Debug)]
pub struct Transcript {
    text: TextArena,
    blob: BlobArena,
    blocks: Vec<Content>,
    messages: Vec<MessageHeader>,
    tool_calls: Vec<ToolCallData>,
    assistant_meta: Vec<AssistantMeta>,
    tool_result_meta: Vec<ToolResultMeta>,
    watermarks: Vec<Watermark>,
    #[cfg(debug_assertions)]
    generation: u32,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(0, 0)
    }

    /// Pre-size the arenas. Worth doing when resuming a known-size session, so
    /// the text arena never reallocates mid-conversation.
    #[must_use]
    pub fn with_capacity(text_bytes: usize, blocks: usize) -> Self {
        Self {
            text: TextArena::with_capacity(text_bytes),
            blob: BlobArena::default(),
            blocks: Vec::with_capacity(blocks),
            messages: Vec::new(),
            tool_calls: Vec::new(),
            assistant_meta: Vec::new(),
            tool_result_meta: Vec::new(),
            watermarks: Vec::new(),
            #[cfg(debug_assertions)]
            generation: NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Number of messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Read a message.
    ///
    /// # Panics
    /// Panics if the id does not belong to this transcript. In debug builds
    /// that includes ids minted before a [`Self::compact_into`] rebuild.
    #[must_use]
    pub fn message(&self, id: MessageId) -> MessageRef<'_> {
        self.view(self.resolve(id))
    }

    /// Read a message by position, returning `None` when out of range.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<MessageRef<'_>> {
        (index < self.messages.len()).then(|| self.view(index))
    }

    /// Read a message by position.
    ///
    /// # Errors
    /// Returns [`Error::UnknownMessage`] when `index` is out of range.
    pub fn try_get(&self, index: usize) -> Result<MessageRef<'_>> {
        self.get(index).ok_or(Error::UnknownMessage(index as u32))
    }

    /// The most recent message.
    #[must_use]
    pub fn last(&self) -> Option<MessageRef<'_>> {
        self.get(self.messages.len().checked_sub(1)?)
    }

    /// Iterate messages oldest first.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = MessageRef<'_>> {
        (0..self.messages.len()).map(|i| self.view(i))
    }

    /// Identify a message by position.
    #[must_use]
    pub fn id_at(&self, index: usize) -> Option<MessageId> {
        (index < self.messages.len()).then(|| self.make_id(index as u32))
    }

    /// Append a system message.
    pub fn push_system(&mut self, text: &str) -> MessageId {
        self.push_parts(
            Role::System,
            &[ContentInput::Text(text)],
            MessageHeader::NO_META,
        )
    }

    /// Append a user message.
    pub fn push_user(&mut self, text: &str) -> MessageId {
        self.push_parts(
            Role::User,
            &[ContentInput::Text(text)],
            MessageHeader::NO_META,
        )
    }

    /// Append a multi-part user message, for prompts that carry images.
    pub fn push_user_parts(&mut self, parts: &[ContentInput<'_>]) -> MessageId {
        self.push_parts(Role::User, parts, MessageHeader::NO_META)
    }

    /// Append the result of running a tool.
    pub fn push_tool_result(
        &mut self,
        meta: ToolResultMeta,
        parts: &[ContentInput<'_>],
    ) -> MessageId {
        let meta_index = self.tool_result_meta.len() as u32;
        self.tool_result_meta.push(meta);
        self.push_parts(Role::ToolResult, parts, meta_index)
    }

    /// Move a completed assistant turn out of its staging buffer and into the
    /// transcript.
    ///
    /// Costs one memcpy per arena regardless of how many tokens streamed.
    pub fn commit(&mut self, buf: MessageBuffer) -> MessageId {
        let timestamp = buf.timestamp();
        self.record_watermark();

        let text_offset = self.text.len();
        let blob_offset = self.blob.len();
        let tool_call_offset = self.tool_calls.len() as u32;

        self.text.push(buf.text.as_str());
        self.blob.push(buf.blob.as_slice());

        let block_start = self.blocks.len() as u32;
        self.blocks.extend(
            buf.blocks
                .iter()
                .map(|b| b.shifted(text_offset, blob_offset, tool_call_offset)),
        );
        self.tool_calls
            .extend(buf.tool_calls.into_iter().map(|t| t.shifted(text_offset)));

        let meta_index = self.assistant_meta.len() as u32;
        self.assistant_meta.push(buf.meta);

        self.push_header(Role::Assistant, timestamp, block_start, meta_index)
    }

    /// Drop all but the first `len` messages, rewinding the arenas exactly.
    ///
    /// This is the cheap removal path: because messages are appended
    /// contiguously, discarding a suffix reclaims its bytes with no copying and
    /// leaves no garbage behind.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.messages.len() {
            return;
        }
        let w = self.watermarks[len];
        self.messages.truncate(len);
        self.watermarks.truncate(len);
        self.blocks.truncate(w.blocks as usize);
        self.tool_calls.truncate(w.tool_calls as usize);
        self.assistant_meta.truncate(w.assistant_meta as usize);
        self.tool_result_meta.truncate(w.tool_result_meta as usize);
        self.text.truncate(w.text);
        self.blob.truncate(w.blob);
    }

    /// Copy the named messages, in the given order, into `out`.
    ///
    /// This is how every non-suffix edit is done — compaction, branch
    /// summarisation, dropping a message from the middle. Rebuilding into a
    /// fresh arena drops all accumulated garbage in one pass, and the caller
    /// swaps the result in.
    ///
    /// # Panics
    /// Panics if any id does not belong to this transcript.
    pub fn compact_into(&self, keep: &[MessageId], out: &mut Transcript) {
        for id in keep {
            let index = self.resolve(*id);
            self.copy_message_into(index, out);
        }
    }

    /// Arena occupancy, including how much of the text arena is garbage.
    #[must_use]
    pub fn arena_stats(&self) -> ArenaStats {
        let mut live_text = 0u32;
        let mut live_blob = 0u32;
        for block in &self.blocks {
            match *block {
                Content::Text { text, signature } => live_text += text.len() + signature.len(),
                Content::Thinking {
                    text, signature, ..
                } => {
                    live_text += text.len() + signature.len();
                }
                Content::Image { data, mime } => {
                    live_blob += data.len();
                    live_text += mime.len();
                }
                Content::ToolCall(idx) => {
                    let tc = &self.tool_calls[idx.index() as usize];
                    live_text += tc.arguments.len() + tc.thought_signature.len();
                }
            }
        }
        ArenaStats {
            messages: self.messages.len(),
            blocks: self.blocks.len(),
            tool_calls: self.tool_calls.len(),
            text_bytes: self.text.len(),
            live_text_bytes: live_text,
            blob_bytes: self.blob.len(),
            live_blob_bytes: live_blob,
        }
    }

    fn frame(&self) -> Frame<'_> {
        Frame {
            text: self.text.as_str(),
            blob: self.blob.as_slice(),
            blocks: &self.blocks,
            tool_calls: &self.tool_calls,
        }
    }

    fn view(&self, index: usize) -> MessageRef<'_> {
        let header = &self.messages[index];
        let meta = match header.role {
            Role::Assistant => MetaRef::Assistant(&self.assistant_meta[header.meta as usize]),
            Role::ToolResult => MetaRef::ToolResult(&self.tool_result_meta[header.meta as usize]),
            Role::System | Role::User => MetaRef::None,
        };
        MessageRef::new(self.frame(), header, meta)
    }

    fn push_parts(&mut self, role: Role, parts: &[ContentInput<'_>], meta_index: u32) -> MessageId {
        self.record_watermark();
        let block_start = self.blocks.len() as u32;
        for part in parts {
            let block = match *part {
                ContentInput::Text(text) => Content::Text {
                    text: self.text.push(text),
                    signature: crate::Span::EMPTY,
                },
                ContentInput::Image { data, mime } => {
                    let data = self.blob.push(data);
                    let mime = self.text.push(mime);
                    Content::Image { data, mime }
                }
            };
            self.blocks.push(block);
        }
        self.push_header(role, chrono::Utc::now(), block_start, meta_index)
    }

    fn push_header(
        &mut self,
        role: Role,
        timestamp: crate::Timestamp,
        block_start: u32,
        meta: u32,
    ) -> MessageId {
        let index = self.messages.len() as u32;
        self.messages.push(MessageHeader {
            role,
            timestamp,
            blocks: block_start..self.blocks.len() as u32,
            meta,
        });
        self.make_id(index)
    }

    fn copy_message_into(&self, index: usize, out: &mut Transcript) {
        let header = &self.messages[index];
        out.record_watermark();
        let block_start = out.blocks.len() as u32;

        for block in &self.blocks[header.blocks.start as usize..header.blocks.end as usize] {
            let copied = match *block {
                Content::Text { text, signature } => Content::Text {
                    text: out.text.push(self.text.get(text)),
                    signature: out.text.push(self.text.get(signature)),
                },
                Content::Thinking {
                    text,
                    signature,
                    redacted,
                } => Content::Thinking {
                    text: out.text.push(self.text.get(text)),
                    signature: out.text.push(self.text.get(signature)),
                    redacted,
                },
                Content::Image { data, mime } => Content::Image {
                    data: out.blob.push(self.blob.get(data)),
                    mime: out.text.push(self.text.get(mime)),
                },
                Content::ToolCall(idx) => {
                    let src = &self.tool_calls[idx.index() as usize];
                    let arguments = out.text.push(self.text.get(src.arguments));
                    let thought_signature = out.text.push(self.text.get(src.thought_signature));
                    let new_idx = ToolCallIdx(out.tool_calls.len() as u32);
                    out.tool_calls.push(ToolCallData {
                        id: src.id.clone(),
                        name: src.name.clone(),
                        arguments,
                        thought_signature,
                        namespace: src.namespace.clone(),
                    });
                    Content::ToolCall(new_idx)
                }
            };
            out.blocks.push(copied);
        }

        let meta = match header.role {
            Role::Assistant => {
                let i = out.assistant_meta.len() as u32;
                out.assistant_meta
                    .push(self.assistant_meta[header.meta as usize].clone());
                i
            }
            Role::ToolResult => {
                let i = out.tool_result_meta.len() as u32;
                out.tool_result_meta
                    .push(self.tool_result_meta[header.meta as usize].clone());
                i
            }
            Role::System | Role::User => MessageHeader::NO_META,
        };

        out.push_header(header.role, header.timestamp, block_start, meta);
    }

    fn record_watermark(&mut self) {
        self.watermarks.push(Watermark {
            text: self.text.len(),
            blob: self.blob.len(),
            blocks: self.blocks.len() as u32,
            tool_calls: self.tool_calls.len() as u32,
            assistant_meta: self.assistant_meta.len() as u32,
            tool_result_meta: self.tool_result_meta.len() as u32,
        });
    }

    fn make_id(&self, index: u32) -> MessageId {
        #[cfg(debug_assertions)]
        {
            MessageId::new(index, self.generation)
        }
        #[cfg(not(debug_assertions))]
        {
            MessageId::new(index, 0)
        }
    }

    fn resolve(&self, id: MessageId) -> usize {
        #[cfg(debug_assertions)]
        assert_eq!(
            id.generation(),
            self.generation,
            "MessageId belongs to a different transcript (or one rebuilt by compact_into)"
        );
        let index = id.index() as usize;
        assert!(index < self.messages.len(), "no message at index {index}");
        index
    }
}
