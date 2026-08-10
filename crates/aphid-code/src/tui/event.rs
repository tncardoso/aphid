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
use aphid_core::{BlockKind, ContentRef, Event, Json, StopReason, Usage};
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
    /// A gated tool is waiting for an answer. The agent's task blocks on `reply`
    /// until the app sends one.
    Confirm {
        tool: String,
        summary: String,
        risk: Risk,
        reply: Reply<Decision>,
    },
    /// Something a plugin wants the user to see.
    Notice(String),
    Key(KeyEvent),
    Resize,
}

/// Sends a plugin's `notify` output to the app loop.
///
/// The terminal UI owns the screen, so a plugin cannot simply print: its output
/// has to arrive as an event like everything else. `log` still goes to standard
/// error, where the UI is not drawing and a developer can capture it.
pub struct UiSink {
    events: UnboundedSender<UiEvent>,
}

impl UiSink {
    #[must_use]
    pub fn new(events: UnboundedSender<UiEvent>) -> Self {
        Self { events }
    }
}

impl aphid_plugin::Sink for UiSink {
    fn notify(&self, plugin: &str, text: &str) {
        let _ = self
            .events
            .send(UiEvent::Notice(format!("{plugin}: {text}")));
    }
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
