//! Where the terminal's messages come from.
//!
//! The agent's hooks are synchronous, so they push onto the hub — a
//! non-blocking, infallible operation, which is precisely why the plugin API
//! was built that way. Terminal input arrives on its own thread, because
//! `crossterm::event::read` blocks.

use aphid_agent::{
    Cx, Flow, Guard, Interest, PendingCall, Plugin, ResultCx, StreamCx, ToolOutcome, TurnSummary,
};
use aphid_core::{BlockKind, ContentRef, Event};
use ratatui::crossterm::event;

use crate::plugins::permissions::{Confirmer, Decision, Risk};
use crate::tui::msg::Msg;
use crate::tui::runtime::{self, ANSWER_TIMEOUT, Answers, Hub};

/// Sends a plugin's `notify` and `prompt` output to the app loop.
///
/// The terminal UI owns the screen, so a plugin cannot simply print: its output
/// has to arrive as an event like everything else. `log` still goes to standard
/// error, where the UI is not drawing and a developer can capture it.
pub struct UiSink {
    events: Hub<Msg>,
}

impl UiSink {
    #[must_use]
    pub fn new(events: Hub<Msg>) -> Self {
        Self { events }
    }
}

impl aphid_plugin::Sink for UiSink {
    fn notify(&self, plugin: &str, text: &str) {
        let _ = self.events.send(Msg::Notice(format!("{plugin}: {text}")));
    }

    fn prompt(&self, _plugin: &str, text: &str) {
        self.events.send(Msg::Prompt(text.to_owned()));
    }
}

/// Forwards the run to the app loop.
///
/// Not the end of it, though: the run's own task says that, because only it
/// knows that the agent has been handed back. A hook cannot know, because it
/// runs inside the run.
pub struct UiPlugin {
    events: Hub<Msg>,
}

impl UiPlugin {
    #[must_use]
    pub fn new(events: Hub<Msg>) -> Self {
        Self { events }
    }

    fn send(&self, event: Msg) {
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
    }

    fn on_turn_start(&self, _cx: &mut Cx<'_>) {
        self.send(Msg::TurnStarted);
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
                self.send(Msg::ToolStreamStart { block: index, name });
            }
            Event::Delta {
                index,
                kind: BlockKind::ToolCall,
                span,
            } => self.send(Msg::ToolStreamDelta {
                block: index,
                bytes: span.len() as usize,
            }),
            Event::Delta {
                kind: BlockKind::Text,
                span,
                ..
            } => self.send(Msg::Text(cx.text(span).to_owned())),
            Event::Delta {
                kind: BlockKind::Thinking,
                span,
                ..
            } => self.send(Msg::Thinking(cx.text(span).to_owned())),
            _ => {}
        }
    }

    fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
        self.send(Msg::ToolCall {
            id: call.id().to_owned(),
            name: call.name().to_owned(),
            arguments: call.arguments().to_owned(),
        });
        Guard::Allow
    }

    fn on_tool_progress(&self, call_id: &str, _tool: &str, chunk: &str) {
        self.send(Msg::ToolProgress {
            id: call_id.to_owned(),
            chunk: chunk.to_owned(),
        });
    }

    fn on_tool_result(&self, outcome: &mut ToolOutcome, cx: &ResultCx<'_>) {
        self.send(Msg::ToolResult {
            id: cx.id().to_owned(),
            name: cx.name().to_owned(),
            text: outcome.text_content(),
            is_error: outcome.is_error,
            details: outcome.details.clone(),
        });
    }

    fn on_turn_end(&self, _cx: &mut Cx<'_>, turn: &TurnSummary) -> Flow {
        self.send(Msg::TurnEnded {
            usage: turn.usage,
            stop: turn.stop_reason,
            error: turn.error.clone(),
        });
        Flow::Continue
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
    events: Hub<Msg>,
    answers: Answers<Decision>,
}

impl UiConfirmer {
    #[must_use]
    pub fn new(events: Hub<Msg>, answers: Answers<Decision>) -> Self {
        Self { events, answers }
    }
}

impl Confirmer for UiConfirmer {
    fn confirm(&self, tool: &str, summary: &str, risk: Risk) -> Decision {
        let (id, answer) = self.answers.open();
        let asked = self.events.send(Msg::Confirm {
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

/// Read the terminal, and say what each event means to this app.
///
/// The thread is the runtime's; what is here is the mapping, because what a
/// mouse wheel or a paste means is the app's own business. The other two
/// terminals write the same few lines and answer differently.
pub fn spawn_input_thread(hub: &Hub<Msg>) {
    runtime::spawn_input_thread(hub.clone(), |event| match event {
        event::Event::Key(key) => Some(Msg::Key(key)),
        event::Event::Paste(text) => Some(Msg::Paste(text)),
        event::Event::Mouse(mouse) => Some(Msg::Mouse(mouse)),
        event::Event::Resize(_, _) => Some(Msg::Resize),
        _ => None,
    });
}
