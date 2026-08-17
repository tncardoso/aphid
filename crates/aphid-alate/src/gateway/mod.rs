//! The socket a running alate is reached through.
//!
//! The gateway is the daemon's only door. A client attaches, is told what the
//! agent is and what it has been doing, and from then on sees the run as it
//! happens and can put words into it. The terminal in [`crate::tui`] is the
//! first client; anything that speaks [`wire`] is the next, and the daemon does
//! not change to gain one.
//!
//! It is a Unix socket. That rules out Windows, which is stated rather than
//! worked around: the alternative is a TCP port on a machine where every other
//! process could reach the agent, and file permissions are a better answer than
//! a token nobody rotates.
//!
//! [`GatewayPlugin`] and [`GatewaySink`] are what turn a run into frames. They
//! follow [`UiPlugin`] and [`UiSink`] closely — the same hooks and the same
//! mapping — because the fan-out channel differs and nothing else does.
//!
//! [`UiPlugin`]: aphid_code::tui::UiPlugin
//! [`UiSink`]: aphid_code::tui::UiSink

pub mod client;
pub mod server;
pub mod wire;

pub use client::{Client, Reader, Writer, is_listening};
pub use server::{Event, Publisher, Server};
pub use wire::{Answer, Envelope, Frame, Request};

use aphid_agent::{
    Cx, Flow, Guard, Interest, PendingCall, Plugin, ResultCx, RunOutcome, StreamCx, ToolOutcome,
    TurnSummary,
};
// Renamed: this module also exports a gateway `Event`, which is what the
// daemon hears from the socket rather than what a provider streams.
use aphid_core::{BlockKind, ContentRef, Event as Protocol};

/// Sends a plugin's `notify` and `prompt` output to the clients.
///
/// `log` keeps its default, which is standard error: a developer running the
/// daemon in a terminal wants it there, and a client wants the conversation and
/// not the tracing.
pub struct GatewaySink {
    publisher: Publisher,
}

impl GatewaySink {
    #[must_use]
    pub fn new(publisher: Publisher) -> Self {
        Self { publisher }
    }
}

impl aphid_plugin::Sink for GatewaySink {
    fn notify(&self, plugin: &str, text: &str) {
        self.publisher.send(Frame::Notice {
            text: format!("{plugin}: {text}"),
        });
    }

    fn prompt(&self, _plugin: &str, text: &str) {
        // Only the echo. The daemon has the prompt queue, and it puts what a
        // plugin says into the same one a client's words go to.
        self.publisher.send(Frame::Prompt {
            text: text.to_owned(),
        });
    }
}

/// Turns the run into frames.
pub struct GatewayPlugin {
    publisher: Publisher,
}

impl GatewayPlugin {
    #[must_use]
    pub fn new(publisher: Publisher) -> Self {
        Self { publisher }
    }
}

impl Plugin for GatewayPlugin {
    fn name(&self) -> &str {
        "gateway"
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
        self.publisher.send(Frame::TurnStarted);
    }

    fn on_event(&self, event: &Protocol, cx: &StreamCx<'_>) {
        match *event {
            Protocol::BlockStart {
                index,
                kind: BlockKind::ToolCall,
            } => self.publisher.send(Frame::ToolStreamStart {
                block: index,
                name: tool_name(cx, index),
            }),
            Protocol::Delta {
                index,
                kind: BlockKind::ToolCall,
                span,
            } => self.publisher.send(Frame::ToolStreamDelta {
                block: index,
                bytes: span.len() as usize,
            }),
            Protocol::Delta {
                kind: BlockKind::Text,
                span,
                ..
            } => self.publisher.send(Frame::Text {
                text: cx.text(span).to_owned(),
            }),
            Protocol::Delta {
                kind: BlockKind::Thinking,
                span,
                ..
            } => self.publisher.send(Frame::Thinking {
                text: cx.text(span).to_owned(),
            }),
            _ => {}
        }
    }

    fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
        self.publisher.send(Frame::ToolCall {
            id: call.id().to_owned(),
            name: call.name().to_owned(),
            arguments: call.arguments().to_owned(),
        });
        Guard::Allow
    }

    fn on_tool_progress(&self, call_id: &str, _tool: &str, chunk: &str) {
        self.publisher.send(Frame::ToolProgress {
            id: call_id.to_owned(),
            chunk: chunk.to_owned(),
        });
    }

    fn on_tool_result(&self, outcome: &mut ToolOutcome, cx: &ResultCx<'_>) {
        self.publisher.send(Frame::ToolResult {
            id: cx.id().to_owned(),
            name: cx.name().to_owned(),
            text: outcome.text_content(),
            is_error: outcome.is_error,
            details: outcome.details.clone(),
        });
    }

    fn on_turn_end(&self, _cx: &mut Cx<'_>, turn: &TurnSummary) -> Flow {
        self.publisher.send(Frame::TurnEnded {
            usage: turn.usage,
            stop: turn.stop_reason,
            error: turn.error.clone(),
        });
        Flow::Continue
    }

    fn on_run_end(&self, _cx: &mut Cx<'_>, outcome: &RunOutcome) {
        self.publisher.send(Frame::RunEnded {
            stop: outcome.stop,
            turns: outcome.turns,
            error: outcome.error.clone(),
        });
    }
}

/// The name of the tool call in the block at `index`.
///
/// The name is staged when the block opens, so this reads it out of the partial
/// message rather than waiting for the call to be announced.
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
