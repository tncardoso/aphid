//! The agent loop, driven by a scripted backend so nothing here touches the
//! network.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_agent::Agent;
use aphid_agent::rt::{Component, Composition, Context, Uid};
use aphid_agent::testing::{Turn, scripted};
use aphid_agent::{
    Blocked, Moment, Prompt, ToolContent, ToolCx, ToolHandler, ToolOutcome, ToolProgress,
    ToolRequest, ToolResult, TurnEnd, TurnStart, tool_fn,
};
use aphid_core::{
    ContentInput, ContentRef, Event, MessageRef, Model, Role, StopReason, providers::deepseek,
};
use serde::Deserialize;

fn model() -> Model {
    deepseek::flash()
}

/// Something to hang listeners on.
///
/// Listeners are filed under the fiber that registered them, so that unloading
/// it takes them with it. Nothing here is ever unloaded — what these tests want
/// from a component is only its identity.
struct Anchor;

impl Component for Anchor {
    fn name(&self) -> &str {
        "anchor"
    }
    fn apply(&self, _ctx: &Context) -> Result<(), String> {
        Ok(())
    }
}

async fn composed() -> (Composition, Uid) {
    let composition = Composition::new();
    let owner = composition.plug(Anchor).await.expect("the anchor mounts");
    (composition, owner)
}

#[derive(Deserialize)]
struct Echo {
    value: String,
}

fn schema() -> aphid_core::Json {
    serde_json::json!({
        "type": "object",
        "properties": { "value": { "type": "string" } },
        "required": ["value"]
    })
}

/// A tool that hands back whatever it was given, so assertions can see the
/// arguments a handler actually received.
fn echo(name: &'static str) -> impl ToolHandler {
    tool_fn(
        name,
        "Echo a value.",
        schema(),
        |args: Echo, _cx: ToolCx| async move { ToolOutcome::text(args.value) },
    )
}

/// A tool that sleeps before echoing, and records the order in which the batch
/// actually completed.
fn slow(name: &'static str, millis: u64, completions: Arc<Mutex<Vec<String>>>) -> impl ToolHandler {
    tool_fn(
        name,
        "Echo a value, slowly.",
        schema(),
        move |args: Echo, _cx: ToolCx| {
            let completions = Arc::clone(&completions);
            async move {
                tokio::time::sleep(Duration::from_millis(millis)).await;
                completions.lock().expect("lock").push(name.to_owned());
                ToolOutcome::text(args.value)
            }
        },
    )
}

fn tool_result_names(agent: &Agent) -> Vec<String> {
    agent
        .transcript()
        .iter()
        .filter_map(|message| message.tool_result())
        .map(|meta| meta.tool_name.to_string())
        .collect()
}

fn tool_result_texts(agent: &Agent) -> Vec<String> {
    agent
        .transcript()
        .iter()
        .filter(|message| message.role() == Role::ToolResult)
        .map(|message| {
            message
                .content()
                .filter_map(|content| content.text())
                .collect::<String>()
        })
        .collect()
}

#[tokio::test]
async fn runs_a_tool_round_trip() {
    let (backend, script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"pong"}"#),
        Turn::text("done"),
    ]);

    let mut agent = Agent::builder()
        .model(model())
        .system("terse")
        .tool(echo("echo"))
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("ping").await;

    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.stop, StopReason::Stop);
    assert!(outcome.error.is_none());

    // system, user, assistant(tool call), tool result, assistant(answer)
    let roles: Vec<Role> = agent.transcript().iter().map(|m| m.role()).collect();
    assert_eq!(
        roles,
        vec![
            Role::System,
            Role::User,
            Role::Assistant,
            Role::ToolResult,
            Role::Assistant
        ]
    );
    assert_eq!(tool_result_texts(&agent), vec!["pong".to_owned()]);

    // The second request must have carried the tool result to the provider.
    let requests = script.requests();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].contains("pong"));
    assert!(requests[1].contains("pong"));
}

#[tokio::test]
async fn commits_results_in_source_order_not_completion_order() {
    let completions = Arc::new(Mutex::new(Vec::new()));

    let (backend, _script) = scripted([
        Turn::call("c1", "slow", r#"{"value":"a"}"#)
            .and_call("c2", "medium", r#"{"value":"b"}"#)
            .and_call("c3", "fast", r#"{"value":"c"}"#),
        Turn::text("done"),
    ]);

    let mut agent = Agent::builder()
        .model(model())
        .tool(slow("slow", 60, Arc::clone(&completions)))
        .tool(slow("medium", 30, Arc::clone(&completions)))
        .tool(slow("fast", 1, Arc::clone(&completions)))
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    // Committed in the order the assistant asked for them...
    assert_eq!(
        tool_result_names(&agent),
        vec!["slow".to_owned(), "medium".to_owned(), "fast".to_owned()]
    );
    assert_eq!(
        tool_result_texts(&agent),
        vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
    );

    // ...even though they ran concurrently and finished in the other order.
    let order = completions.lock().expect("lock").clone();
    assert_eq!(
        order,
        vec!["fast".to_owned(), "medium".to_owned(), "slow".to_owned()],
        "batch should have executed concurrently"
    );
}

#[tokio::test]
async fn a_plugin_can_block_a_tool_call() {
    let ran = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&ran);

    let guarded = tool_fn(
        "echo",
        "Echo a value.",
        schema(),
        move |args: Echo, _cx: ToolCx| {
            let seen = Arc::clone(&seen);
            async move {
                seen.fetch_add(1, Ordering::Relaxed);
                ToolOutcome::text(args.value)
            }
        },
    );

    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"pong"}"#),
        Turn::text("understood"),
    ]);

    let (composition, owner) = composed().await;
    composition.bus.on::<ToolRequest>(owner, |request| {
        if request.name == "echo" {
            request.refuse(Blocked::new("not allowed"));
        }
    });

    let mut agent = Agent::builder()
        .model(model())
        .tool(guarded)
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("ping").await;

    assert_eq!(ran.load(Ordering::Relaxed), 0, "handler must not run");

    let result = agent
        .transcript()
        .iter()
        .find(|message| message.role() == Role::ToolResult)
        .expect("a result is committed in the tool's place");
    assert!(result.tool_result().expect("meta").is_error);
    assert_eq!(tool_result_texts(&agent), vec!["not allowed".to_owned()]);
}

#[tokio::test]
async fn plugins_patch_arguments_in_order() {
    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"x"}"#),
        Turn::text("done"),
    ]);

    let (composition, owner) = composed().await;
    for suffix in ["-first", "-second"] {
        composition.bus.on::<ToolRequest>(owner, move |request| {
            request.arguments = format!(r#"{{"value":"{}{suffix}"}}"#, request.arguments.len());
        });
    }

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    // The first listener rewrote `{"value":"x"}` (15 bytes) to
    // `{"value":"15-first"}` (20 bytes); the second saw that and rewrote again.
    assert_eq!(tool_result_texts(&agent), vec!["20-second".to_owned()]);
}

#[tokio::test]
async fn plugins_chain_when_patching_results() {
    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"base"}"#),
        Turn::text("done"),
    ]);

    let (composition, owner) = composed().await;
    for suffix in ["-one", "-two"] {
        composition.bus.on::<ToolResult>(owner, move |result| {
            assert_eq!(result.name, "echo");
            let text = format!("{}{suffix}", text_of_result(&result.content));
            result.content = vec![ToolContent::Text(text)];
            result.details = Some(serde_json::json!({ "patched_by": suffix }));
        });
    }

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(tool_result_texts(&agent), vec!["base-one-two".to_owned()]);

    let meta = agent
        .transcript()
        .iter()
        .find_map(|message| message.tool_result())
        .expect("meta");
    // The later listener's details replace the earlier one's; nothing deep-merges.
    assert_eq!(
        meta.details,
        Some(serde_json::json!({ "patched_by": "-two" }))
    );
    // A field no listener touched keeps the handler's value.
    assert!(!meta.is_error);
}

#[tokio::test]
async fn an_unknown_tool_becomes_an_error_result() {
    let (backend, _script) = scripted([Turn::call("call_1", "nope", "{}"), Turn::text("sorry")]);

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    let meta = agent
        .transcript()
        .iter()
        .find_map(|message| message.tool_result())
        .expect("meta");
    assert!(meta.is_error);
    assert_eq!(meta.tool_name, "nope");
    assert!(tool_result_texts(&agent)[0].contains("not a registered tool"));
}

#[tokio::test]
async fn max_turns_caps_a_runaway_loop() {
    let (backend, script) =
        scripted((0..10).map(|i| Turn::call(format!("call_{i}"), "echo", r#"{"value":"again"}"#)));

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .max_turns(3)
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("go").await;

    assert_eq!(outcome.turns, 3);
    assert_eq!(script.request_count(), 3);
}

#[tokio::test]
async fn a_plugin_can_stop_the_run() {
    let (backend, script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"pong"}"#),
        Turn::text("never reached"),
    ]);

    let (composition, owner) = composed().await;
    composition.bus.on::<TurnEnd>(owner, |end| end.stop = true);

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .compose(&composition)
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("go").await;

    assert_eq!(outcome.turns, 1);
    assert_eq!(script.request_count(), 1);
    // The tool still ran and its result is committed, so the run can resume.
    assert_eq!(tool_result_texts(&agent), vec!["pong".to_owned()]);
}

/// A tool that asks the run to end after its batch.
fn terminating(name: &'static str) -> impl ToolHandler {
    tool_fn(
        name,
        "Echo a value and ask to stop.",
        schema(),
        |args: Echo, _cx: ToolCx| async move { ToolOutcome::text(args.value).terminating() },
    )
}

#[tokio::test]
async fn terminate_needs_the_whole_batch_to_agree() {
    // One terminating tool alongside an ordinary one: the run continues.
    let (backend, script) = scripted([
        Turn::call("c1", "stop", r#"{"value":"a"}"#).and_call("c2", "echo", r#"{"value":"b"}"#),
        Turn::text("done"),
    ]);

    let mut agent = Agent::builder()
        .model(model())
        .tool(terminating("stop"))
        .tool(echo("echo"))
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("go").await;
    assert_eq!(outcome.turns, 2, "one dissenting tool keeps the run going");
    assert_eq!(script.request_count(), 2);

    // Every tool terminating: the run stops without another request.
    let (backend, script) = scripted([
        Turn::call("c1", "stop", r#"{"value":"a"}"#).and_call("c2", "halt", r#"{"value":"b"}"#),
        Turn::text("never reached"),
    ]);

    let mut agent = Agent::builder()
        .model(model())
        .tool(terminating("stop"))
        .tool(terminating("halt"))
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("go").await;
    assert_eq!(outcome.turns, 1);
    assert_eq!(script.request_count(), 1);
}

#[tokio::test]
async fn cancelling_ends_the_run_as_aborted() {
    let (backend, script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"pong"}"#),
        Turn::text("never reached"),
    ]);

    let (composition, owner) = composed().await;
    composition.bus.on::<TurnEnd>(owner, |end| end.run.cancel());

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .compose(&composition)
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("go").await;

    assert_eq!(outcome.stop, StopReason::Aborted);
    assert_eq!(script.request_count(), 1);
    assert!(agent.handle().is_cancelled());
}

#[tokio::test]
async fn a_provider_failure_ends_the_run_without_panicking() {
    let (backend, script) = scripted([Turn::failed("provider returned HTTP 500")]);

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("go").await;

    assert!(outcome.is_failure());
    assert_eq!(outcome.error.as_deref(), Some("provider returned HTTP 500"));
    assert_eq!(script.request_count(), 1);
}

#[tokio::test]
async fn only_what_subscribed_is_dispatched_to() {
    let counted = Arc::new(AtomicUsize::new(0));
    let (backend, _script) = scripted([Turn::text("hello")]);

    let (composition, owner) = composed().await;
    let events = Arc::clone(&counted);
    composition.stream.subscribe(owner, move |_event, _cx| {
        events.fetch_add(1, Ordering::Relaxed);
    });

    let mut agent = Agent::builder()
        .model(model())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    // Start, BlockStart, Delta, BlockEnd, Done.
    assert_eq!(counted.load(Ordering::Relaxed), 5);
}

#[tokio::test]
async fn a_response_nobody_watches_dispatches_nothing() {
    let (backend, _script) = scripted([Turn::text("hello")]);
    let (composition, _owner) = composed().await;

    // Nothing subscribed, so the loop can skip the whole per-token path — and
    // says so before it streams a single one.
    assert!(!composition.stream.is_observed());

    let mut agent = Agent::builder()
        .model(model())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("go").await;
    assert_eq!(outcome.turns, 1);
}

#[tokio::test]
async fn a_stream_listener_can_resolve_delta_spans() {
    let text = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&text);

    let (backend, _script) = scripted([Turn::text("streamed answer")]);

    let (composition, owner) = composed().await;
    composition.stream.subscribe(owner, move |event, cx| {
        if let Event::Delta { span, .. } = event {
            sink.lock().expect("lock").push_str(cx.text(*span));
        }
    });

    let mut agent = Agent::builder()
        .model(model())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(text.lock().expect("lock").as_str(), "streamed answer");
}

/// Contributes a tool of its own, shadowing a built-in of the same name.
struct ToolProvider;

impl Component for ToolProvider {
    fn name(&self) -> &str {
        "tool-provider"
    }

    fn apply(&self, _ctx: &Context) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn a_component_tool_shadows_one_of_the_same_name() {
    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"pong"}"#),
        Turn::text("done"),
    ]);

    let composition = Composition::new();
    let owner = composition.plug(ToolProvider).await.expect("mounts");
    composition.tools.register(
        owner,
        Arc::new(tool_fn(
            "echo",
            "A component's own echo, shadowing the built-in.",
            schema(),
            |args: Echo, _cx: ToolCx| async move {
                ToolOutcome::text(format!("component:{}", args.value))
            },
        )),
    );

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .compose(&composition)
        .stream_fn(backend)
        .build();

    assert_eq!(agent.tools().len(), 1, "shadowing replaces, not appends");

    agent.prompt("go").await;

    assert_eq!(tool_result_texts(&agent), vec!["component:pong".to_owned()]);
}

#[tokio::test]
async fn a_listener_can_add_context_before_a_request() {
    let (backend, script) = scripted([Turn::text("brief")]);

    let (composition, owner) = composed().await;
    composition
        .bus
        .on::<TurnStart>(owner, |start| start.0.note("remember: be brief"));

    let mut agent = Agent::builder()
        .model(model())
        .system("base prompt")
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert!(script.requests()[0].contains("remember: be brief"));
    // The note stays in the transcript, so the run replays exactly as it ran.
    assert_eq!(agent.transcript().len(), 4);
}

#[tokio::test]
async fn set_system_rebuilds_the_transcript() {
    let (backend, _script) = scripted([Turn::text("first"), Turn::text("second")]);

    let mut agent = Agent::builder()
        .model(model())
        .system("original")
        .stream_fn(backend)
        .build();

    agent.prompt("one").await;
    let before = agent.transcript().len();

    agent.set_system("replaced");

    assert_eq!(agent.transcript().len(), before);
    let system = agent.transcript().get(0).expect("system message");
    assert_eq!(system.role(), Role::System);
    assert_eq!(
        system
            .content()
            .filter_map(|c| c.text())
            .collect::<String>(),
        "replaced"
    );
    // The rest of the conversation survived the rebuild.
    assert_eq!(agent.transcript().get(1).unwrap().role(), Role::User);
    assert_eq!(agent.transcript().get(2).unwrap().role(), Role::Assistant);
}

#[tokio::test]
async fn resume_continues_without_a_new_prompt() {
    let (backend, script) = scripted([Turn::text("first"), Turn::text("second")]);

    let mut agent = Agent::builder().model(model()).stream_fn(backend).build();

    agent.prompt("go").await;
    assert_eq!(script.request_count(), 1);

    agent.resume().await;
    assert_eq!(script.request_count(), 2);
    assert_eq!(agent.transcript().len(), 3);
}

/// A tool that reports its work line by line before returning.
fn chatty(name: &'static str) -> impl ToolHandler {
    tool_fn(
        name,
        "Echo a value, noisily.",
        schema(),
        |args: Echo, cx: ToolCx| async move {
            assert_eq!(cx.tool(), "chatty");
            assert_eq!(cx.call_id(), "call_1");
            for part in args.value.split(',') {
                cx.progress(part);
            }
            ToolOutcome::text(args.value)
        },
    )
}

#[tokio::test]
async fn tool_progress_reaches_a_listener_in_order() {
    let chunks = Arc::new(Mutex::new(Vec::new()));

    let (backend, _script) = scripted([
        Turn::call("call_1", "chatty", r#"{"value":"a,b,c"}"#),
        Turn::text("done"),
    ]);

    let (composition, owner) = composed().await;
    let seen = Arc::clone(&chunks);
    composition.bus.on::<ToolProgress>(owner, move |progress| {
        seen.lock().expect("lock").push(format!(
            "{}/{}/{}",
            progress.call_id, progress.tool, progress.chunk
        ));
    });

    let mut agent = Agent::builder()
        .model(model())
        .tool(chatty("chatty"))
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(
        chunks.lock().expect("lock").clone(),
        vec![
            "call_1/chatty/a".to_owned(),
            "call_1/chatty/b".to_owned(),
            "call_1/chatty/c".to_owned(),
        ]
    );
    // The final result is still the authoritative output.
    assert_eq!(tool_result_texts(&agent), vec!["a,b,c".to_owned()]);
}

#[tokio::test]
async fn a_tool_can_tell_when_nobody_is_watching_progress() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&observed);

    let probe = tool_fn(
        "probe",
        "Report whether progress is observed.",
        schema(),
        move |_args: Echo, cx: ToolCx| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().expect("lock").push(cx.is_observed());
                ToolOutcome::text("ok")
            }
        },
    );

    let (backend, _script) = scripted([
        Turn::call("call_1", "probe", r#"{"value":"x"}"#),
        Turn::text("done"),
    ]);

    // A composition with a component on it but nothing listening for progress.
    let (composition, _owner) = composed().await;

    let mut agent = Agent::builder()
        .model(model())
        .tool(probe)
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(observed.lock().expect("lock").clone(), vec![false]);
}

/// The concatenated text of a tool result.
fn text_of_result(content: &[ToolContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ToolContent::Text(text) => Some(text.as_str()),
            ToolContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The concatenated text of a message.
fn text_of(message: MessageRef<'_>) -> String {
    message
        .content()
        .filter_map(|part| match part {
            ContentRef::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect()
}

/// A composition that rewrites every prompt, and turns away one of them.
async fn gatekept() -> Composition {
    let (composition, owner) = composed().await;
    composition.bus.on::<Prompt>(owner, |prompt| {
        if prompt.text.contains("secret") {
            prompt.reject("that one is off limits");
            return;
        }
        prompt.text = format!("{} (reviewed)", prompt.text);
    });
    composition
}

#[tokio::test]
async fn a_prompt_hook_rewrites_before_the_transcript_sees_it() {
    let (backend, script) = scripted([Turn::text("done")]);

    let composition = gatekept().await;
    let mut agent = Agent::builder()
        .model(model())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    let user = agent.transcript().get(0).expect("a user message");
    assert_eq!(user.role(), Role::User);
    assert_eq!(text_of(user), "go (reviewed)");
    assert_eq!(script.request_count(), 1);
}

#[tokio::test]
async fn a_rejected_prompt_appends_nothing_and_sends_nothing() {
    let (backend, script) = scripted([Turn::text("never reached")]);

    let composition = gatekept().await;
    let mut agent = Agent::builder()
        .model(model())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("tell me the secret").await;

    assert_eq!(outcome.stop, StopReason::Aborted);
    assert_eq!(outcome.turns, 0);
    assert_eq!(outcome.error.as_deref(), Some("that one is off limits"));
    assert_eq!(agent.transcript().len(), 0, "the transcript is untouched");
    assert_eq!(script.request_count(), 0, "no request was sent");
}

#[tokio::test]
async fn rewriting_mixed_content_keeps_the_attachments() {
    let (backend, _script) = scripted([Turn::text("done")]);

    let composition = gatekept().await;
    let mut agent = Agent::builder()
        .model(model())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent
        .prompt_parts(&[
            ContentInput::Text("look"),
            ContentInput::Image {
                data: &[1, 2, 3],
                mime: "image/png",
            },
        ])
        .await;

    let user = agent.transcript().get(0).expect("a user message");
    assert_eq!(text_of(user), "look (reviewed)");
    let images = user
        .content()
        .filter(|part| matches!(part, ContentRef::Image(_)))
        .count();
    assert_eq!(images, 1, "the image survived the rewrite");
}

#[tokio::test]
async fn a_transcript_listener_sees_every_committed_response() {
    let seen = Arc::new(Mutex::new(Vec::new()));

    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"x"}"#),
        Turn::text("done"),
    ]);

    let (composition, owner) = composed().await;
    let recorded = Arc::clone(&seen);
    // Reading the transcript is what this needs, so it listens where transcript
    // readers listen rather than on the bus.
    composition
        .transcript
        .subscribe(owner, move |moment, transcript, _run| {
            if moment != Moment::Message {
                return;
            }
            let last = transcript.len().saturating_sub(1);
            let id = transcript.id_at(last).expect("a committed message");
            // Committed by the time this fires, so it is readable.
            assert_eq!(transcript.message(id).role(), Role::Assistant);
            recorded.lock().expect("lock").push(id);
        });

    let mut agent = Agent::builder()
        .model(model())
        .tool(tool_fn(
            "echo",
            "Echo a value.",
            schema(),
            |args: Echo, _cx: ToolCx| async move { ToolOutcome::text(args.value) },
        ))
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(seen.lock().expect("lock").len(), 2, "one per turn");
}
