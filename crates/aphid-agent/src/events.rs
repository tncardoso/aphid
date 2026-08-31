//! The events the agent loop announces.
//!
//! Each one names the moment, the mode it dispatches in, and what a listener
//! may change. Together they are the whole of what the loop offers a component:
//! there is no second surface and no separate trait to keep in step.
//!
//! # Why payloads own their data
//!
//! An event type is `'static`, and the loop's own contexts borrow the
//! transcript mutably, so a payload cannot simply carry a reference to it.
//! Instead a payload that wants to change the run **records** what it wants —
//! a note to append, a prompt to queue, a cancellation — and the loop applies
//! the record once dispatch returns. Listeners can then run wherever they like,
//! and two of them asking for conflicting edits resolve in the order they ran
//! rather than by whoever held the borrow.
//!
//! The one thing not shaped this way is the per-token stream, which is
//! documented where it lives: [`crate::rt::Snapshot`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aphid_core::{MessageId, Model, StopReason, Usage};

use crate::RunOutcome;
use crate::plugin::TurnSummary;
use crate::rt::{Emitted, Event, Failure, Scope, Waterfalled};
use crate::tool::ToolContent;

/// Something a listener asked the loop to do once dispatch returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Append a system message at the tail of the transcript.
    Note(String),
    /// Append a user message at the tail of the transcript.
    User(String),
}

/// The handle a run-scoped payload carries.
///
/// Holds no borrow, so a listener may keep it, move it to another thread, or
/// answer from a task. The transcript only ever grows, so this adds rather than
/// rewrites — which is also what makes a run replayable from its transcript.
#[derive(Clone)]
pub struct Run {
    pub model: Model,
    /// Zero-based index of the turn about to start, or that just ended.
    pub turn: u32,
    /// Tokens and cost accumulated so far.
    pub usage: Usage,
    edits: Arc<Mutex<Vec<Edit>>>,
    cancel: Arc<AtomicBool>,
}

impl Run {
    pub(crate) fn new(model: Model, turn: u32, usage: Usage, cancel: Arc<AtomicBool>) -> Run {
        Run {
            model,
            turn,
            usage,
            edits: Arc::default(),
            cancel,
        }
    }

    /// Append a system message at the tail of the transcript.
    pub fn note(&self, text: impl Into<String>) {
        self.record(Edit::Note(text.into()));
    }

    /// Append a user message at the tail of the transcript.
    pub fn push_user(&self, text: impl Into<String>) {
        self.record(Edit::User(text.into()));
    }

    /// Ask the run to stop at the next checkpoint.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn record(&self, edit: Edit) {
        if let Ok(mut edits) = self.edits.lock() {
            edits.push(edit);
        }
    }

    pub(crate) fn take_edits(&self) -> Vec<Edit> {
        self.edits
            .lock()
            .map(|mut edits| std::mem::take(&mut *edits))
            .unwrap_or_default()
    }
}

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run")
            .field("model", &self.model.id)
            .field("turn", &self.turn)
            .finish_non_exhaustive()
    }
}

/// A prompt on its way into the transcript.
///
/// The one payload that can rewrite rather than only add, because nothing has
/// been committed yet.
#[derive(Debug)]
pub struct Prompt {
    pub text: String,
    rejected: Option<String>,
}

impl Prompt {
    pub(crate) fn new(text: String) -> Prompt {
        Prompt {
            text,
            rejected: None,
        }
    }

    /// Drop the prompt and end the run before a request is sent. Nothing is
    /// appended, so a rejected prompt leaves the conversation as it was.
    ///
    /// The first rejection is the one reported; later listeners still run,
    /// because an observer wants to see a prompt somebody else turned away.
    pub fn reject(&mut self, reason: impl Into<String>) {
        if self.rejected.is_none() {
            self.rejected = Some(reason.into());
        }
    }

    #[must_use]
    pub fn rejection(&self) -> Option<&str> {
        self.rejected.as_deref()
    }
}

impl Event for Prompt {
    const NAME: &'static str = "agent/prompt";
}
impl Emitted for Prompt {}

/// A run is about to start, after the prompt has been appended.
#[derive(Debug)]
pub struct RunStart(pub Run);
impl Event for RunStart {
    const NAME: &'static str = "agent/run-start";
}
impl Emitted for RunStart {}

/// A request is about to be sent. The last chance to add context.
#[derive(Debug)]
pub struct TurnStart(pub Run);
impl Event for TurnStart {
    const NAME: &'static str = "agent/turn-start";
}
impl Emitted for TurnStart {}

/// An assistant message has been committed, before its tool calls are read.
#[derive(Debug)]
pub struct Message {
    pub run: Run,
    pub message: MessageId,
}
impl Event for Message {
    const NAME: &'static str = "agent/message";
}
impl Emitted for Message {}

/// A turn is complete and its results are committed.
#[derive(Debug)]
pub struct TurnEnd {
    pub run: Run,
    pub summary: TurnSummary,
    /// Set by a listener that wants the run to stop cleanly after this turn.
    /// Any one listener asking is enough.
    pub stop: bool,
}
impl Event for TurnEnd {
    const NAME: &'static str = "agent/turn-end";
}
impl Emitted for TurnEnd {}

/// The run has stopped.
#[derive(Debug)]
pub struct RunEnd {
    pub run: Run,
    pub stop: StopReason,
    pub turns: u32,
    pub error: Option<String>,
}
impl RunEnd {
    pub(crate) fn new(run: Run, outcome: &RunOutcome) -> RunEnd {
        RunEnd {
            run,
            stop: outcome.stop,
            turns: outcome.turns,
            error: outcome.error.clone(),
        }
    }
}
impl Event for RunEnd {
    const NAME: &'static str = "agent/run-end";
}
impl Emitted for RunEnd {}

/// A tool call that has been requested but not yet run.
///
/// Broadcast rather than bailed, and the difference matters: **every** listener
/// runs even after one has refused, because an observer still wants to see a
/// call somebody else blocked. The first refusal is the one that takes effect.
///
/// Failure is closed. A guard that raised has not agreed to anything, and
/// running the tool anyway would defeat the only reason people write one.
#[derive(Clone, Debug)]
pub struct ToolRequest {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// Whether a tool is registered under this name.
    pub known: bool,
    /// Set by the first listener to refuse. Later ones see it and can leave it
    /// alone; [`ToolRequest::refuse`] does that for you.
    pub blocked: Option<Blocked>,
}

impl ToolRequest {
    /// Refuse the call, unless somebody already did.
    pub fn refuse(&mut self, blocked: Blocked) {
        if self.blocked.is_none() {
            self.blocked = Some(blocked);
        }
    }

    /// Whether an earlier listener already refused.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.blocked.is_some()
    }
}

/// Why a tool call did not run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Blocked {
    pub reason: String,
    /// Ask the run to stop after this batch. Honoured only when every result in
    /// the batch asks for it.
    pub terminate: bool,
}

impl Blocked {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Blocked {
        Blocked {
            reason: reason.into(),
            terminate: false,
        }
    }

    #[must_use]
    pub fn and_stop(mut self) -> Blocked {
        self.terminate = true;
        self
    }
}

impl Event for ToolRequest {
    const NAME: &'static str = "agent/tool-call";
    const FAILURE: Failure = Failure::Closed;
}
impl Emitted for ToolRequest {}

/// A tool call's arguments, on their way to the handler.
///
/// Separate from [`ToolRequest`] because rewriting arguments and refusing the call
/// are different decisions: a waterfall transforms, a bail answers. Keeping
/// them apart means a listener that only rewrites cannot accidentally block.
#[derive(Debug)]
pub struct ToolArguments;
impl Event for ToolArguments {
    const NAME: &'static str = "agent/tool-arguments";
}
impl Waterfalled for ToolArguments {
    type In = String;
    type Out = String;
}

/// A running tool published partial output.
///
/// Fires from the tool's own task, possibly while sibling tools are still
/// running, so chunks from different calls interleave — `call_id` is what tells
/// them apart. These are for showing progress, not for accumulating the answer.
#[derive(Clone, Debug)]
pub struct ToolProgress {
    pub call_id: String,
    pub tool: String,
    pub chunk: String,
}
impl Event for ToolProgress {
    const NAME: &'static str = "agent/tool-progress";
}
impl Emitted for ToolProgress {}

/// A tool finished. Listeners chain: each sees the previous one's edits.
#[derive(Debug)]
pub struct ToolResult {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub turn: u32,
    pub content: Vec<ToolContent>,
    pub is_error: bool,
    /// Structured detail a front end may render, opaque to the model.
    pub details: Option<serde_json::Value>,
}
impl Event for ToolResult {
    const NAME: &'static str = "agent/tool-result";
}
impl Emitted for ToolResult {}

/// Every event this crate announces.
///
/// Declared in one place so that a component subscribing to a name nobody
/// emits is told, rather than waiting quietly for something that never comes.
pub const AGENT_EVENTS: &[&str] = &[
    Prompt::NAME,
    RunStart::NAME,
    TurnStart::NAME,
    Message::NAME,
    TurnEnd::NAME,
    RunEnd::NAME,
    ToolRequest::NAME,
    ToolArguments::NAME,
    ToolProgress::NAME,
    ToolResult::NAME,
];

// ---------------------------------------------------------- the arena borrows
//
// Two dispatches do not go through the bus, and both for the same reason: what
// they hand a listener is a **borrow into the core's arenas**. A bus event is
// `'static`, and copying an arena out to satisfy that would undo the layout the
// core exists to provide.
//
// So each is a flat list of listeners with a higher-ranked signature, and the
// loop takes a snapshot of the list once — per stream, per run — rather than
// per item. Reading a snapshot costs nothing and the writer never blocks on it.
//
// The rule, for anything added later: **an arena borrow is not a bus event.**
// If somebody tidies these onto the typed bus they will copy a transcript per
// turn and add a hash lookup and a downcast to every token of every response.
// Neither is an oversight.

use crate::plugin::StreamCx;
use crate::rt::Uid;
use std::sync::RwLock;

type StreamListener = Arc<dyn for<'a> Fn(&aphid_core::Event, &StreamCx<'a>) + Send + Sync>;

/// Where a run-scoped listener is being called from.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Moment {
    /// The prompt has been appended and the run is about to start.
    RunStart,
    /// An assistant message has just been committed.
    Message,
    /// A turn's results are committed.
    TurnEnd,
    /// The run has stopped.
    RunEnd,
}

type TranscriptListener =
    Arc<dyn for<'t> Fn(Moment, &'t aphid_core::Transcript, &Run) + Send + Sync>;

/// Listeners that need to read the transcript at the moment it grew.
///
/// The transcript is append-only and not cloneable, so this is the shape that
/// serves a component wanting to persist, index or summarise it: called where
/// the growth happened, handed the real thing, holding it no longer than the
/// call. See the note on arena borrows above.
#[derive(Default)]
pub struct TranscriptListeners {
    listeners: RwLock<Vec<(Uid, Scope, TranscriptListener)>>,
    current: RwLock<Arc<Vec<(Scope, TranscriptListener)>>>,
}

impl TranscriptListeners {
    #[must_use]
    pub fn new() -> TranscriptListeners {
        TranscriptListeners::default()
    }

    /// Read every transcript that grows, whatever session it belongs to.
    pub fn subscribe(
        &self,
        owner: Uid,
        listener: impl for<'t> Fn(Moment, &'t aphid_core::Transcript, &Run) + Send + Sync + 'static,
    ) {
        self.subscribe_scoped(None, owner, listener);
    }

    /// Read one session's transcript only. See [`Scope`].
    pub fn subscribe_scoped(
        &self,
        scope: Scope,
        owner: Uid,
        listener: impl for<'t> Fn(Moment, &'t aphid_core::Transcript, &Run) + Send + Sync + 'static,
    ) {
        if let Ok(mut listeners) = self.listeners.write() {
            listeners.push((owner, scope, Arc::new(listener)));
        }
        self.republish();
    }

    pub fn unsubscribe(&self, owner: Uid) {
        if let Ok(mut listeners) = self.listeners.write() {
            listeners.retain(|(uid, _, _)| *uid != owner);
        }
        self.republish();
    }

    fn republish(&self) {
        let next: Vec<(Scope, TranscriptListener)> = self
            .listeners
            .read()
            .map(|listeners| {
                listeners
                    .iter()
                    .map(|(_, s, l)| (s.clone(), Arc::clone(l)))
                    .collect()
            })
            .unwrap_or_default();
        if let Ok(mut current) = self.current.write() {
            *current = Arc::new(next);
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<Vec<TranscriptListener>> {
        self.current
            .read()
            .map(|current| Arc::new(current.iter().map(|(_, l)| Arc::clone(l)).collect()))
            .unwrap_or_default()
    }

    #[must_use]
    pub fn is_observed(&self) -> bool {
        self.current
            .read()
            .map(|current| !current.is_empty())
            .unwrap_or(false)
    }

    pub(crate) fn announce(
        &self,
        scope: &Scope,
        moment: Moment,
        transcript: &aphid_core::Transcript,
        run: &Run,
    ) {
        for (listener_scope, listener) in self.snapshot_pairs().iter() {
            if listener_scope.is_none() || listener_scope.as_deref() == scope.as_deref() {
                listener(moment, transcript, run);
            }
        }
    }

    fn snapshot_pairs(&self) -> Arc<Vec<(Scope, TranscriptListener)>> {
        self.current
            .read()
            .map(|current| Arc::clone(&current))
            .unwrap_or_default()
    }
}

impl std::fmt::Debug for TranscriptListeners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .current
            .read()
            .map(|current| current.len())
            .unwrap_or(0);
        f.debug_struct("TranscriptListeners")
            .field("count", &count)
            .finish()
    }
}

/// The subscribers to the token stream, and the snapshot the loop holds.
#[derive(Default)]
pub struct StreamListeners {
    listeners: RwLock<Vec<(Uid, Scope, StreamListener)>>,
    /// Republished on every change, so the read path never takes a lock it
    /// might contend on. Each entry carries its scope, so a turn can take the
    /// listeners that are its own without holding a lock.
    current: RwLock<Arc<Vec<(Scope, StreamListener)>>>,
}

impl StreamListeners {
    #[must_use]
    pub fn new() -> StreamListeners {
        StreamListeners::default()
    }

    /// Watch every stream, whatever session it belongs to.
    pub fn subscribe(
        &self,
        owner: Uid,
        listener: impl for<'a> Fn(&aphid_core::Event, &StreamCx<'a>) + Send + Sync + 'static,
    ) {
        self.subscribe_scoped(None, owner, listener);
    }

    /// Watch one session's streams only. See [`Scope`].
    pub fn subscribe_scoped(
        &self,
        scope: Scope,
        owner: Uid,
        listener: impl for<'a> Fn(&aphid_core::Event, &StreamCx<'a>) + Send + Sync + 'static,
    ) {
        if let Ok(mut listeners) = self.listeners.write() {
            listeners.push((owner, scope, Arc::new(listener)));
        }
        self.republish();
    }

    pub fn unsubscribe(&self, owner: Uid) {
        if let Ok(mut listeners) = self.listeners.write() {
            listeners.retain(|(uid, _, _)| *uid != owner);
        }
        self.republish();
    }

    fn republish(&self) {
        let next: Vec<(Scope, StreamListener)> = self
            .listeners
            .read()
            .map(|listeners| {
                listeners
                    .iter()
                    .map(|(_, s, l)| (s.clone(), Arc::clone(l)))
                    .collect()
            })
            .unwrap_or_default();
        if let Ok(mut current) = self.current.write() {
            *current = Arc::new(next);
        }
    }

    /// Take the subscriber list for one stream. Called once per turn, not once
    /// per token, and filtered to the turn's scope so one session never sees
    /// another's tokens.
    #[must_use]
    pub fn snapshot(&self, scope: &Scope) -> Arc<Vec<StreamListener>> {
        self.current
            .read()
            .map(|current| {
                Arc::new(
                    current
                        .iter()
                        .filter(|(listener_scope, _)| {
                            listener_scope.is_none()
                                || listener_scope.as_deref() == scope.as_deref()
                        })
                        .map(|(_, l)| Arc::clone(l))
                        .collect(),
                )
            })
            .unwrap_or_default()
    }

    /// Whether anybody is watching. Checked once per stream, so a response
    /// nobody observes skips the dispatch entirely.
    #[must_use]
    pub fn is_observed(&self) -> bool {
        self.current
            .read()
            .map(|current| !current.is_empty())
            .unwrap_or(false)
    }
}

impl std::fmt::Debug for StreamListeners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .current
            .read()
            .map(|current| current.len())
            .unwrap_or(0);
        f.debug_struct("StreamListeners")
            .field("count", &count)
            .finish()
    }
}
