//! A scripted backend, so the loop can be exercised without a network.
//!
//! This is a first-class part of the crate rather than a test fixture: anyone
//! writing a plugin needs a way to drive it through a known sequence of turns,
//! and the alternative is mocking HTTP.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use aphid_core::{
    AssistantMeta, AssistantStream, BlockKind, Event, MessageBuffer, MessageRef, Model,
    SimpleStreamOptions, Span, StopReason, Tool, Transcript, Usage, encode_request,
};
use futures_core::Stream;

use crate::stream::{Backend, BoxStream, StreamFn};
use crate::tool::BoxFuture;

/// One scripted assistant turn.
#[derive(Clone, Debug)]
pub struct Turn {
    pub text: Option<String>,
    /// Tool calls, as `(id, name, raw JSON arguments)`, in the order the model
    /// would have emitted them.
    pub calls: Vec<(String, String, String)>,
    pub stop: StopReason,
    pub error: Option<String>,
    pub usage: Usage,
}

impl Turn {
    /// A turn that answers and stops.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            calls: Vec::new(),
            stop: StopReason::Stop,
            error: None,
            usage: Usage::default(),
        }
    }

    /// A turn that asks for one tool.
    #[must_use]
    pub fn call(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            text: None,
            calls: vec![(id.into(), name.into(), arguments.into())],
            stop: StopReason::ToolUse,
            error: None,
            usage: Usage::default(),
        }
    }

    /// Add another tool call to this turn, to exercise batch execution.
    #[must_use]
    pub fn and_call(
        mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        self.calls.push((id.into(), name.into(), arguments.into()));
        self.stop = StopReason::ToolUse;
        self
    }

    /// A turn that failed, the way a transport or provider error arrives.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            text: None,
            calls: Vec::new(),
            stop: StopReason::Error,
            error: Some(message.into()),
            usage: Usage::default(),
        }
    }

    #[must_use]
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }
}

/// A backend that replays a fixed list of turns and records what was sent.
#[derive(Debug, Default)]
pub struct Script {
    turns: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<String>>,
}

impl Script {
    #[must_use]
    pub fn new(turns: impl IntoIterator<Item = Turn>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// The encoded body of every request made so far, newest last. Assert
    /// against these to check that tool results actually reached the provider.
    ///
    /// # Panics
    ///
    /// Panics if a previous caller panicked while holding the lock.
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("script lock").clone()
    }

    /// How many requests the agent made.
    ///
    /// # Panics
    ///
    /// Panics if a previous caller panicked while holding the lock.
    #[must_use]
    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("script lock").len()
    }

    /// Turns left unplayed.
    ///
    /// # Panics
    ///
    /// Panics if a previous caller panicked while holding the lock.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.turns.lock().expect("script lock").len()
    }
}

impl Backend for Script {
    fn stream<'a>(
        &'a self,
        model: &'a Model,
        transcript: &'a Transcript,
        tools: &'a [Tool],
        options: &'a SimpleStreamOptions,
    ) -> BoxFuture<'a, BoxStream> {
        // Encoding the real request keeps the mock honest: a transcript the
        // encoder rejects fails here rather than passing silently.
        let recorded = match encode_request(model, transcript, tools, options) {
            Ok(body) => body,
            Err(error) => format!("<encode failed: {error}>"),
        };
        self.requests.lock().expect("script lock").push(recorded);

        let turn = self
            .turns
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or_else(|| Turn::failed("the script ran out of turns"));

        Box::pin(std::future::ready(
            Box::new(ScriptedStream::new(model, &turn)) as BoxStream,
        ))
    }
}

/// Turn a script into something [`AgentBuilder::stream_fn`] accepts.
///
/// [`AgentBuilder::stream_fn`]: crate::AgentBuilder::stream_fn
#[must_use]
pub fn scripted(turns: impl IntoIterator<Item = Turn>) -> (StreamFn, Arc<Script>) {
    let script = Script::new(turns);
    (Arc::clone(&script) as StreamFn, script)
}

/// One scripted turn, replayed as protocol events.
///
/// The whole message is built up front, so `text` resolves spans exactly the way
/// a live stream does.
pub struct ScriptedStream {
    buffer: MessageBuffer,
    events: VecDeque<Event>,
}

impl ScriptedStream {
    fn new(model: &Model, turn: &Turn) -> Self {
        let meta = AssistantMeta::new(model.api.clone(), model.provider.clone(), model.id.clone());
        let mut buffer = MessageBuffer::new(meta);
        let mut events = VecDeque::new();
        events.push_back(Event::Start);

        if let Some(text) = &turn.text {
            let index = buffer.begin_text();
            let span = buffer.push_delta(index, text);
            events.push_back(Event::BlockStart {
                index,
                kind: BlockKind::Text,
            });
            events.push_back(Event::Delta {
                index,
                kind: BlockKind::Text,
                span,
            });
            events.push_back(Event::BlockEnd { index });
        }

        for (id, name, arguments) in &turn.calls {
            let index = buffer.begin_tool_call(id.as_str(), name.as_str());
            let span = buffer.push_delta(index, arguments);
            events.push_back(Event::BlockStart {
                index,
                kind: BlockKind::ToolCall,
            });
            events.push_back(Event::Delta {
                index,
                kind: BlockKind::ToolCall,
                span,
            });
            events.push_back(Event::BlockEnd { index });
        }

        buffer.meta_mut().stop_reason = turn.stop;
        buffer.meta_mut().usage = turn.usage;
        buffer.meta_mut().error_message.clone_from(&turn.error);

        events.push_back(if turn.stop.is_failure() {
            Event::Error { stop: turn.stop }
        } else {
            Event::Done { stop: turn.stop }
        });

        Self { buffer, events }
    }
}

impl Stream for ScriptedStream {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Event>> {
        Poll::Ready(self.events.pop_front())
    }
}

impl AssistantStream for ScriptedStream {
    fn text(&self, span: Span) -> &str {
        self.buffer.text(span)
    }

    fn partial(&self) -> MessageRef<'_> {
        self.buffer.partial()
    }

    fn finish(self) -> MessageBuffer {
        self.buffer
    }
}
