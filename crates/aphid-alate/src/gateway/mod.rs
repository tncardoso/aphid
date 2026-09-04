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
//! [`GatewayComponent`] and [`GatewaySink`] are what turn a run into frames. They
//! follow [`UiComponent`] and [`UiSink`] closely — the same events and the same
//! mapping — because the fan-out channel differs and nothing else does.
//!
//! [`UiComponent`]: aphid_code::tui::UiComponent
//! [`UiSink`]: aphid_code::tui::UiSink

pub mod attachment;
pub mod client;
pub mod server;
pub mod wire;

pub use client::{Client, Reader, Writer, is_listening};
pub use server::{Event, Publisher, Server};
pub use wire::{Answer, Envelope, Frame, Request};

use std::sync::Arc;

use aphid_agent::rt::{Component, Composition, Context, Disposer, Scope};
use aphid_agent::{
    RunEnd, StreamCx, ToolContent, ToolProgress, ToolRequest, ToolResult, TurnEnd, TurnStart,
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

impl aphid_agent::Sink for GatewaySink {
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
pub struct GatewayComponent {
    publisher: Publisher,
    composition: Composition,
    /// The session this component turns into frames, or `None` for a standalone
    /// agent. Frames are only published for announcements stamped with it, so
    /// one conversation never shows up as another's.
    scope: Scope,
}

impl GatewayComponent {
    #[must_use]
    pub fn new(scope: Scope, publisher: Publisher, composition: &Composition) -> Self {
        Self {
            publisher,
            composition: composition.clone(),
            scope,
        }
    }
}

impl Component for GatewayComponent {
    fn name(&self) -> &str {
        "gateway"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let owner = ctx.uid();
        let bus = Arc::clone(&self.composition.bus);

        let publisher = self.publisher.clone();
        let scope = self.scope.clone();
        bus.on_scoped::<TurnStart>(scope, owner, move |_| publisher.send(Frame::TurnStarted));

        let publisher = self.publisher.clone();
        let scope = self.scope.clone();
        bus.on_scoped::<ToolRequest>(scope, owner, move |request| {
            publisher.send(Frame::ToolCall {
                id: request.id.clone(),
                name: request.name.clone(),
                arguments: request.arguments.clone(),
            });
        });

        let publisher = self.publisher.clone();
        let scope = self.scope.clone();
        bus.on_scoped::<ToolProgress>(scope, owner, move |progress| {
            publisher.send(Frame::ToolProgress {
                id: progress.call_id.clone(),
                chunk: progress.chunk.clone(),
            });
        });

        let publisher = self.publisher.clone();
        let scope = self.scope.clone();
        bus.on_scoped::<ToolResult>(scope, owner, move |result| {
            publisher.send(Frame::ToolResult {
                id: result.id.clone(),
                name: result.name.clone(),
                text: text_of(&result.content),
                is_error: result.is_error,
                details: result.details.clone(),
            });
        });

        let publisher = self.publisher.clone();
        let scope = self.scope.clone();
        bus.on_scoped::<TurnEnd>(scope, owner, move |end| {
            publisher.send(Frame::TurnEnded {
                usage: end.summary.usage,
                stop: end.summary.stop_reason,
                error: end.summary.error.clone(),
            });
        });

        let publisher = self.publisher.clone();
        let scope = self.scope.clone();
        bus.on_scoped::<RunEnd>(scope, owner, move |end| {
            publisher.send(Frame::RunEnded {
                stop: end.stop,
                turns: end.turns,
                error: end.error.clone(),
            });
        });

        let publisher = self.publisher.clone();
        let scope = self.scope.clone();
        self.composition
            .stream
            .subscribe_scoped(scope, owner, move |event, cx| match *event {
                Protocol::BlockStart {
                    index,
                    kind: BlockKind::ToolCall,
                } => publisher.send(Frame::ToolStreamStart {
                    block: index,
                    name: tool_name(cx, index),
                }),
                Protocol::Delta {
                    index,
                    kind: BlockKind::ToolCall,
                    span,
                } => publisher.send(Frame::ToolStreamDelta {
                    block: index,
                    bytes: span.len() as usize,
                }),
                Protocol::Delta {
                    kind: BlockKind::Text,
                    span,
                    ..
                } => publisher.send(Frame::Text {
                    text: cx.text(span).to_owned(),
                }),
                Protocol::Delta {
                    kind: BlockKind::Thinking,
                    span,
                    ..
                } => publisher.send(Frame::Thinking {
                    text: cx.text(span).to_owned(),
                }),
                _ => {}
            });

        let bus = Arc::clone(&self.composition.bus);
        let stream = Arc::clone(&self.composition.stream);
        ctx.effect(move || {
            Disposer::sync(move || {
                bus.unsubscribe::<TurnStart>(owner);
                bus.unsubscribe::<ToolRequest>(owner);
                bus.unsubscribe::<ToolProgress>(owner);
                bus.unsubscribe::<ToolResult>(owner);
                bus.unsubscribe::<TurnEnd>(owner);
                bus.unsubscribe::<RunEnd>(owner);
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
