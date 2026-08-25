//! The loop, seen through the events it announces.
//!
//! Everything here goes through the bus rather than a plugin, which is the
//! point: a component subscribes to what it cares about and the loop does not
//! know it exists.

use std::sync::{Arc, Mutex};

use aphid_agent::rt::Uid;
use aphid_agent::testing::{Turn, scripted};
use aphid_agent::{
    Agent, Blocked, Prompt, Run, RunEnd, RunStart, ToolArguments, ToolOutcome, ToolRequest,
    ToolResult, TurnEnd, TurnStart, tool_fn,
};
use aphid_core::providers::deepseek;
use serde::Deserialize;

#[derive(Deserialize)]
struct Echo {
    text: String,
}

fn echo_tool() -> impl aphid_agent::ToolHandler {
    tool_fn(
        "echo",
        "Repeat the text back.",
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }),
        |args: Echo, _cx| async move { ToolOutcome::text(args.text) },
    )
}

/// Any distinct owner will do; listeners here are never unsubscribed.
fn owner() -> Uid {
    aphid_agent::rt::Runtime::new()
        .mount(Arc::new(Nothing), serde_json::Value::Null)
        .expect("mounts")
}

struct Nothing;
impl aphid_agent::rt::Component for Nothing {
    fn name(&self) -> &str {
        "nothing"
    }
    fn apply(&self, _ctx: &aphid_agent::rt::Context) -> Result<(), String> {
        Ok(())
    }
}

/// The text of a message, joined across its blocks.
fn text_of(message: &aphid_core::MessageRef<'_>) -> String {
    message
        .content()
        .filter_map(|block| block.text())
        .collect::<Vec<_>>()
        .join("")
}

/// The first message with this role, as text.
fn first_text(agent: &Agent, role: aphid_core::Role) -> Option<String> {
    (0..agent.transcript().len())
        .filter_map(|index| agent.transcript().get(index))
        .find(|message| message.role() == role)
        .map(|message| text_of(&message))
}

#[tokio::test]
async fn the_loop_announces_its_shape_in_order() {
    let (backend, _script) = scripted([Turn::text("done.")]);
    let agent = Agent::builder()
        .model(deepseek::flash())
        .stream_fn(backend)
        .build();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    agent
        .bus()
        .on::<RunStart>(owner(), move |_| log.lock().expect("ok").push("run-start"));
    let log = Arc::clone(&seen);
    agent
        .bus()
        .on::<TurnStart>(owner(), move |_| log.lock().expect("ok").push("turn-start"));
    let log = Arc::clone(&seen);
    agent
        .bus()
        .on::<TurnEnd>(owner(), move |_| log.lock().expect("ok").push("turn-end"));
    let log = Arc::clone(&seen);
    agent
        .bus()
        .on::<RunEnd>(owner(), move |_| log.lock().expect("ok").push("run-end"));

    let mut agent = agent;
    agent.prompt("hello").await;

    assert_eq!(
        *seen.lock().expect("ok"),
        ["run-start", "turn-start", "turn-end", "run-end"]
    );
}

#[tokio::test]
async fn a_listener_rewrites_the_prompt_before_it_is_appended() {
    let (backend, _script) = scripted([Turn::text("ok")]);
    let agent = Agent::builder()
        .model(deepseek::flash())
        .stream_fn(backend)
        .build();

    agent.bus().on::<Prompt>(owner(), |prompt| {
        prompt.text = prompt.text.replace("Lisbon", "Porto");
    });

    let mut agent = agent;
    agent.prompt("weather in Lisbon?").await;

    let user = first_text(&agent, aphid_core::Role::User).expect("the prompt was appended");
    assert_eq!(user, "weather in Porto?");
}

#[tokio::test]
async fn a_rejected_prompt_leaves_the_transcript_untouched() {
    let (backend, _script) = scripted([Turn::text("ok")]);
    let agent = Agent::builder()
        .model(deepseek::flash())
        .stream_fn(backend)
        .build();

    agent
        .bus()
        .on::<Prompt>(owner(), |prompt| prompt.reject("not today"));

    let mut agent = agent;
    let outcome = agent.prompt("anything").await;

    assert_eq!(outcome.error.as_deref(), Some("not today"));
    assert_eq!(agent.transcript().len(), 0);
}

#[tokio::test]
async fn a_listener_adds_context_before_a_request() {
    let (backend, _script) = scripted([Turn::text("ok")]);
    let agent = Agent::builder()
        .model(deepseek::flash())
        .stream_fn(backend)
        .build();

    agent
        .bus()
        .on::<TurnStart>(owner(), |start: &mut TurnStart| {
            start.0.note("today is a Tuesday");
        });

    let mut agent = agent;
    agent.prompt("hi").await;

    let notes: Vec<String> = (0..agent.transcript().len())
        .filter_map(|index| agent.transcript().get(index))
        .filter(|message| message.role() == aphid_core::Role::System)
        .map(|message| text_of(&message))
        .collect();
    assert_eq!(notes, ["today is a Tuesday"]);
}

#[tokio::test]
async fn a_guard_blocks_a_tool_and_the_model_reads_why() {
    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"text":"secret"}"#),
        Turn::text("understood."),
    ]);
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .tool(echo_tool())
        .stream_fn(backend)
        .build();

    agent.bus().on::<ToolRequest>(owner(), |request| {
        if request.arguments.contains("secret") {
            request.refuse(Blocked::new("secrets are off limits"));
        }
    });

    agent.prompt("echo the secret").await;

    let result = first_text(&agent, aphid_core::Role::ToolResult)
        .expect("a blocked call still commits a result");
    assert!(result.contains("secrets are off limits"), "{result}");
}

#[tokio::test]
async fn a_listener_rewrites_tool_arguments_without_blocking() {
    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"text":"before"}"#),
        Turn::text("done"),
    ]);
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .tool(echo_tool())
        .stream_fn(backend)
        .build();

    agent.bus().on_waterfall::<ToolArguments>(
        owner(),
        |args: String, next: aphid_agent::rt::Next<'_, ToolArguments>| {
            next.run(args.replace("before", "after"))
        },
    );

    agent.prompt("echo").await;

    let result = first_text(&agent, aphid_core::Role::ToolResult).expect("the tool ran");
    assert_eq!(result, "after");
}

#[tokio::test]
async fn a_listener_patches_a_tool_result() {
    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"text":"raw"}"#),
        Turn::text("done"),
    ]);
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .tool(echo_tool())
        .stream_fn(backend)
        .build();

    agent.bus().on::<ToolResult>(owner(), |result| {
        result.content = vec![aphid_agent::ToolContent::Text("redacted".to_owned())];
    });

    agent.prompt("echo").await;

    let result = first_text(&agent, aphid_core::Role::ToolResult).expect("the tool ran");
    assert_eq!(result, "redacted");
}

#[tokio::test]
async fn a_listener_stops_the_run_after_a_turn() {
    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"text":"one"}"#),
        Turn::call("call_2", "echo", r#"{"text":"two"}"#),
        Turn::text("never reached"),
    ]);
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .tool(echo_tool())
        .stream_fn(backend)
        .build();

    agent.bus().on::<TurnEnd>(owner(), |end: &mut TurnEnd| {
        end.stop = true;
    });

    let outcome = agent.prompt("go").await;
    assert_eq!(outcome.turns, 1);
}

#[tokio::test]
async fn the_token_stream_reaches_a_listener_without_the_bus() {
    let (backend, _script) = scripted([Turn::text("hello there")]);
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .stream_fn(backend)
        .build();

    let deltas = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&deltas);
    agent
        .stream_listeners()
        .subscribe(owner(), move |event, cx| {
            if let aphid_core::Event::Delta { span, .. } = event {
                sink.lock().expect("ok").push_str(cx.text(*span));
            }
        });

    agent.prompt("hi").await;
    assert_eq!(*deltas.lock().expect("ok"), "hello there");
}

#[tokio::test]
async fn nothing_listening_means_nothing_dispatched() {
    let (backend, _script) = scripted([Turn::text("quiet")]);
    let agent = Agent::builder()
        .model(deepseek::flash())
        .stream_fn(backend)
        .build();

    assert!(!agent.bus().has_listeners::<RunStart>());
    assert!(!agent.stream_listeners().is_observed());

    let mut agent = agent;
    let outcome = agent.prompt("hi").await;
    assert_eq!(outcome.turns, 1);
}

#[tokio::test]
async fn a_run_payload_can_be_answered_from_another_task() {
    let (backend, _script) = scripted([Turn::text("ok")]);
    let agent = Agent::builder()
        .model(deepseek::flash())
        .stream_fn(backend)
        .build();

    // The payload holds no borrow, so a listener may keep it and hand it on.
    let captured: Arc<Mutex<Option<Run>>> = Arc::default();
    let slot = Arc::clone(&captured);
    agent
        .bus()
        .on::<RunStart>(owner(), move |start: &mut RunStart| {
            *slot.lock().expect("ok") = Some(start.0.clone());
        });

    let mut agent = agent;
    agent.prompt("hi").await;

    let run = captured.lock().expect("ok").take().expect("captured");
    assert_eq!(run.turn, 0);
    assert_eq!(run.model.id, deepseek::flash().id);
}
