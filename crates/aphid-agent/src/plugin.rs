//! What the loop hands a listener while a response is streaming.
//!
//! Everything else that used to live here — the plugin trait, the interest
//! bitset, the per-hook payloads — is gone. A component subscribes to
//! [`events`](crate::events) instead, and the runtime decides when it runs.
//! What is left is the one thing an event cannot carry: a borrow into the
//! stream's arena, resolved while the response is still open.

use aphid_core::{MessageId, MessageRef, Span, StopReason, Usage};

use crate::stream::DynAssistantStream;

/// What a listener can see while a response is streaming.
pub struct StreamCx<'a> {
    pub(crate) stream: &'a (dyn DynAssistantStream + Send + Unpin),
    pub(crate) turn: u32,
}

impl StreamCx<'_> {
    /// Resolve the bytes named by an [`Event::Delta`](aphid_core::Event::Delta)
    /// span. Zero copies: this is a borrow of the stream's own arena.
    #[must_use]
    pub fn text(&self, span: Span) -> &str {
        self.stream.text(span)
    }

    /// The assistant message accumulated so far.
    #[must_use]
    pub fn partial(&self) -> MessageRef<'_> {
        self.stream.partial()
    }

    #[must_use]
    pub fn turn(&self) -> u32 {
        self.turn
    }
}

/// What one turn produced.
#[derive(Clone, Debug)]
pub struct TurnSummary {
    /// The assistant message committed for this turn.
    pub message: MessageId,
    pub stop_reason: StopReason,
    pub usage: Usage,
    /// How many tools the turn asked for.
    pub tool_calls: usize,
    pub error: Option<String>,
}
