//! What the UI reacts to, and where it comes from.
//!
//! Two producers, one channel. The agent's hooks are synchronous, so they push
//! onto an unbounded sender — a non-blocking, infallible operation, which is
//! precisely why the plugin API was built that way. Terminal input arrives on
//! its own thread, because `crossterm::event::read` blocks.

use aphid_agent::{
    Cx, Flow, Guard, Interest, PendingCall, Plugin, ResultCx, RunOutcome, StreamCx, ToolOutcome,
    TurnSummary,
};
use aphid_core::{BlockKind, ContentRef, Event, Json, StopReason, Usage};
use ratatui::crossterm::event::{self, KeyEvent, MouseEvent};

use crate::plugins::permissions::{Confirmer, Decision, Risk};
use crate::tui::runtime::{self, ANSWER_TIMEOUT, Answers, Hub, RequestId};

/// Everything the app loop reacts to.
pub enum UiEvent {
    TurnStarted,
    /// A chunk of assistant prose.
    Text(String),
    /// A chunk of reasoning.
    Thinking(String),
    /// A tool-call block opened. Its arguments are still arriving, and the call
    /// itself is not announced until the whole turn has been committed.
    ToolStreamStart {
        block: u32,
        name: String,
    },
    /// More argument bytes landed in the block at `block`.
    ToolStreamDelta {
        block: u32,
        bytes: usize,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolProgress {
        id: String,
        chunk: String,
    },
    ToolResult {
        id: String,
        name: String,
        text: String,
        is_error: bool,
        details: Option<Json>,
    },
    TurnEnded {
        usage: Usage,
        stop: StopReason,
        error: Option<String>,
    },
    RunEnded(Box<RunOutcome>),
    /// A gated tool is waiting for an answer. The agent's task is blocked on
    /// the channel this id names, and stays blocked until the app answers it.
    Confirm {
        id: RequestId,
        tool: String,
        summary: String,
        risk: Risk,
    },
    /// Something a plugin wants the user to see.
    Notice(String),
    /// Text a plugin sent to the model, which is queued as a typed line is.
    Prompt(String),
    Key(KeyEvent),
    /// A block of text the terminal delivered in one piece, under bracketed
    /// paste. Its newlines are text, not the Enters they would be if the same
    /// characters had been typed.
    Paste(String),
    /// A mouse button, drag or wheel event. The app decides who it belongs to.
    Mouse(MouseEvent),
    Resize,
}

/// Sends a plugin's `notify` and `prompt` output to the app loop.
///
/// The terminal UI owns the screen, so a plugin cannot simply print: its output
/// has to arrive as an event like everything else. `log` still goes to standard
/// error, where the UI is not drawing and a developer can capture it.
pub struct UiSink {
    events: Hub<UiEvent>,
}

impl UiSink {
    #[must_use]
    pub fn new(events: Hub<UiEvent>) -> Self {
        Self { events }
    }
}

impl aphid_plugin::Sink for UiSink {
    fn notify(&self, plugin: &str, text: &str) {
        let _ = self
            .events
            .send(UiEvent::Notice(format!("{plugin}: {text}")));
    }

    fn prompt(&self, _plugin: &str, text: &str) {
        self.events.send(UiEvent::Prompt(text.to_owned()));
    }
}

/// Forwards the run to the app loop.
pub struct UiPlugin {
    events: Hub<UiEvent>,
}

impl UiPlugin {
    #[must_use]
    pub fn new(events: Hub<UiEvent>) -> Self {
        Self { events }
    }

    fn send(&self, event: UiEvent) {
        // A closed channel means the app is gone; there is nothing to do about
        // it here, and the run is being cancelled anyway.
        self.events.send(event);
    }
}

impl Plugin for UiPlugin {
    fn name(&self) -> &str {
        "tui"
    }

    fn interests(&self) -> Interest {
        Interest::TURN_START
            | Interest::EVENT
            | Interest::TOOL_CALL
            | Interest::TOOL_PROGRESS
            | Interest::TOOL_RESULT
            | Interest::TURN_END
            | Interest::RUN_END
    }

    fn on_turn_start(&self, _cx: &mut Cx<'_>) {
        self.send(UiEvent::TurnStarted);
    }

    fn on_event(&self, event: &Event, cx: &StreamCx<'_>) {
        match *event {
            // Tool-call arguments stream as deltas too. They are shown in full
            // when the call is announced, so raw JSON would only be noise here
            // — the bytes are counted instead, which is what proves to the user
            // that a slow call is moving rather than stuck.
            Event::BlockStart {
                index,
                kind: BlockKind::ToolCall,
            } => {
                let name = tool_name(cx, index);
                self.send(UiEvent::ToolStreamStart { block: index, name });
            }
            Event::Delta {
                index,
                kind: BlockKind::ToolCall,
                span,
            } => self.send(UiEvent::ToolStreamDelta {
                block: index,
                bytes: span.len() as usize,
            }),
            Event::Delta {
                kind: BlockKind::Text,
                span,
                ..
            } => self.send(UiEvent::Text(cx.text(span).to_owned())),
            Event::Delta {
                kind: BlockKind::Thinking,
                span,
                ..
            } => self.send(UiEvent::Thinking(cx.text(span).to_owned())),
            _ => {}
        }
    }

    fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
        self.send(UiEvent::ToolCall {
            id: call.id().to_owned(),
            name: call.name().to_owned(),
            arguments: call.arguments().to_owned(),
        });
        Guard::Allow
    }

    fn on_tool_progress(&self, call_id: &str, _tool: &str, chunk: &str) {
        self.send(UiEvent::ToolProgress {
            id: call_id.to_owned(),
            chunk: chunk.to_owned(),
        });
    }

    fn on_tool_result(&self, outcome: &mut ToolOutcome, cx: &ResultCx<'_>) {
        self.send(UiEvent::ToolResult {
            id: cx.id().to_owned(),
            name: cx.name().to_owned(),
            text: outcome.text_content(),
            is_error: outcome.is_error,
            details: outcome.details.clone(),
        });
    }

    fn on_turn_end(&self, _cx: &mut Cx<'_>, turn: &TurnSummary) -> Flow {
        self.send(UiEvent::TurnEnded {
            usage: turn.usage,
            stop: turn.stop_reason,
            error: turn.error.clone(),
        });
        Flow::Continue
    }

    fn on_run_end(&self, _cx: &mut Cx<'_>, outcome: &RunOutcome) {
        self.send(UiEvent::RunEnded(Box::new(outcome.clone())));
    }
}

/// The name of the tool call in the block at `index`.
///
/// The name is already staged when the block opens — a provider identifies a
/// call on its first delta — so this reads it back out of the partial message
/// rather than waiting for the call to be announced.
fn tool_name(cx: &StreamCx<'_>, index: u32) -> String {
    let named = match cx.partial().content().nth(index as usize) {
        Some(ContentRef::ToolCall(call)) => call.name(),
        _ => "",
    };
    if named.is_empty() {
        "tool".to_owned()
    } else {
        named.to_owned()
    }
}

/// Asks through the UI and blocks the agent's task until an answer arrives.
///
/// This is why the agent runs on its own task rather than inside the app's
/// `select!`: blocking here must not stop the loop that draws the prompt.
pub struct UiConfirmer {
    events: Hub<UiEvent>,
    answers: Answers<Decision>,
}

impl UiConfirmer {
    #[must_use]
    pub fn new(events: Hub<UiEvent>, answers: Answers<Decision>) -> Self {
        Self { events, answers }
    }
}

impl Confirmer for UiConfirmer {
    fn confirm(&self, tool: &str, summary: &str, risk: Risk) -> Decision {
        let (id, answer) = self.answers.open();
        let asked = self.events.send(UiEvent::Confirm {
            id,
            tool: tool.to_owned(),
            summary: summary.to_owned(),
            risk,
        });
        if !asked {
            // Nobody is left to ask, so nobody can allow it.
            self.answers.abandon(id);
            return Decision::Deny;
        }
        // A dropped sender means the app quit without answering; the timeout
        // means whoever it asked walked away. Both refuse.
        answer
            .recv_timeout(ANSWER_TIMEOUT)
            .unwrap_or(Decision::Deny)
    }
}

/// Read the terminal on a dedicated thread and forward what it says.
///
/// `crossterm::event::read` blocks, and a blocked runtime thread is a stalled
/// UI. The thread ends when the channel closes.
pub fn spawn_input_thread(hub: &Hub<UiEvent>) {
    runtime::spawn_input_thread(hub.clone(), |event| match event {
        event::Event::Key(key) => Some(UiEvent::Key(key)),
        event::Event::Paste(text) => Some(UiEvent::Paste(text)),
        event::Event::Mouse(mouse) => Some(UiEvent::Mouse(mouse)),
        event::Event::Resize(_, _) => Some(UiEvent::Resize),
        _ => None,
    });
}
