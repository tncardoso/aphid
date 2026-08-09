//! Message, model and streaming types for the aphid agent harness.
//!
//! # The memory model
//!
//! A conversation lives in a [`Transcript`]: a flat list of messages over two
//! append-only arenas, one for text and one for binary payloads. Content blocks
//! hold byte ranges rather than owned strings, so a whole session is a handful
//! of allocations that are freed together when the transcript drops. No lifetime
//! parameter escapes into user code — indices are self-relative, so a
//! `Transcript` is a single owned, `Send` value.
//!
//! Spans stay inside the crate. Everything is read through [`MessageRef`] and
//! [`ContentRef`], which resolve ranges against the arena and hand back plain
//! `&str`.
//!
//! Streaming is where the layout pays off. A provider accumulates a reply into a
//! [`MessageBuffer`] with its own arenas; each delta is appended to the arena
//! tail exactly once, and [`Event`] carries only the [`Span`] of the bytes just
//! written. [`Transcript::commit`] then moves the finished turn across in one
//! memcpy per arena, however many tokens streamed.
//!
//! The system prompt is not special: it is a message with [`Role::System`].
//! Mapping it onto a wire format is a provider encoder's job.
//!
//! # Example
//!
//! ```
//! use aphid_core::{
//!     Api, AssistantMeta, ContentInput, ContentRef, MessageBuffer, ProviderId, Role,
//!     StopReason, ToolResultMeta, Transcript,
//! };
//!
//! let mut transcript = Transcript::new();
//! transcript.push_system("You are terse.");
//! transcript.push_user("What is 2 + 2?");
//!
//! // A streamed assistant turn: a tool call whose arguments arrive in pieces.
//! let meta = AssistantMeta::new(
//!     Api::OpenAiCompletions,
//!     ProviderId::DEEPSEEK,
//!     "deepseek-v4-flash",
//! );
//! let mut turn = MessageBuffer::new(meta);
//! let call = turn.begin_tool_call("call_1", "calculator");
//! turn.push_delta(call, r#"{"expr":"#);
//! turn.push_delta(call, r#""2+2"}"#);
//! turn.meta_mut().stop_reason = StopReason::ToolUse;
//! transcript.commit(turn);
//!
//! transcript.push_tool_result(
//!     ToolResultMeta::new("call_1", "calculator"),
//!     &[ContentInput::Text("4")],
//! );
//!
//! // Read it back: no spans, just borrowed strings.
//! let assistant = transcript.get(2).unwrap();
//! assert_eq!(assistant.role(), Role::Assistant);
//! let ContentRef::ToolCall(tool_call) = assistant.content().next().unwrap() else {
//!     panic!("expected a tool call");
//! };
//! assert_eq!(tool_call.name(), "calculator");
//! assert_eq!(tool_call.arguments_raw(), r#"{"expr":"2+2"}"#);
//! assert_eq!(tool_call.arguments().unwrap()["expr"], "2+2");
//! assert_eq!(transcript.len(), 4);
//! ```

mod arena;
mod buffer;
mod compat;
mod content;
mod error;
mod event;
mod id;
pub mod json;
mod message;
mod model;
mod options;
mod provider;
pub mod providers;
mod span;
mod thinking;
mod tool;
mod transcript;
mod view;

pub use buffer::MessageBuffer;
pub use compat::{Compat, MaxTokensField, OpenAiCompletionsCompat, ThinkingFormat};
pub use content::BlockKind;
pub use error::{Diagnostic, DiagnosticError, Error, Result};
pub use event::{AssistantStream, Event};
pub use id::{MessageId, Timestamp, ToolCallIdx};
pub use json::{Json, JsonError};
pub use message::{AssistantMeta, Role, StopReason, ToolResultMeta};
pub use model::{InputModalities, Model, ModelCost, ModelCostRates, ModelCostTier};
pub use options::{CacheRetention, RequestOptions, SimpleStreamOptions, StreamOptions};
pub use provider::{Api, ProviderId};
pub use span::{BlobSpan, Span};
pub use thinking::{
    LevelMapping, ModelThinkingLevel, ThinkingBudgets, ThinkingLevel, ThinkingLevelMap,
};
pub use tool::{ConstrainedSampling, GrammarFormat, GrammarVariants, Strictness, Tool};
pub use transcript::{ArenaStats, ContentInput, Transcript};
pub use usage::{Cost, Usage};
pub use view::{
    ContentIter, ContentRef, ImageRef, MessageRef, MetaRef, TextRef, ThinkingRef, ToolCallRef,
};

mod usage;

/// Layout guarantees the data-oriented design depends on. A regression here is
/// a silent memory-footprint regression across every conversation, so it fails
/// the build instead.
mod layout {
    use super::*;
    use std::mem::size_of;

    const _: () = assert!(size_of::<Span>() == 8);
    const _: () = assert!(size_of::<crate::content::Content>() <= 24);
    const _: () = assert!(size_of::<Event>() <= 16);
    const _: () = assert!(size_of::<crate::message::MessageHeader>() <= 32);
}
