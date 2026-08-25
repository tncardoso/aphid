//! Where the terminal's messages come from.
//!
//! Listeners are synchronous, so they push onto the hub — a non-blocking,
//! infallible operation, which is precisely why the surface was built that way.
//! Terminal input arrives on its own thread, because `crossterm::event::read`
//! blocks.

use std::sync::Arc;

use aphid_agent::rt::{Bus, Component, Composition, Context, Disposer};
use aphid_agent::{
    StreamCx, StreamListeners, ToolContent, ToolProgress, ToolRequest, ToolResult, TurnEnd,
    TurnStart,
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

impl aphid_agent::Sink for UiSink {
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
/// knows that the agent has been handed back. A listener cannot know, because
/// it runs inside the run.
pub struct UiComponent {
    events: Hub<Msg>,
    bus: Arc<Bus>,
    stream: Arc<StreamListeners>,
}

impl UiComponent {
    #[must_use]
    pub fn new(events: Hub<Msg>, composition: &Composition) -> Self {
        Self {
            events,
            bus: Arc::clone(&composition.bus),
            stream: Arc::clone(&composition.stream),
        }
    }
}

impl Component for UiComponent {
    fn name(&self) -> &str {
        "tui"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let owner = ctx.uid();
        let bus = Arc::clone(&self.bus);

        let events = self.events.clone();
        bus.on::<TurnStart>(owner, move |_| {
            let _ = events.send(Msg::TurnStarted);
        });

        let events = self.events.clone();
        bus.on::<ToolRequest>(owner, move |request| {
            let _ = events.send(Msg::ToolCall {
                id: request.id.clone(),
                name: request.name.clone(),
                arguments: request.arguments.clone(),
            });
        });

        let events = self.events.clone();
        bus.on::<ToolProgress>(owner, move |progress| {
            let _ = events.send(Msg::ToolProgress {
                id: progress.call_id.clone(),
                chunk: progress.chunk.clone(),
            });
        });

        let events = self.events.clone();
        bus.on::<ToolResult>(owner, move |result| {
            let _ = events.send(Msg::ToolResult {
                id: result.id.clone(),
                name: result.name.clone(),
                text: text_of(&result.content),
                is_error: result.is_error,
                details: result.details.clone(),
            });
        });

        let events = self.events.clone();
        bus.on::<TurnEnd>(owner, move |end| {
            let _ = events.send(Msg::TurnEnded {
                usage: end.summary.usage,
                stop: end.summary.stop_reason,
                error: end.summary.error.clone(),
            });
        });

        // The token stream, which is not on the bus: what it hands out borrows
        // the response arena, and copying that out is the one thing the core
        // exists to avoid.
        let events = self.events.clone();
        self.stream.subscribe(owner, move |event, cx| {
            match *event {
                // Tool-call arguments stream as deltas too. They are shown in
                // full when the call is announced, so raw JSON would only be
                // noise here — the bytes are counted instead, which is what
                // proves to the user that a slow call is moving rather than
                // stuck.
                Event::BlockStart {
                    index,
                    kind: BlockKind::ToolCall,
                } => {
                    let name = tool_name(cx, index);
                    let _ = events.send(Msg::ToolStreamStart { block: index, name });
                }
                Event::Delta {
                    index,
                    kind: BlockKind::ToolCall,
                    span,
                } => {
                    let _ = events.send(Msg::ToolStreamDelta {
                        block: index,
                        bytes: span.len() as usize,
                    });
                }
                Event::Delta {
                    kind: BlockKind::Text,
                    span,
                    ..
                } => {
                    let _ = events.send(Msg::Text(cx.text(span).to_owned()));
                }
                Event::Delta {
                    kind: BlockKind::Thinking,
                    span,
                    ..
                } => {
                    let _ = events.send(Msg::Thinking(cx.text(span).to_owned()));
                }
                _ => {}
            }
        });

        let bus = Arc::clone(&self.bus);
        let stream = Arc::clone(&self.stream);
        ctx.effect(move || {
            Disposer::sync(move || {
                bus.unsubscribe::<TurnStart>(owner);
                bus.unsubscribe::<ToolRequest>(owner);
                bus.unsubscribe::<ToolProgress>(owner);
                bus.unsubscribe::<ToolResult>(owner);
                bus.unsubscribe::<TurnEnd>(owner);
                stream.unsubscribe(owner);
            })
        });
        Ok(())
    }
}

/// The text of a tool result, joined across its blocks.
fn text_of(content: &[ToolContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ToolContent::Text(text) => Some(text.as_str()),
            ToolContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
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
