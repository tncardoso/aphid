//! Plugins driven through a real agent run, with a scripted backend so nothing
//! here touches the network.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aphid_agent::Sink;
use aphid_agent::exec;
use aphid_agent::rt::Composition;
use aphid_agent::testing::{Turn, scripted};
use aphid_agent::{Agent, ToolCx, ToolOutcome, tool_fn};
use aphid_code::scripting::{Capabilities, PluginHost, discover};
use aphid_core::{ContentRef, MessageRef, Role, StopReason, providers::deepseek};
use serde::Deserialize;

/// A scratch workspace holding one project plugin, removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str, source: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("aphid-plugin-{}-{}", std::process::id(), unique()));
        let dir = root.join(".aphid").join("plugins");
        std::fs::create_dir_all(&dir).expect("create the plugin directory");
        std::fs::write(dir.join(format!("{name}.rhai")), source).expect("write the plugin");
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Collects everything a script says, so a test can assert on it.
#[derive(Clone, Default)]
struct Recorder {
    lines: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("lock").clone()
    }
}

impl Sink for Recorder {
    fn notify(&self, plugin: &str, text: &str) {
        self.lines
            .lock()
            .expect("lock")
            .push(format!("{plugin}: {text}"));
    }

    fn log(&self, plugin: &str, text: &str) {
        self.notify(plugin, text);
    }

    fn prompt(&self, plugin: &str, text: &str) {
        self.lines
            .lock()
            .expect("lock")
            .push(format!("{plugin} prompts: {text}"));
    }
}

/// Load one plugin out of a fixture, with the filesystem confined to it.
fn host(fixture: &Fixture, sink: &Recorder) -> Arc<PluginHost> {
    host_recording(fixture, sink, &Arc::new(exec::Registry::new()))
}

/// The same, keeping hold of the registry the plugin's commands are recorded in.
fn host_recording(
    fixture: &Fixture,
    sink: &Recorder,
    processes: &Arc<exec::Registry>,
) -> Arc<PluginHost> {
    let (files, problems) = discover(fixture.root(), None);
    assert!(problems.is_empty(), "discovery problems: {problems:?}");
    assert_eq!(files.len(), 1, "one plugin was written");

    let caps = Capabilities::full(fixture.root());
    let (host, diagnostics) = PluginHost::load(&files, &caps, Arc::new(sink.clone()), processes);
    assert!(diagnostics.is_empty(), "load problems: {diagnostics:?}");
    Arc::new(host)
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

fn echo_tool() -> impl aphid_agent::ToolHandler {
    tool_fn(
        "echo",
        "Echo a value.",
        schema(),
        |args: Echo, _cx: ToolCx| async move { ToolOutcome::text(args.value) },
    )
}

fn text_of(message: MessageRef<'_>) -> String {
    message
        .content()
        .filter_map(|part| match part {
            ContentRef::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_script_blocks_a_tool_call() {
    let fixture = Fixture::new(
        "guard",
        r#"

        fn apply(ctx) {
            on("agent/tool-call", |tool| {
                if tool.name == "echo" && tool.arguments.contains("forbidden") {
                    return block("not that one");
                }
            });
        }
"#,
    );
    let sink = Recorder::default();

    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"forbidden"}"#),
        Turn::text("understood"),
    ]);

    let composition = compose(host(&fixture, &sink)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .tool(echo_tool())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    let result = agent
        .transcript()
        .get(2)
        .expect("a tool result was committed");
    assert_eq!(result.role(), Role::ToolResult);
    assert!(
        text_of(result).contains("not that one"),
        "the block reason reaches the model: {}",
        text_of(result)
    );
}

#[tokio::test]
async fn a_script_patches_tool_arguments() {
    let fixture = Fixture::new(
        "rewrite",
        r#"

        fn apply(ctx) {
            on("agent/tool-call", |tool| {
                return #{ arguments: `{"value":"patched"}` };
            });
        }
"#,
    );
    let sink = Recorder::default();

    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"original"}"#),
        Turn::text("done"),
    ]);

    let composition = compose(host(&fixture, &sink)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .tool(echo_tool())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    let result = agent.transcript().get(2).expect("a tool result");
    assert_eq!(text_of(result), "patched");
}

#[tokio::test]
async fn a_script_rejects_a_prompt() {
    let fixture = Fixture::new(
        "gate",
        r#"

        fn apply(ctx) {
            on("agent/prompt", |draft| {
                if draft.text.contains("secret") {
                    return reject("not that");
                }
                return #{ text: draft.text + " (reviewed)" };
            });
        }
"#,
    );
    let sink = Recorder::default();

    let (backend, script) = scripted([Turn::text("never reached")]);

    let composition = compose(host(&fixture, &sink)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("tell me the secret").await;

    assert_eq!(outcome.stop, StopReason::Aborted);
    assert_eq!(outcome.error.as_deref(), Some("not that"));
    assert_eq!(agent.transcript().len(), 0);
    assert_eq!(script.request_count(), 0);
}

#[tokio::test]
async fn a_script_rewrites_a_prompt() {
    let fixture = Fixture::new(
        "gate",
        r#"

        fn apply(ctx) {
            on("agent/prompt", |draft| {
                return #{ text: draft.text + " (reviewed)" };
            });
        }
"#,
    );
    let sink = Recorder::default();

    let (backend, _script) = scripted([Turn::text("done")]);

    let composition = compose(host(&fixture, &sink)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(
        text_of(agent.transcript().get(0).expect("a prompt")),
        "go (reviewed)"
    );
}

#[tokio::test]
async fn a_script_adds_context_and_stops_the_run() {
    let fixture = Fixture::new(
        "steer",
        r#"

        fn apply(ctx) {
            on("agent/turn-start", |cx| {
                cx.note("turn " + cx.turn + " on " + cx.model);
            });

            on("agent/turn-end", |cx, turn| {
                if turn.tool_calls == 0 {
                    return stop();
                }
            });
        }
"#,
    );
    let sink = Recorder::default();

    let (backend, script) = scripted([Turn::text("first"), Turn::text("second")]);

    let composition = compose(host(&fixture, &sink)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(script.request_count(), 1, "the run stopped after one turn");

    let note = agent.transcript().get(1).expect("the note");
    assert_eq!(note.role(), Role::System);
    assert!(
        text_of(note).starts_with("turn 0 on "),
        "cx exposes the turn and model: {}",
        text_of(note)
    );
}

#[tokio::test]
async fn a_script_patches_a_tool_result() {
    let fixture = Fixture::new(
        "redact",
        r#"

        fn apply(ctx) {
            on("agent/tool-result", |result| {
                if result.name == "echo" {
                    return #{ content: "[redacted]", is_error: false };
                }
            });
        }
"#,
    );
    let sink = Recorder::default();

    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"a password"}"#),
        Turn::text("done"),
    ]);

    let composition = compose(host(&fixture, &sink)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .tool(echo_tool())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(
        text_of(agent.transcript().get(2).expect("a result")),
        "[redacted]"
    );
}

#[tokio::test]
async fn a_script_sees_the_committed_message_and_can_notify() {
    let fixture = Fixture::new(
        "watch",
        r#"

        fn apply(ctx) {
            on("agent/message", |cx, message| {
                notify("saw " + message.tool_calls.len() + " calls: " + message.text);
            });
        }
"#,
    );
    let sink = Recorder::default();

    let (backend, _script) = scripted([Turn::text("hello there")]);

    let composition = compose(host(&fixture, &sink)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(sink.lines(), vec!["watch: saw 0 calls: hello there"]);
}

#[tokio::test]
async fn a_failing_guard_blocks_but_a_failing_observer_does_not() {
    let fixture = Fixture::new(
        "broken",
        r#"

        fn apply(ctx) {
            on("agent/tool-call", |tool| {
                throw "I have no idea";
            });

            on("agent/turn-start", |cx| {
                throw "nor here";
            });
        }
"#,
    );
    let sink = Recorder::default();

    let (backend, _script) = scripted([
        Turn::call("call_1", "echo", r#"{"value":"x"}"#),
        Turn::text("done"),
    ]);

    let composition = compose(host(&fixture, &sink)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .tool(echo_tool())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    let outcome = agent.prompt("go").await;

    // on_turn_start failed open: the run still reached its second turn.
    assert_eq!(outcome.turns, 2);

    // on_tool_call failed closed: the tool did not run.
    let result = agent.transcript().get(2).expect("a tool result");
    assert!(
        text_of(result).contains("failed to decide"),
        "a guard that raised blocks the call: {}",
        text_of(result)
    );

    assert!(
        sink.lines()
            .iter()
            .any(|line| line.contains("I have no idea")),
        "the failure was reported: {:?}",
        sink.lines()
    );
}

#[tokio::test]
async fn a_script_reads_relative_to_the_workspace_and_beyond_it() {
    // A coding session grants `exec`, and `sh` reads anywhere, so the file
    // functions are unconfined too. What stays true is where a *relative* path
    // starts. `caps::resolve`'s own tests cover a confined host.
    let outside = std::env::temp_dir().join(format!("aphid-outside-{}.txt", unique()));
    std::fs::write(&outside, "beyond").expect("write the outside file");

    let fixture = Fixture::new(
        "reader",
        &format!(
            r#"
        fn apply(ctx) {{
            on("agent/run-start", |cx| {{
                notify(fs_read("note.txt"));
                notify(fs_read({:?}));
            }});
        }}
"#,
            outside.display().to_string()
        ),
    );
    std::fs::write(fixture.root().join("note.txt"), "inside").expect("write the file");
    let sink = Recorder::default();

    let (backend, _script) = scripted([Turn::text("done")]);

    let composition = compose(host(&fixture, &sink)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;
    let _ = std::fs::remove_file(&outside);

    assert_eq!(sink.lines(), vec!["reader: inside", "reader: beyond"]);
}

#[tokio::test]
async fn a_script_prompts_the_model_from_a_hook() {
    let fixture = Fixture::new(
        "nudge",
        r#"

        fn apply(ctx) {
            on("agent/run-end", |cx, outcome| {
                prompt("and now the other thing");
            });
        }
"#,
    );
    let sink = Recorder::default();

    let (backend, _script) = scripted([Turn::text("done")]);

    let composition = compose(host(&fixture, &sink)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(
        sink.lines(),
        vec!["nudge prompts: and now the other thing"],
        "a hook sends text to the model through the sink"
    );
}

#[tokio::test]
async fn a_tick_reaches_the_plugins_that_want_one() {
    let fixture = Fixture::new(
        "clock",
        r#"
        fn apply(ctx) {
            on("code/tick", || {
                let memory = state();
                memory.ticks = if memory.ticks == () { 1 } else { memory.ticks + 1 };
                save_state(memory);
                notify("tick " + memory.ticks);
            });
        }
        "#,
    );
    let sink = Recorder::default();
    let composition = compose(host(&fixture, &sink)).await;

    assert!(composition.bus.has_listeners::<aphid_code::events::Tick>());

    // Twice, because the guard that stops a slow tick from queueing behind
    // itself has to be cleared again when the tick finishes.
    composition.bus.emit(&mut aphid_code::events::Tick);
    composition.bus.emit(&mut aphid_code::events::Tick);

    assert_eq!(sink.lines(), vec!["clock: tick 1", "clock: tick 2"]);
}

#[tokio::test]
async fn a_plugin_without_a_tick_is_never_asked_for_one() {
    let fixture = Fixture::new(
        "quiet",
        r#"fn apply(ctx) { on("agent/run-start", |cx| { notify("hello"); }); }"#,
    );
    let sink = Recorder::default();
    let composition = compose(host(&fixture, &sink)).await;

    assert!(!composition.bus.has_listeners::<aphid_code::events::Tick>());
    composition.bus.emit(&mut aphid_code::events::Tick);

    assert!(sink.lines().is_empty(), "{:?}", sink.lines());
}

#[tokio::test]
async fn a_command_a_plugin_runs_is_recorded_under_the_plugin_name() {
    let fixture = Fixture::new(
        "runner",
        r#"

        fn apply(ctx) {
            on("agent/run-start", |cx| {
                let result = exec("echo from a plugin");
                notify(result.stdout);
            });
        }
"#,
    );
    let sink = Recorder::default();
    let processes = Arc::new(exec::Registry::new());

    let (backend, _script) = scripted([Turn::text("done")]);
    let composition = compose(host_recording(&fixture, &sink, &processes)).await;
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .compose(&composition)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;

    assert_eq!(sink.lines(), vec!["runner: from a plugin\n"]);

    // The same record the `bash` tool writes to, and the same one `/ps` reads.
    let recorded = processes.snapshot();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].origin, "runner");
    assert_eq!(recorded[0].command, "echo from a plugin");
    assert_eq!(recorded[0].status, exec::Status::Exited(0));
    assert!(recorded[0].pid.is_some());
}

/// Mount the loaded scripts on a fresh composition, for an agent to be built
/// into.
async fn compose(host: std::sync::Arc<aphid_code::scripting::PluginHost>) -> Composition {
    let composition = Composition::new();
    // Mounted first: a plugin that offers a command declares `commands` in its
    // `inject`, so without this it waits forever and contributes nothing.
    composition
        .add(
            aphid_code::registries::Registries::for_composition(&composition)
                as std::sync::Arc<dyn aphid_agent::rt::Component>,
            serde_json::Value::Null,
        )
        .await
        .expect("the registry has no dependencies");
    composition
        .add(
            std::sync::Arc::new(aphid_code::scripting::ScriptHost::new(host, &composition)),
            serde_json::Value::Null,
        )
        .await
        .expect("the script host has no dependencies and no schema");
    composition
}
