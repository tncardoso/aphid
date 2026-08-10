//! The agent loop, driven by a scripted backend so nothing here touches the
//! network.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_agent::testing::{Turn, scripted};
use aphid_agent::{
    Agent, Cx, Flow, Guard, Interest, PendingCall, Plugin, ResultCx, StreamCx, ToolCx, ToolHandler,
    ToolOutcome, TurnSummary, tool_fn,
};
use aphid_core::{Event, Model, Role, StopReason, providers::deepseek};
use serde::Deserialize;

fn model() -> Model {
    deepseek::flash()
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

struct Blocker;

impl Plugin for Blocker {
    fn name(&self) -> &str {
        "blocker"
    }

    fn interests(&self) -> Interest {
        Interest::TOOL_CALL
    }

    fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
        if call.name() == "echo" {
            return Guard::block("not allowed");
        }
        Guard::Allow
    }
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

    let mut agent = Agent::builder()
        .model(model())
        .tool(guarded)
        .plugin(Blocker)
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

struct Patcher(&'static str);

impl Plugin for Patcher {
    fn name(&self) -> &str {
        "patcher"
    }

    fn interests(&self) -> Interest {
        Interest::TOOL_CALL
    }

    fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
        let patched = format!(r#"{{"value":"{}{}"}}"#, call.arguments().len(), self.0);
        call.set_arguments(patched);
        Guard::Allow
    }
}

#[tokio::test]
async fn plugins_patch_arguments_in_order() {
    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"x"}"#),
        Turn::text("done"),
    ]);

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .plugin(Patcher("-first"))
        .plugin(Patcher("-second"))
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    // The first plugin rewrote `{"value":"x"}` (15 bytes) to
    // `{"value":"15-first"}` (20 bytes); the second saw that and rewrote again.
    assert_eq!(tool_result_texts(&agent), vec!["20-second".to_owned()]);
}

struct Suffix(&'static str);

impl Plugin for Suffix {
    fn name(&self) -> &str {
        "suffix"
    }

    fn interests(&self) -> Interest {
        Interest::TOOL_RESULT
    }

    fn on_tool_result(&self, outcome: &mut ToolOutcome, cx: &ResultCx<'_>) {
        assert_eq!(cx.name(), "echo");
        let text = format!("{}{}", outcome.text_content(), self.0);
        outcome.content = vec![aphid_agent::ToolContent::Text(text)];
        outcome.details = Some(serde_json::json!({ "patched_by": self.0 }));
    }
}

#[tokio::test]
async fn plugins_chain_when_patching_results() {
    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"base"}"#),
        Turn::text("done"),
    ]);

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .plugin(Suffix("-one"))
        .plugin(Suffix("-two"))
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(tool_result_texts(&agent), vec!["base-one-two".to_owned()]);

    let meta = agent
        .transcript()
        .iter()
        .find_map(|message| message.tool_result())
        .expect("meta");
    // The later plugin's details replace the earlier one's; nothing deep-merges.
    assert_eq!(
        meta.details,
        Some(serde_json::json!({ "patched_by": "-two" }))
    );
    // A field no plugin touched keeps the handler's value.
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

struct StopAfterFirst;

impl Plugin for StopAfterFirst {
    fn name(&self) -> &str {
        "stop-after-first"
    }

    fn interests(&self) -> Interest {
        Interest::TURN_END
    }

    fn on_turn_end(&self, _cx: &mut Cx<'_>, _turn: &TurnSummary) -> Flow {
        Flow::Stop
    }
}

#[tokio::test]
async fn a_plugin_can_stop_the_run() {
    let (backend, script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"pong"}"#),
        Turn::text("never reached"),
    ]);

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .plugin(StopAfterFirst)
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

struct CancelOnFirstTurn;

impl Plugin for CancelOnFirstTurn {
    fn name(&self) -> &str {
        "canceller"
    }

    fn interests(&self) -> Interest {
        Interest::TURN_END
    }

    fn on_turn_end(&self, cx: &mut Cx<'_>, _turn: &TurnSummary) -> Flow {
        cx.cancel();
        Flow::Continue
    }
}

#[tokio::test]
async fn cancelling_ends_the_run_as_aborted() {
    let (backend, script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"pong"}"#),
        Turn::text("never reached"),
    ]);

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .plugin(CancelOnFirstTurn)
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

struct Counter {
    interests: Interest,
    events: Arc<AtomicUsize>,
}

impl Plugin for Counter {
    fn name(&self) -> &str {
        "counter"
    }

    fn interests(&self) -> Interest {
        self.interests
    }

    fn on_event(&self, _event: &Event, _cx: &StreamCx<'_>) {
        self.events.fetch_add(1, Ordering::Relaxed);
    }
}

#[tokio::test]
async fn interests_gate_the_dispatch() {
    let subscribed = Arc::new(AtomicUsize::new(0));
    let unsubscribed = Arc::new(AtomicUsize::new(0));

    let (backend, _script) = scripted([Turn::text("hello")]);

    let mut agent = Agent::builder()
        .model(model())
        .plugin(Counter {
            interests: Interest::EVENT,
            events: Arc::clone(&subscribed),
        })
        .plugin(Counter {
            interests: Interest::empty(),
            events: Arc::clone(&unsubscribed),
        })
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    // Start, BlockStart, Delta, BlockEnd, Done.
    assert_eq!(subscribed.load(Ordering::Relaxed), 5);
    assert_eq!(unsubscribed.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn on_event_can_resolve_delta_spans() {
    let text = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&text);

    let (backend, _script) = scripted([Turn::text("streamed answer")]);

    let mut agent = Agent::builder()
        .model(model())
        .on_event(move |event, cx| {
            if let Event::Delta { span, .. } = event {
                sink.lock().expect("lock").push_str(cx.text(*span));
            }
        })
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(text.lock().expect("lock").as_str(), "streamed answer");
}

struct ToolProvider;

impl Plugin for ToolProvider {
    fn name(&self) -> &str {
        "tool-provider"
    }

    fn tools(&self) -> Vec<Arc<dyn ToolHandler>> {
        vec![Arc::new(tool_fn(
            "echo",
            "A plugin's own echo, shadowing the built-in.",
            schema(),
            |args: Echo, _cx: ToolCx| async move { ToolOutcome::text(format!("plugin:{}", args.value)) },
        ))]
    }
}

#[tokio::test]
async fn a_plugin_tool_shadows_one_of_the_same_name() {
    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"pong"}"#),
        Turn::text("done"),
    ]);

    let mut agent = Agent::builder()
        .model(model())
        .tool(echo("echo"))
        .plugin(ToolProvider)
        .stream_fn(backend)
        .build();

    assert_eq!(agent.tools().len(), 1, "shadowing replaces, not appends");

    agent.prompt("go").await;

    assert_eq!(tool_result_texts(&agent), vec!["plugin:pong".to_owned()]);
}

struct ContextInjector;

impl Plugin for ContextInjector {
    fn name(&self) -> &str {
        "context-injector"
    }

    fn interests(&self) -> Interest {
        Interest::TURN_START
    }

    fn on_turn_start(&self, cx: &mut Cx<'_>) {
        cx.push_system_note("remember: be brief");
    }
}

#[tokio::test]
async fn a_plugin_can_add_context_before_a_request() {
    let (backend, script) = scripted([Turn::text("brief")]);

    let mut agent = Agent::builder()
        .model(model())
        .system("base prompt")
        .plugin(ContextInjector)
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
