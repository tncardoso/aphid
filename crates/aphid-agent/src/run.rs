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
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Context;

use aphid_core::{ContentInput, ContentRef, Event, StopReason, ToolResultMeta, Transcript, Usage};
use futures_core::Stream;
use tokio::task::JoinSet;

use crate::agent::{Agent, RunOutcome};
use crate::events::{self, Blocked, Edit, Moment, Run};
use crate::plugin::{StreamCx, TurnSummary};
use crate::rt::{Bus, Scope};
use crate::tool::{Execution, ToolCall, ToolContent, ToolCx, ToolHandler, ToolOutcome};

impl Agent {
    /// Append a user message and run to completion.
    pub async fn prompt(&mut self, text: &str) -> RunOutcome {
        self.ready().await;
        let text = match self.draft(text) {
            Ok(text) => text,
            Err(reason) => return RunOutcome::rejected(reason),
        };
        self.transcript.push_user(&text);
        self.run().await
    }

    /// Append a user message built from mixed content — text and images — and
    /// run to completion.
    pub async fn prompt_parts(&mut self, parts: &[ContentInput<'_>]) -> RunOutcome {
        self.ready().await;
        // Plugins are shown the text, not the attachments. A rewrite therefore
        // replaces every text part with one, and the images ride along in order.
        if self.observes_prompts() {
            let joined = join_text(parts);
            let edited = match self.draft(&joined) {
                Ok(text) => text,
                Err(reason) => return RunOutcome::rejected(reason),
            };
            if edited != joined {
                let mut replaced: Vec<ContentInput<'_>> = vec![ContentInput::Text(&edited)];
                replaced.extend(
                    parts
                        .iter()
                        .filter(|part| !matches!(part, ContentInput::Text(_)))
                        .copied(),
                );
                self.transcript.push_user_parts(&replaced);
                return self.run().await;
            }
        }

        self.transcript.push_user_parts(parts);
        self.run().await
    }

    /// Show a prompt to the plugins before anything is appended.
    ///
    /// `Err` is the reason a listener turned it away. A rejected prompt leaves
    /// the transcript untouched, so the conversation is exactly as it was.
    fn draft<'a>(&self, text: &'a str) -> Result<Cow<'a, str>, String> {
        if !self.observes_prompts() {
            return Ok(Cow::Borrowed(text));
        }

        let mut prompt = events::Prompt::new(text.to_owned());
        self.bus.emit_scoped(&self.scope, &mut prompt);

        match prompt.rejection() {
            Some(reason) => Err(reason.to_owned()),
            None => Ok(Cow::Owned(prompt.text)),
        }
    }

    fn observes_prompts(&self) -> bool {
        self.bus.has_listeners::<events::Prompt>()
    }

    /// A payload for the run-scoped events, and the handle that applies what a
    /// listener asked for.
    fn run_payload(&self, turn: u32, usage: Usage) -> Run {
        Run::new(
            self.model.clone(),
            turn,
            usage,
            std::sync::Arc::clone(&self.cancel),
        )
    }

    /// Run from the transcript as it stands, without appending anything.
    ///
    /// Use this to resume a loaded session, or to continue after a run stopped
    /// early.
    pub async fn resume(&mut self) -> RunOutcome {
        self.run().await
    }

    /// Load anything mounted since the last time through.
    ///
    /// Called before the prompt is drafted, not just before the run: the
    /// prompt is announced first, and a component mounted for it would
    /// otherwise miss the very thing it was mounted for.
    async fn ready(&self) {
        if let Some(composition) = &self.composition {
            composition.settle().await;
        }
    }

    async fn run(&mut self) -> RunOutcome {
        // Assembly code that could not await gets to be ordinary assembly
        // code: whatever it mounted is loaded by the time anything is
        // announced. `prompt` settles earlier still, because the prompt is
        // announced before the run begins.
        self.ready().await;

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
            let mut start = events::RunStart(self.run_payload(0, outcome.usage));
            self.bus.emit_scoped(&self.scope, &mut start);
            apply_edits(&mut self.transcript, &start.0);
            self.transcript_listeners.announce(
                &self.scope,
                Moment::RunStart,
                &self.transcript,
                &start.0,
            );
        }

        let mut turn: u32 = 0;
        let mut aborted = false;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                aborted = true;
                break;
            }
            {
                let mut start = events::TurnStart(self.run_payload(turn, outcome.usage));
                self.bus.emit_scoped(&self.scope, &mut start);
                apply_edits(&mut self.transcript, &start.0);
            }

            // The stream borrows nothing once it resolves, so the immutable
            // borrows of the transcript and the tool table end here.
            let backend = self.stream_fn.clone();
            // Read afresh each turn rather than once at build, which is what
            // lets a component contribute or withdraw a tool mid-session.
            let declarations = self.tools.declarations();
            let mut stream = backend
                .stream(&self.model, &self.transcript, &declarations, &self.options)
                .await;

            // Taken once per stream, not once per token: the list cannot
            // change under a response, and the read costs nothing thereafter.
            // Scoped, so a turn streams only to the listeners that belong to
            // its session.
            let listeners = self.stream_listeners.snapshot(&self.scope);
            let observed = !listeners.is_empty();
            while let Some(event) = next(&mut stream).await {
                if observed {
                    let stream_cx = StreamCx {
                        stream: &*stream,
                        turn,
                    };
                    for listener in listeners.iter() {
                        listener(&event, &stream_cx);
                    }
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

            {
                let mut event = events::Message {
                    run: self.run_payload(turn - 1, outcome.usage),
                    message,
                };
                self.bus.emit_scoped(&self.scope, &mut event);
                apply_edits(&mut self.transcript, &event.run);
                self.transcript_listeners.announce(
                    &self.scope,
                    Moment::Message,
                    &self.transcript,
                    &event.run,
                );
            }

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
                        handler: self.tools.get(call.name()),
                        blocked: None,
                    });
                }
            }

            let tool_calls = calls.len();
            let mut done = tool_calls == 0 || stop.is_failure();
            let mut results: Vec<(ToolResultMeta, Vec<ToolContent>)> = Vec::new();

            if !done {
                for call in &mut calls {
                    // A rewrite and a refusal are different decisions, so
                    // they are different events: a listener that only edits
                    // the arguments cannot accidentally block the call. The
                    // rewrite runs first, so what the guards are shown is what
                    // the tool would actually receive.
                    if self.bus.has_listeners::<events::ToolArguments>() {
                        let before = call.arguments.clone().into_owned();
                        let edited = self
                            .bus
                            .waterfall::<events::ToolArguments>(before.clone(), &|args| args);
                        if edited != before {
                            call.arguments = Cow::Owned(edited);
                        }
                    }

                    let mut request = events::ToolRequest {
                        id: call.id.to_owned(),
                        name: call.name.to_owned(),
                        arguments: call.arguments.clone().into_owned(),
                        known: call.handler.is_some(),
                        blocked: None,
                    };
                    self.bus.emit_scoped(&self.scope, &mut request);
                    if request.arguments != *call.arguments {
                        call.arguments = Cow::Owned(request.arguments);
                    }
                    if call.blocked.is_none() {
                        call.blocked = request.blocked;
                    }
                }

                let (batch, all_terminate) =
                    execute(&self.bus, &self.scope, &calls, &tool_cx, turn - 1).await;
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
            let stop_asked = {
                let mut event = events::TurnEnd {
                    run: self.run_payload(turn - 1, outcome.usage),
                    summary,
                    stop: false,
                };
                self.bus.emit_scoped(&self.scope, &mut event);
                apply_edits(&mut self.transcript, &event.run);
                self.transcript_listeners.announce(
                    &self.scope,
                    Moment::TurnEnd,
                    &self.transcript,
                    &event.run,
                );
                event.stop
            };

            if done || stop_asked || turn >= self.max_turns {
                break;
            }
        }

        // Only a run that was actually cut short reports as aborted: a plugin
        // that cancels on the very turn the model finished still finished.
        if aborted {
            outcome.stop = StopReason::Aborted;
        }

        {
            let mut event = events::RunEnd::new(self.run_payload(turn, outcome.usage), &outcome);
            self.bus.emit_scoped(&self.scope, &mut event);
            apply_edits(&mut self.transcript, &event.run);
        }

        outcome
    }
}

/// Apply what listeners asked for, in the order they asked.
///
/// Deferred rather than applied inside the listener because the payload holds
/// no borrow of the transcript — which is what lets a listener keep it, or
/// answer from another thread.
fn apply_edits(transcript: &mut Transcript, run: &Run) {
    for edit in run.take_edits() {
        match edit {
            Edit::Note(text) => {
                transcript.push_system(&text);
            }
            Edit::User(text) => {
                transcript.push_user(&text);
            }
        }
    }
}

/// A tool call read out of the transcript and not yet run.
///
/// The loop's own bookkeeping, not a public surface: `arguments` borrows the
/// transcript arena until a listener replaces it, so inspecting a call costs
/// nothing and rewriting one costs a single allocation.
struct PendingCall<'a> {
    id: &'a str,
    name: &'a str,
    arguments: Cow<'a, str>,
    handler: Option<Arc<dyn ToolHandler>>,
    /// Why this call will not run, if a listener refused it.
    blocked: Option<Blocked>,
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
/// The text parts of a mixed-content prompt, as one string.
fn join_text(parts: &[ContentInput<'_>]) -> String {
    let mut joined = String::new();
    for part in parts {
        if let ContentInput::Text(text) = part {
            if !joined.is_empty() {
                joined.push('\n');
            }
            joined.push_str(text);
        }
    }
    joined
}

async fn execute(
    bus: &Bus,
    scope: &Scope,
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
        match (&call.blocked, &call.handler) {
            (Some(blocked), _) => {
                outcomes[index] = Some(ToolOutcome::error(blocked.reason.clone()));
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
            let cx = tool_cx.for_call(calls[index].id, calls[index].name);
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
            let cx = tool_cx.for_call(call.id, call.name);
            let outcome = handler
                .execute(
                    ToolCall {
                        id: call.id,
                        name: call.name,
                        arguments: &call.arguments,
                    },
                    &cx,
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

        if let Some(blocked) = &call.blocked {
            outcome.terminate = blocked.terminate;
        }

        if bus.has_listeners::<events::ToolResult>() {
            let mut event = events::ToolResult {
                id: call.id.to_owned(),
                name: call.name.to_owned(),
                arguments: call.arguments.clone().into_owned(),
                turn,
                content: std::mem::take(&mut outcome.content),
                is_error: outcome.is_error,
                details: outcome.details.clone(),
            };
            bus.emit_scoped(scope, &mut event);
            outcome.content = event.content;
            outcome.is_error = event.is_error;
            outcome.details = event.details;
        }

        all_terminate &= outcome.terminate;
        results.push(outcome.into_meta(call.id, call.name));
    }

    (results, all_terminate)
}
