//! The streaming protocol.

use futures_core::Stream;

use crate::buffer::MessageBuffer;
use crate::content::BlockKind;
use crate::message::StopReason;
use crate::span::Span;
use crate::view::MessageRef;

/// One step of a streamed assistant turn.
///
/// Sixteen bytes, [`Copy`] and `'static`: an event can be queued, buffered or
/// sent to a renderer on another task. It carries no text of its own — a delta
/// names the bytes that were just appended to the stream's arena, which the
/// consumer resolves with [`AssistantStream::text`]. That keeps per-token cost
/// at zero copies.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// Emitted once, before any other event.
    Start,
    /// A new content block was opened at `index`.
    BlockStart { index: u32, kind: BlockKind },
    /// Bytes were appended to the block at `index`.
    ///
    /// For a tool call the bytes are raw JSON arguments, not prose.
    Delta {
        index: u32,
        kind: BlockKind,
        span: Span,
    },
    /// The block at `index` is complete.
    BlockEnd { index: u32 },
    /// The turn finished successfully. Terminal.
    Done { stop: StopReason },
    /// The turn failed or was cancelled. Terminal; details are on the message's
    /// [`AssistantMeta`](crate::AssistantMeta).
    Error { stop: StopReason },
}

impl Event {
    /// Whether this event ends the stream.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Event::Done { .. } | Event::Error { .. })
    }
}

/// A provider response in progress.
///
/// Implementations must uphold the protocol: emit [`Event::Start`] before any
/// other event, and terminate with exactly one [`Event::Done`] or
/// [`Event::Error`]. Request, model and transport failures are reported through
/// [`Event::Error`] plus [`AssistantMeta::stop_reason`] and
/// [`AssistantMeta::error_message`] — never by panicking or by failing to
/// construct the stream.
///
/// [`AssistantMeta::stop_reason`]: crate::AssistantMeta::stop_reason
/// [`AssistantMeta::error_message`]: crate::AssistantMeta::error_message
pub trait AssistantStream: Stream<Item = Event> {
    /// Resolve a span carried by [`Event::Delta`].
    fn text(&self, span: Span) -> &str;

    /// The message accumulated so far.
    ///
    /// This is the snapshot affordance a renderer wants, lent rather than
    /// cloned, so it costs nothing to call on every event.
    fn partial(&self) -> MessageRef<'_>;

    /// Take the finished message, ready for
    /// [`Transcript::commit`](crate::Transcript::commit).
    fn finish(self) -> MessageBuffer
    where
        Self: Sized;
}
