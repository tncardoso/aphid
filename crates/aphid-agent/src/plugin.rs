//! The plugin API.
//!
//! A plugin observes and steers a run. Every hook is **synchronous**: the only
//! per-token hook is [`Plugin::on_event`], and a boxed future per token would
//! undo the point of the core's arena layout. Work that has to await belongs in
//! a [`ToolHandler`](crate::ToolHandler).
//!
//! Plugins declare which hooks they want with [`Plugin::interests`]. The
//! registry turns those declarations into one index list per hook, so a hook
//! nobody subscribed to costs an empty-slice check.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aphid_core::{Event, MessageId, MessageRef, Model, Span, StopReason, Transcript, Usage};

use crate::stream::DynAssistantStream;
use crate::tool::{ToolHandler, ToolOutcome};

/// The set of hooks a plugin wants to receive.
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub struct Interest(u16);

impl Interest {
    pub const RUN_START: Interest = Interest(1 << 0);
    pub const TURN_START: Interest = Interest(1 << 1);
    pub const EVENT: Interest = Interest(1 << 2);
    pub const TOOL_CALL: Interest = Interest(1 << 3);
    pub const TOOL_RESULT: Interest = Interest(1 << 4);
    pub const TURN_END: Interest = Interest(1 << 5);
    pub const RUN_END: Interest = Interest(1 << 6);

    /// Every hook. The default, so a plugin that only overrides the methods it
    /// cares about still works.
    pub const ALL: Interest = Interest(0b111_1111);

    /// How many hooks there are, and so how many index lists the registry keeps.
    pub(crate) const COUNT: usize = 7;

    #[must_use]
    pub const fn empty() -> Self {
        Interest(0)
    }

    #[must_use]
    pub const fn contains(self, other: Interest) -> bool {
        self.0 & other.0 == other.0
    }

    pub(crate) const fn bit(index: usize) -> Interest {
        Interest(1 << index)
    }
}

impl std::ops::BitOr for Interest {
    type Output = Interest;

    fn bitor(self, rhs: Interest) -> Interest {
        Interest(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Interest {
    fn bitor_assign(&mut self, rhs: Interest) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for Interest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Interest({:#09b})", self.0)
    }
}

/// Whether a run continues after a turn.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Flow {
    #[default]
    Continue,
    /// Stop the run cleanly once this turn's results are committed.
    Stop,
}

/// A plugin's verdict on a tool call it was shown.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Guard {
    #[default]
    Allow,
    /// Do not run the tool. An error result carrying `reason` is committed in
    /// its place, so the model sees why.
    Block { reason: String, terminate: bool },
}

impl Guard {
    /// Block this call and let the run continue.
    #[must_use]
    pub fn block(reason: impl Into<String>) -> Self {
        Guard::Block {
            reason: reason.into(),
            terminate: false,
        }
    }

    /// Block this call and ask the run to stop after the batch. Honoured only
    /// when every result in the batch asks for it.
    #[must_use]
    pub fn block_and_stop(reason: impl Into<String>) -> Self {
        Guard::Block {
            reason: reason.into(),
            terminate: true,
        }
    }
}

/// What a plugin can see and change between requests.
///
/// The transcript is append-only, so a hook adds context rather than rewriting
/// it. Everything appended here is part of the conversation the provider sees
/// and stays in the transcript afterwards, which is what makes a run replayable.
pub struct Cx<'a> {
    pub(crate) transcript: &'a mut Transcript,
    pub(crate) model: &'a Model,
    pub(crate) turn: u32,
    pub(crate) usage: Usage,
    pub(crate) cancel: &'a AtomicBool,
}

impl Cx<'_> {
    #[must_use]
    pub fn transcript(&self) -> &Transcript {
        self.transcript
    }

    /// The model this run is talking to.
    #[must_use]
    pub fn model(&self) -> &Model {
        self.model
    }

    /// Zero-based index of the turn about to start, or that just ended.
    #[must_use]
    pub fn turn(&self) -> u32 {
        self.turn
    }

    /// Tokens and cost accumulated by this run so far.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Append a system message at the tail of the transcript.
    ///
    /// The core treats the system prompt as an ordinary [`Role::System`] message
    /// and the arenas are append-only, so a hook cannot rewrite the prompt in
    /// place. A trailing note is the shape that does work, and it survives in
    /// the transcript as a record of what was actually sent. To replace the
    /// prompt wholesale, use [`Agent::set_system`](crate::Agent::set_system)
    /// between runs.
    ///
    /// [`Role::System`]: aphid_core::Role::System
    pub fn push_system_note(&mut self, text: &str) -> MessageId {
        self.transcript.push_system(text)
    }

    /// Append a user message at the tail of the transcript.
    pub fn push_user(&mut self, text: &str) -> MessageId {
        self.transcript.push_user(text)
    }

    /// Ask the run to stop at the next checkpoint.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Context for run-scoped hooks.
pub type RunCx<'a> = Cx<'a>;
/// Context for turn-scoped hooks.
pub type TurnCx<'a> = Cx<'a>;

/// What a plugin can see while a response is streaming.
pub struct StreamCx<'a> {
    pub(crate) stream: &'a (dyn DynAssistantStream + Send + Unpin),
    pub(crate) turn: u32,
}

impl StreamCx<'_> {
    /// Resolve the bytes named by an [`Event::Delta`] span. Zero copies: this
    /// is a borrow of the stream's own arena.
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

/// A tool call that has been requested but not yet run.
///
/// `arguments` is borrowed from the transcript arena until a hook replaces it,
/// so inspecting a call costs nothing.
pub struct PendingCall<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) arguments: Cow<'a, str>,
    pub(crate) handler: Option<Arc<dyn ToolHandler>>,
    pub(crate) block: Option<Guard>,
}

impl PendingCall<'_> {
    #[must_use]
    pub fn id(&self) -> &str {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.name
    }

    /// The raw JSON arguments, including any edit made by an earlier hook.
    #[must_use]
    pub fn arguments(&self) -> &str {
        &self.arguments
    }

    /// Whether a tool is registered under this name. A call with no handler
    /// becomes an error result the model can correct.
    #[must_use]
    pub fn is_known(&self) -> bool {
        self.handler.is_some()
    }

    /// Replace the arguments before the tool runs.
    ///
    /// Nothing re-validates the replacement against the tool's schema; a handler
    /// built with [`tool_fn`](crate::tool_fn) will report a deserialization
    /// failure as an error result.
    pub fn set_arguments(&mut self, arguments: impl Into<String>) {
        self.arguments = Cow::Owned(arguments.into());
    }

    /// Whether an earlier hook already blocked this call.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.block.is_some()
    }
}

/// What a plugin can see while patching a tool result.
pub struct ResultCx<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) arguments: &'a str,
    pub(crate) turn: u32,
}

impl ResultCx<'_> {
    #[must_use]
    pub fn id(&self) -> &str {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.name
    }

    #[must_use]
    pub fn arguments(&self) -> &str {
        self.arguments
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

/// Extends an agent run.
///
/// Every method has a default, so a plugin implements only what it needs. Narrow
/// [`Plugin::interests`] to skip the dispatch entirely for the rest.
pub trait Plugin: Send + Sync + 'static {
    /// Used in diagnostics and to resolve tool-name collisions.
    fn name(&self) -> &str;

    /// Which hooks to dispatch to this plugin.
    fn interests(&self) -> Interest {
        Interest::ALL
    }

    /// Tools this plugin contributes. Registered after the builder's own tools,
    /// so a plugin can shadow one by reusing its name.
    fn tools(&self) -> Vec<Arc<dyn ToolHandler>> {
        Vec::new()
    }

    /// A run is about to start, after the prompt has been appended.
    fn on_run_start(&self, _cx: &mut RunCx<'_>) {}

    /// A request is about to be sent. The last chance to add context.
    fn on_turn_start(&self, _cx: &mut TurnCx<'_>) {}

    /// One protocol event. The hot path — keep it cheap.
    fn on_event(&self, _event: &Event, _cx: &StreamCx<'_>) {}

    /// A tool call has been requested. Return [`Guard::Block`] to stop it, or
    /// mutate the call to patch its arguments.
    fn on_tool_call(&self, _call: &mut PendingCall<'_>) -> Guard {
        Guard::Allow
    }

    /// A tool finished. Hooks chain: each sees the previous one's edits.
    fn on_tool_result(&self, _outcome: &mut ToolOutcome, _cx: &ResultCx<'_>) {}

    /// A turn is complete and its results are committed.
    fn on_turn_end(&self, _cx: &mut TurnCx<'_>, _turn: &TurnSummary) -> Flow {
        Flow::Continue
    }

    /// The run has stopped.
    fn on_run_end(&self, _cx: &mut RunCx<'_>, _outcome: &crate::RunOutcome) {}
}

/// A [`Plugin`] wrapping a closure over the streaming hook, for callers that
/// only want to render.
pub struct EventListener<F> {
    name: &'static str,
    listener: F,
}

impl<F> EventListener<F>
where
    F: Fn(&Event, &StreamCx<'_>) + Send + Sync + 'static,
{
    #[must_use]
    pub fn new(listener: F) -> Self {
        Self {
            name: "event-listener",
            listener,
        }
    }
}

impl<F> Plugin for EventListener<F>
where
    F: Fn(&Event, &StreamCx<'_>) + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        self.name
    }

    fn interests(&self) -> Interest {
        Interest::EVENT
    }

    fn on_event(&self, event: &Event, cx: &StreamCx<'_>) {
        (self.listener)(event, cx);
    }
}
