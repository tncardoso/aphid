//! Subscribing a script to what the loop announces.
//!
//! A script says what it wants:
//!
//! ```rhai
//! fn apply(ctx) {
//!     on("agent/tool-call", |tool| {
//!         if tool.name == "write" { block("not that one"); }
//!     });
//! }
//! ```
//!
//! Rather than being read off a function's name. The difference is that this
//! is a **decision the plugin makes**: it can subscribe only when its
//! configuration asks for it, subscribe to something another plugin declared,
//! or subscribe twice. A name in the source can do none of those, and it is the
//! same static-subscription problem that used to keep the loop from noticing a
//! plugin loaded after it started.
//!
//! Everything registered here is filed under the fiber that registered it, so
//! it leaves when the plugin does.

use std::sync::Arc;

use aphid_agent::rt::{Composition, Next, Uid};
use aphid_agent::{
    Blocked, Moment, Prompt, RunEnd, RunStart, ToolContent, ToolProgress, ToolRequest, ToolResult,
    TurnEnd, TurnStart,
};
use rhai::{Dynamic, FnPtr, Map};

use crate::events::{
    Ask, FileChange, Notice, Permission, Session, SessionEnd, SessionStart, SystemPrompt, Tick,
};

use super::convert;
use super::cx::ScriptCx;
use super::host::{as_map, event_map, field_string, message_map, stop_reason, verdict};
use super::script::ScriptPlugin;

/// Every event name a script may subscribe to.
///
/// Declared so that a typo is reported rather than silently never firing —
/// which, in a model where waiting is a legitimate state, is the one failure
/// nobody can debug.
pub(crate) const EVENTS: &[&str] = &[
    "agent/prompt",
    "agent/run-start",
    "agent/turn-start",
    "agent/message",
    "agent/tool-call",
    "agent/tool-progress",
    "agent/tool-result",
    "agent/turn-end",
    "agent/run-end",
    "agent/event",
    "code/system-prompt",
    "code/tick",
    "code/session-start",
    "code/session-end",
    "code/permission",
    "code/file-change",
    "code/notice",
];

/// Call a listener, reporting a failure rather than returning it.
///
/// The other direction — returning it — is what a tool body wants, because a
/// tool's failure belongs in its result where the model will read it. A
/// listener has no result, so its failure belongs where a person will read it.
fn deliver(plugin: &ScriptPlugin, body: &FnPtr, args: impl rhai::FuncArgs) -> Option<Dynamic> {
    match plugin.call_fn(body, args) {
        Ok(value) => Some(value),
        Err(error) => {
            plugin.report(&format!("a listener failed: {error}"));
            None
        }
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

/// Subscribe one script closure to one event.
///
/// # Errors
///
/// A name nothing announces.
pub(crate) fn on(
    composition: &Composition,
    owner: Uid,
    plugin: &Arc<ScriptPlugin>,
    event: &str,
    body: FnPtr,
) -> Result<(), String> {
    let bus = &composition.bus;
    let script = Arc::clone(plugin);

    match event {
        "agent/prompt" => bus.on::<Prompt>(owner, move |payload| {
            prompt(&script, &body, payload);
        }),
        "agent/run-start" => bus.on::<RunStart>(owner, move |start| {
            let cx = ScriptCx::new(&start.0);
            deliver(&script, &body, (cx,));
        }),
        "agent/turn-start" => bus.on::<TurnStart>(owner, move |start| {
            let cx = ScriptCx::new(&start.0);
            deliver(&script, &body, (cx,));
        }),
        "agent/tool-call" => bus.on::<ToolRequest>(owner, move |request| {
            tool_call(&script, &body, request);
        }),
        "agent/tool-progress" => bus.on::<ToolProgress>(owner, move |progress| {
            deliver(
                &script,
                &body,
                (
                    progress.call_id.clone(),
                    progress.tool.clone(),
                    progress.chunk.clone(),
                ),
            );
        }),
        "agent/tool-result" => bus.on::<ToolResult>(owner, move |result| {
            tool_result(&script, &body, result);
        }),
        "agent/turn-end" => bus.on::<TurnEnd>(owner, move |end| {
            turn_end(&script, &body, end);
        }),
        "agent/run-end" => bus.on::<RunEnd>(owner, move |end| {
            run_end(&script, &body, end);
        }),
        // Reads the transcript, so it listens where transcript readers listen.
        "agent/message" => {
            composition
                .transcript
                .subscribe(owner, move |moment, transcript, run| {
                    if moment != Moment::Message {
                        return;
                    }
                    let last = transcript.len().saturating_sub(1);
                    let Some(id) = transcript.id_at(last) else {
                        return;
                    };
                    let cx = ScriptCx::new(run);
                    let payload = Dynamic::from_map(message_map(transcript, id));
                    deliver(&script, &body, (cx, payload));
                });
        }
        // The token stream, which is not on the bus: what it hands out borrows
        // the response arena. Subscribing to it is a choice with a cost, and
        // `/plugins` says who made it.
        "agent/event" => {
            composition.stream.subscribe(owner, move |event, cx| {
                deliver(&script, &body, (event_map(event, cx),));
            });
        }
        "code/system-prompt" => {
            bus.on_waterfall::<SystemPrompt>(
                owner,
                move |prompt: String, next: Next<'_, SystemPrompt>| {
                    // The chain runs outside in, so a listener sees what the ones
                    // registered after it produced. Appending and replacing are the
                    // same operation from two ends.
                    let text = next.run(prompt);
                    let Some(returned) = deliver(&script, &body, (text.clone(),)) else {
                        return text;
                    };
                    let Some(patch) = as_map(&returned) else {
                        return text;
                    };
                    let mut text = match patch.get("replace").filter(|value| value.is_string()) {
                        Some(replacement) => replacement.to_string(),
                        None => text,
                    };
                    if let Some(extra) = patch.get("append").filter(|value| value.is_string()) {
                        text.push_str("\n\n");
                        text.push_str(&extra.to_string());
                    }
                    text
                },
            );
        }
        "code/tick" => bus.on::<Tick>(owner, move |_| {
            deliver(&script, &body, ());
        }),
        "code/session-start" => bus.on::<SessionStart>(owner, move |start| {
            deliver(&script, &body, (session_map(&start.0),));
        }),
        "code/session-end" => bus.on::<SessionEnd>(owner, move |end| {
            deliver(&script, &body, (session_map(&end.0),));
        }),
        "code/permission" => bus.on_bail::<Ask>(owner, move |ask| permission(&script, &body, ask)),
        "code/file-change" => bus.on::<FileChange>(owner, move |change| {
            let mut payload = Map::new();
            payload.insert("path".into(), change.path.display().to_string().into());
            payload.insert("kind".into(), change.kind.as_str().into());
            payload.insert(
                "before".into(),
                change
                    .before
                    .as_ref()
                    .map_or(Dynamic::UNIT, |text| text.clone().into()),
            );
            payload.insert("after".into(), change.after.clone().into());
            deliver(&script, &body, (payload,));
        }),
        "code/notice" => bus.on::<Notice>(owner, move |notice| {
            deliver(&script, &body, (notice.0.clone(),));
        }),
        other => {
            return Err(format!(
                "nothing announces `{other}` — the names are: {}",
                EVENTS.join(", ")
            ));
        }
    }
    Ok(())
}

/// A session, flattened for a script.
fn session_map(session: &Session) -> Map {
    let mut payload = Map::new();
    payload.insert(
        "id".into(),
        session
            .id
            .as_ref()
            .map_or(Dynamic::UNIT, |id| id.clone().into()),
    );
    payload.insert(
        "path".into(),
        session
            .path
            .as_ref()
            .map_or(Dynamic::UNIT, |path| path.display().to_string().into()),
    );
    payload.insert("reason".into(), session.reason.clone().into());
    payload.insert(
        "restored".into(),
        i64::try_from(session.restored).unwrap_or(i64::MAX).into(),
    );
    payload
}

/// Ask one script about a permission.
///
/// `None` is *no opinion*, which is what lets the next listener — and finally
/// the user — decide. A script that raises **denies**: this is the announcement
/// people subscribe to when they mean to be careful, and a guard that failed
/// has not approved anything.
fn permission(plugin: &ScriptPlugin, body: &FnPtr, ask: &Ask) -> Option<Permission> {
    let mut payload = Map::new();
    payload.insert("tool".into(), ask.tool.clone().into());
    payload.insert("summary".into(), ask.summary.clone().into());
    payload.insert(
        "risk".into(),
        format!("{:?}", ask.risk).to_lowercase().into(),
    );

    let Some(returned) = deliver(plugin, body, (payload,)) else {
        return Some(Permission::Deny);
    };

    if returned.is_string() {
        return Permission::parse(&returned.to_string());
    }
    as_map(&returned)
        .and_then(|map| map.get("verdict").map(std::string::ToString::to_string))
        .and_then(|text| Permission::parse(&text))
}

/// Drop every subscription a fiber made, whatever it subscribed to.
pub(crate) fn unsubscribe(composition: &Composition, owner: Uid) {
    let bus = &composition.bus;
    bus.unsubscribe::<Prompt>(owner);
    bus.unsubscribe::<RunStart>(owner);
    bus.unsubscribe::<TurnStart>(owner);
    bus.unsubscribe::<ToolRequest>(owner);
    bus.unsubscribe::<ToolProgress>(owner);
    bus.unsubscribe::<ToolResult>(owner);
    bus.unsubscribe::<TurnEnd>(owner);
    bus.unsubscribe::<RunEnd>(owner);
    bus.unsubscribe::<Tick>(owner);
    bus.unsubscribe::<SessionStart>(owner);
    bus.unsubscribe::<SessionEnd>(owner);
    bus.unsubscribe::<FileChange>(owner);
    bus.unsubscribe::<Notice>(owner);
    bus.unsubscribe_bail::<Ask>(owner);
    bus.unsubscribe_waterfall::<SystemPrompt>(owner);
    composition.stream.unsubscribe(owner);
    composition.transcript.unsubscribe(owner);
}

fn prompt(plugin: &ScriptPlugin, body: &FnPtr, prompt: &mut Prompt) {
    {
        // A later listener cannot un-reject, so there is nothing to apply.
        if prompt.rejection().is_some() {
            return;
        }

        let mut payload = Map::new();
        payload.insert("text".into(), prompt.text.clone().into());

        let Some(returned) = deliver(plugin, body, (payload,)) else {
            return;
        };

        match verdict(&returned) {
            Some(("reject", reason)) => prompt.reject(reason),
            _ => {
                if let Some(text) = field_string(&returned, "text") {
                    prompt.text = text;
                } else if returned.is_string() {
                    prompt.text = returned.into_string().unwrap_or_default();
                }
            }
        }
    }
}

/// Preflight one tool call.
///
/// Every script that defines the hook runs, even after one has refused, so
/// an observer sees the call. The first refusal is the one that stands.
fn tool_call(plugin: &ScriptPlugin, body: &FnPtr, request: &mut ToolRequest) {
    {
        let mut payload = Map::new();
        payload.insert("id".into(), request.id.clone().into());
        payload.insert("name".into(), request.name.clone().into());
        payload.insert("arguments".into(), request.arguments.clone().into());
        payload.insert("known".into(), request.known.into());
        payload.insert("blocked".into(), request.is_blocked().into());

        let Some(returned) = deliver(plugin, body, (payload,)) else {
            // Fail closed: a guard that raised has not allowed anything.
            request.refuse(Blocked::new(format!(
                "plugin `{}` failed to decide",
                plugin.name()
            )));
            return;
        };

        match verdict(&returned) {
            Some(("block", reason)) => request.refuse(Blocked::new(reason)),
            Some(("block_and_stop", reason)) => {
                request.refuse(Blocked::new(reason).and_stop());
            }
            _ => {
                if let Some(arguments) = field_string(&returned, "arguments") {
                    request.arguments = arguments;
                }
            }
        }
    }
}

/// Let every script patch a result. They chain: each sees the last one's
/// edits.
fn tool_result(plugin: &ScriptPlugin, body: &FnPtr, result: &mut ToolResult) {
    {
        let mut payload = Map::new();
        payload.insert("id".into(), result.id.clone().into());
        payload.insert("name".into(), result.name.clone().into());
        payload.insert("arguments".into(), result.arguments.clone().into());
        payload.insert("turn".into(), i64::from(result.turn).into());
        payload.insert("content".into(), text_of(&result.content).into());
        payload.insert("is_error".into(), result.is_error.into());
        payload.insert(
            "details".into(),
            result
                .details
                .as_ref()
                .map_or(Dynamic::UNIT, convert::to_dynamic),
        );

        let Some(returned) = deliver(plugin, body, (payload,)) else {
            return;
        };
        let Some(patch) = as_map(&returned) else {
            return;
        };

        if let Some(content) = patch.get("content") {
            result.content = vec![ToolContent::Text(content.to_string())];
        }
        if let Some(flag) = patch.get("is_error").and_then(|v| v.as_bool().ok()) {
            result.is_error = flag;
        }
        if let Some(details) = patch.get("details") {
            result.details = if details.is_unit() {
                None
            } else {
                Some(convert::to_json(details))
            };
        }
    }
}

fn turn_end(plugin: &ScriptPlugin, body: &FnPtr, end: &mut TurnEnd) {
    {
        let mut payload = Map::new();
        payload.insert(
            "stop_reason".into(),
            stop_reason(end.summary.stop_reason).into(),
        );
        payload.insert(
            "tool_calls".into(),
            i64::try_from(end.summary.tool_calls)
                .unwrap_or(i64::MAX)
                .into(),
        );
        payload.insert("input".into(), i64::from(end.summary.usage.input).into());
        payload.insert("output".into(), i64::from(end.summary.usage.output).into());
        payload.insert(
            "error".into(),
            end.summary
                .error
                .as_ref()
                .map_or(Dynamic::UNIT, |text| text.clone().into()),
        );

        let script_cx = ScriptCx::new(&end.run);
        let returned = deliver(plugin, body, (script_cx, payload));

        if let Some(returned) = returned
            && matches!(verdict(&returned), Some(("stop", _)))
        {
            end.stop = true;
        }
    }
}

fn run_end(plugin: &ScriptPlugin, body: &FnPtr, end: &RunEnd) {
    let mut payload = Map::new();
    payload.insert("stop".into(), stop_reason(end.stop).into());
    payload.insert("turns".into(), i64::from(end.turns).into());
    payload.insert("input".into(), i64::from(end.run.usage.input).into());
    payload.insert("output".into(), i64::from(end.run.usage.output).into());
    payload.insert(
        "error".into(),
        end.error
            .as_ref()
            .map_or(Dynamic::UNIT, |text| text.clone().into()),
    );

    let cx = ScriptCx::new(&end.run);
    deliver(plugin, body, (cx, Dynamic::from_map(payload)));

    // A run is the natural save point: whatever a plugin learned this run is
    // on disk before the next prompt, and a session that never ends cleanly
    // still keeps everything up to its last turn.
    plugin.flush();
}
