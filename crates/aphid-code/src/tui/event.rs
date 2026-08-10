//! What the UI reacts to, and where it comes from.
//!
//! Two producers, one channel. The agent's hooks are synchronous, so they push
//! onto an unbounded sender — a non-blocking, infallible operation, which is
//! precisely why the plugin API was built that way. Terminal input arrives on
//! its own thread, because `crossterm::event::read` blocks.

use std::sync::mpsc::Sender as Reply;

use aphid_agent::{
    Cx, Flow, Guard, Interest, PendingCall, Plugin, ResultCx, RunOutcome, StreamCx, ToolOutcome,
    TurnSummary,
};
use aphid_core::{BlockKind, Event, Json, StopReason, Usage};
use ratatui::crossterm::event::{self, KeyEvent};
use tokio::sync::mpsc::UnboundedSender;

use crate::plugins::permissions::{Confirmer, Decision, Risk};

/// Everything the app loop reacts to.
pub enum UiEvent {
    TurnStarted,
    /// A chunk of assistant prose.
    Text(String),
    /// A chunk of reasoning.
    Thinking(String),
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
    /// A gated tool is waiting for an answer. The agent's task blocks on `reply`
    /// until the app sends one.
    Confirm {
        tool: String,
        summary: String,
        risk: Risk,
        reply: Reply<Decision>,
    },
    Key(KeyEvent),
    Resize,
}

/// Forwards the run to the app loop.
pub struct UiPlugin {
    events: UnboundedSender<UiEvent>,
}

impl UiPlugin {
    #[must_use]
    pub fn new(events: UnboundedSender<UiEvent>) -> Self {
        Self { events }
    }

    fn send(&self, event: UiEvent) {
        // A closed channel means the app is gone; there is nothing to do about
        // it here, and the run is being cancelled anyway.
        let _ = self.events.send(event);
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
        // Tool-call arguments stream as deltas too. They are shown in full when
        // the call is announced, so streaming raw JSON here would only be noise.
        let Event::Delta { kind, span, .. } = *event else {
            return;
        };
        match kind {
            BlockKind::Text => self.send(UiEvent::Text(cx.text(span).to_owned())),
            BlockKind::Thinking => self.send(UiEvent::Thinking(cx.text(span).to_owned())),
            BlockKind::ToolCall | BlockKind::Image => {}
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

/// Asks through the UI and blocks the agent's task until an answer arrives.
///
/// This is why the agent runs on its own task rather than inside the app's
/// `select!`: blocking here must not stop the loop that draws the prompt.
pub struct UiConfirmer {
    events: UnboundedSender<UiEvent>,
}

impl UiConfirmer {
    #[must_use]
    pub fn new(events: UnboundedSender<UiEvent>) -> Self {
        Self { events }
    }
}

impl Confirmer for UiConfirmer {
    fn confirm(&self, tool: &str, summary: &str, risk: Risk) -> Decision {
        let (reply, answer) = std::sync::mpsc::channel();
        if self
            .events
            .send(UiEvent::Confirm {
                tool: tool.to_owned(),
                summary: summary.to_owned(),
                risk,
                reply,
            })
            .is_err()
        {
            return Decision::Deny;
        }
        // A dropped sender means the app quit without answering.
        answer.recv().unwrap_or(Decision::Deny)
    }
}

/// Read the terminal on a dedicated thread and forward what it says.
///
/// `crossterm::event::read` blocks, and a blocked runtime thread is a stalled
/// UI. The thread ends when the channel closes.
pub fn spawn_input_thread(events: UnboundedSender<UiEvent>) {
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(event::Event::Key(key)) => {
                    if events.send(UiEvent::Key(key)).is_err() {
                        return;
                    }
                }
                Ok(event::Event::Resize(_, _)) => {
                    if events.send(UiEvent::Resize).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });
}
