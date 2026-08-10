//! The agent loop.
//!
//! One run is: append the prompt, then repeat *request → stream → commit →
//! execute tools* until the model stops asking for tools, a plugin says stop, or
//! the turn cap is reached.
//!
//! Two invariants hold throughout:
//!
//! - **Nothing panics on a provider failure.** A failed turn arrives as
//!   [`Event::Error`] plus a stop reason on the message, exactly as the core's
//!   streaming contract promises, and the run ends with that reason recorded.
//! - **Tool results are committed in assistant source order**, never completion
//!   order. Providers match results to calls positionally, so how they were
//!   scheduled must never leak into the transcript.

use std::borrow::Cow;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Context;

use aphid_core::{
    ContentInput, ContentRef, Event, Model, StopReason, ToolResultMeta, Transcript, Usage,
};
use futures_core::Stream;
use tokio::task::JoinSet;

use crate::agent::{Agent, RunOutcome};
use crate::plugin::{Cx, Flow, Guard, PendingCall, ResultCx, StreamCx, TurnSummary};
use crate::registry::Plugins;
use crate::tool::{Execution, ToolCall, ToolContent, ToolCx, ToolOutcome};

impl Agent {
    /// Append a user message and run to completion.
    pub async fn prompt(&mut self, text: &str) -> RunOutcome {
        self.transcript.push_user(text);
        self.run().await
    }

    /// Append a user message built from mixed content — text and images — and
    /// run to completion.
    pub async fn prompt_parts(&mut self, parts: &[ContentInput<'_>]) -> RunOutcome {
        self.transcript.push_user_parts(parts);
        self.run().await
    }

    /// Run from the transcript as it stands, without appending anything.
    ///
    /// Use this to resume a loaded session, or to continue after a run stopped
    /// early.
    pub async fn resume(&mut self) -> RunOutcome {
        self.run().await
    }

    async fn run(&mut self) -> RunOutcome {
        self.cancel.store(false, Ordering::Relaxed);
        let tool_cx = self.tool_cx();

        let mut outcome = RunOutcome {
            stop: StopReason::Pending,
            turns: 0,
            usage: Usage::default(),
            last: None,
            error: None,
        };

        {
            let mut cx = cx(
                &mut self.transcript,
                &self.model,
                0,
                outcome.usage,
                &self.cancel,
            );
            self.plugins.run_start(&mut cx);
        }

        let mut turn: u32 = 0;
        let mut aborted = false;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                aborted = true;
                break;
            }
            {
                let mut cx = cx(
                    &mut self.transcript,
                    &self.model,
                    turn,
                    outcome.usage,
                    &self.cancel,
                );
                self.plugins.turn_start(&mut cx);
            }

            // The stream borrows nothing once it resolves, so the immutable
            // borrows of the transcript and the tool table end here.
            let backend = self.stream_fn.clone();
            let mut stream = backend
                .stream(
                    &self.model,
                    &self.transcript,
                    self.tools.declarations(),
                    &self.options,
                )
                .await;

            let observed = self.plugins.observes_events();
            while let Some(event) = next(&mut stream).await {
                if observed {
                    let stream_cx = StreamCx {
                        stream: &*stream,
                        turn,
                    };
                    self.plugins.event(&event, &stream_cx);
                }
            }

            let buffer = stream.finish_boxed();
            let turn_usage = buffer.meta().usage;
            let stop = buffer.meta().stop_reason;
            let error = buffer.meta().error_message.clone();

            self.usage += turn_usage;
            outcome.usage += turn_usage;
            outcome.stop = stop;
            outcome.error.clone_from(&error);

            // One memcpy per arena, however many tokens streamed.
            let message = self.transcript.commit(buffer);
            turn += 1;
            outcome.turns = turn;
            outcome.last = Some(message);

            // Read the requested calls out of the arena. `calls` borrows the
            // transcript, so every result is produced before anything is
            // appended.
            let mut calls: Vec<PendingCall<'_>> = Vec::new();
            for content in self.transcript.message(message).content() {
                if let ContentRef::ToolCall(call) = content {
                    calls.push(PendingCall {
                        id: call.id(),
                        name: call.name(),
                        arguments: Cow::Borrowed(call.arguments_raw()),
                        handler: self.tools.get(call.name()).map(Clone::clone),
                        block: None,
                    });
                }
            }

            let tool_calls = calls.len();
            let mut done = tool_calls == 0 || stop.is_failure();
            let mut results: Vec<(ToolResultMeta, Vec<ToolContent>)> = Vec::new();

            if !done {
                for call in &mut calls {
                    self.plugins.tool_call(call);
                }

                let (batch, all_terminate) =
                    execute(&self.plugins, &calls, &tool_cx, turn - 1).await;
                results = batch;
                done = all_terminate;
            }

            // Releases the transcript so results can be appended.
            drop(calls);

            for (meta, content) in results {
                let parts: Vec<ContentInput<'_>> =
                    content.iter().map(ToolContent::as_input).collect();
                self.transcript.push_tool_result(meta, &parts);
            }

            let summary = TurnSummary {
                message,
                stop_reason: stop,
                usage: turn_usage,
                tool_calls,
                error,
            };
            let flow = {
                let mut cx = cx(
                    &mut self.transcript,
                    &self.model,
                    turn - 1,
                    outcome.usage,
                    &self.cancel,
                );
                self.plugins.turn_end(&mut cx, &summary)
            };

            if done || flow == Flow::Stop || turn >= self.max_turns {
                break;
            }
        }

        // Only a run that was actually cut short reports as aborted: a plugin
        // that cancels on the very turn the model finished still finished.
        if aborted {
            outcome.stop = StopReason::Aborted;
        }

        {
            let mut cx = cx(
                &mut self.transcript,
                &self.model,
                turn,
                outcome.usage,
                &self.cancel,
            );
            self.plugins.run_end(&mut cx, &outcome);
        }

        outcome
    }
}

fn cx<'a>(
    transcript: &'a mut Transcript,
    model: &'a Model,
    turn: u32,
    usage: Usage,
    cancel: &'a AtomicBool,
) -> Cx<'a> {
    Cx {
        transcript,
        model,
        turn,
        usage,
        cancel,
    }
}

/// `futures_core` gives us the trait but no combinators, and one `poll_fn` is
/// cheaper than depending on `futures-util`.
async fn next<S: Stream<Item = Event> + Unpin + ?Sized>(stream: &mut S) -> Option<Event> {
    std::future::poll_fn(|cx: &mut Context<'_>| Pin::new(&mut *stream).poll_next(cx)).await
}

/// Run one turn's tool calls and return their results in assistant source order.
///
/// The second half of the return value is the batch's early-termination
/// verdict: true only when *every* result asked to stop, which is what keeps one
/// opinionated tool from ending a run on its own.
async fn execute(
    plugins: &Plugins,
    calls: &[PendingCall<'_>],
    tool_cx: &ToolCx,
    turn: u32,
) -> (Vec<(ToolResultMeta, Vec<ToolContent>)>, bool) {
    let mut outcomes: Vec<Option<ToolOutcome>> = vec![None; calls.len()];

    // Calls that never reach a handler: blocked by a plugin, or naming a tool
    // that is not registered. Both come back as error results the model can read
    // and correct.
    let mut pending: Vec<usize> = Vec::with_capacity(calls.len());
    for (index, call) in calls.iter().enumerate() {
        match (&call.block, &call.handler) {
            (Some(Guard::Block { reason, .. }), _) => {
                outcomes[index] = Some(ToolOutcome::error(reason.clone()));
            }
            (_, None) => {
                outcomes[index] = Some(ToolOutcome::error(format!(
                    "`{}` is not a registered tool",
                    call.name
                )));
            }
            (_, Some(_)) => pending.push(index),
        }
    }

    let concurrent = pending.len() > 1
        && pending.iter().all(|&index| {
            calls[index]
                .handler
                .as_ref()
                .is_some_and(|handler| handler.execution() == Execution::Parallel)
        });

    if concurrent {
        // `JoinSet::spawn` needs `'static`, so this path copies each call's
        // arguments out of the arena. One small allocation per concurrent call,
        // never per token — and the single-call case below avoids even that.
        let mut set: JoinSet<(usize, ToolOutcome)> = JoinSet::new();
        for &index in &pending {
            let handler = calls[index]
                .handler
                .clone()
                .expect("pending calls have a handler");
            let cx = tool_cx.clone();
            let id = calls[index].id.to_owned();
            let name = calls[index].name.to_owned();
            let arguments = calls[index].arguments.clone().into_owned();
            set.spawn(async move {
                let call = ToolCall {
                    id: &id,
                    name: &name,
                    arguments: &arguments,
                };
                (index, handler.execute(call, &cx).await)
            });
        }
        while let Some(joined) = set.join_next().await {
            // A panicking tool leaves its slot empty; the sweep below turns that
            // into an error result rather than losing the call.
            if let Ok((index, outcome)) = joined {
                outcomes[index] = Some(outcome);
            }
        }
    } else {
        for &index in &pending {
            let call = &calls[index];
            let handler = call
                .handler
                .as_ref()
                .expect("pending calls have a handler")
                .clone();
            let outcome = handler
                .execute(
                    ToolCall {
                        id: call.id,
                        name: call.name,
                        arguments: &call.arguments,
                    },
                    tool_cx,
                )
                .await;
            outcomes[index] = Some(outcome);
        }
    }

    let mut all_terminate = !calls.is_empty();
    let mut results = Vec::with_capacity(calls.len());
    for (index, call) in calls.iter().enumerate() {
        let mut outcome = outcomes[index]
            .take()
            .unwrap_or_else(|| ToolOutcome::error(format!("`{}` produced no result", call.name)));

        if let Some(Guard::Block { terminate, .. }) = &call.block {
            outcome.terminate = *terminate;
        }

        plugins.tool_result(
            &mut outcome,
            &ResultCx {
                id: call.id,
                name: call.name,
                arguments: &call.arguments,
                turn,
            },
        );

        all_terminate &= outcome.terminate;
        results.push(outcome.into_meta(call.id, call.name));
    }

    (results, all_terminate)
}
