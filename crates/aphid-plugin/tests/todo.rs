//! The todo plugin, driven through a real agent run.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aphid_agent::Agent;
use aphid_agent::exec;
use aphid_agent::testing::{Turn, scripted};
use aphid_core::{ContentRef, MessageRef, providers::deepseek};
use aphid_plugin::{
    Action, Capabilities, PluginHost, Silent, Sink, SurfaceRender, Widget, explicit,
};

const PLUGIN: &str = include_str!("../../../.aphid/plugins/todo.rhai");

struct Fixture {
    root: PathBuf,
}

/// Collects prompts, so a test can assert on what a command sent to the model.
#[derive(Clone, Default)]
struct Recorder {
    prompts: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("lock").clone()
    }
}

impl Sink for Recorder {
    fn notify(&self, _plugin: &str, _text: &str) {}

    fn prompt(&self, plugin: &str, text: &str) {
        self.prompts
            .lock()
            .expect("lock")
            .push(format!("{plugin}: {text}"));
    }
}

impl Fixture {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let root = std::env::temp_dir().join(format!(
            "aphid-todo-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(".aphid").join("plugins")).expect("create");
        std::fs::write(
            root.join(".aphid").join("plugins").join("todo.rhai"),
            PLUGIN,
        )
        .expect("write the plugin");
        Self { root }
    }

    fn host(&self) -> Arc<PluginHost> {
        self.host_with(Arc::new(Silent))
    }

    fn host_with(&self, sink: Arc<dyn Sink>) -> Arc<PluginHost> {
        let file = explicit(&self.root.join(".aphid").join("plugins").join("todo.rhai"))
            .expect("readable");
        let (host, diagnostics) = PluginHost::load(
            &[file],
            &Capabilities::full(&self.root),
            sink,
            &Arc::new(exec::Registry::new()),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        Arc::new(host)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The text content of the nth transcript message.
fn text_of(message: MessageRef<'_>) -> String {
    message
        .content()
        .filter_map(|part| match part {
            ContentRef::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect()
}

/// Drive one tool call through the agent and return the result text.
async fn run_one_tool(host: Arc<PluginHost>, tool: &str, args: &str) -> String {
    let (backend, _script) = scripted([Turn::call("c1", tool, args), Turn::text("done")]);

    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .plugin_arc(host)
        .stream_fn(backend)
        .build();

    agent.prompt("go").await;
    text_of(agent.transcript().get(2).expect("a result"))
}

#[test]
fn the_plugin_loads_with_commands_tools_and_a_surface() {
    let fixture = Fixture::new();
    let host = fixture.host();

    assert_eq!(host.commands().len(), 2);
    assert_eq!(host.plugins()[0].tools().len(), 4);
    assert_eq!(host.surfaces().len(), 1);
    assert_eq!(host.surfaces()[0].name, "todo");
    assert!(!host.surfaces()[0].interactive);
}

#[test]
fn todo_status_toggles_the_open_flag() {
    let fixture = Fixture::new();
    let host = fixture.host();

    // The flag is the panel's own, not the plugin's: a surface keeps its model
    // beside the rest of what the plugin remembers.
    let open = |host: &PluginHost| {
        host.plugins()[0]
            .surface_state("todo")
            .get("open")
            .and_then(|value| value.as_bool().ok())
            .unwrap_or(false)
    };
    assert!(!open(&host), "the panel starts closed");

    let actions = host.run_command("todo-status", "on").expect("registered");
    assert!(matches!(actions.as_slice(), [Action::Notice(text)] if text.contains("on")));
    assert!(open(&host), "the flag was set to true");

    host.run_command("todo-status", "off").expect("registered");
    assert!(!open(&host), "the flag was set to false");
}

#[test]
fn todo_command_sends_the_prompt_in_todo_mode() {
    let fixture = Fixture::new();
    let recorder = Recorder::default();
    let host = fixture.host_with(Arc::new(recorder.clone()));

    let actions = host.run_command("todo", "write tests").expect("registered");
    assert_eq!(
        actions,
        vec![Action::Notice("todo mode: write tests".to_owned())]
    );

    let prompts = recorder.prompts();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(
        prompts[0].starts_with("todo: You are in TODO MODE."),
        "{prompts:?}"
    );
    assert!(prompts[0].ends_with("write tests"), "{prompts:?}");
}

#[tokio::test]
async fn adding_and_completing_a_task_renders_markdown() {
    let fixture = Fixture::new();
    let host = fixture.host();

    let added = run_one_tool(host.clone(), "todo_add", r#"{"task":"write tests"}"#).await;
    assert!(added.contains("Added task 1"), "{added}");
    assert!(added.contains("- [ ] 1. write tests"), "{added}");

    let done = run_one_tool(host.clone(), "todo_done", r#"{"id":1}"#).await;
    assert!(done.contains("Marked task 1 done"), "{done}");
    assert!(done.contains("- [x] 1. write tests"), "{done}");

    let listed = run_one_tool(host.clone(), "todo_list", "{}").await;
    assert!(listed.contains("## Pending"), "{listed}");
    assert!(listed.contains("_(none)_"), "{listed}");
    assert!(listed.contains("## Done"), "{listed}");
    assert!(listed.contains("- [x] 1. write tests"), "{listed}");
}

#[tokio::test]
async fn clearing_the_list_removes_pending_and_done_tasks() {
    let fixture = Fixture::new();
    let host = fixture.host();

    let _ = run_one_tool(host.clone(), "todo_add", r#"{"task":"write tests"}"#).await;
    let _ = run_one_tool(host.clone(), "todo_done", r#"{"id":1}"#).await;

    let cleared = run_one_tool(host.clone(), "todo_clear", "{}").await;
    assert!(cleared.contains("Cleared the todo list"), "{cleared}");
    assert!(cleared.contains("_(none)_"), "{cleared}");

    let listed = run_one_tool(host.clone(), "todo_list", "{}").await;
    assert!(listed.contains("## Pending"), "{listed}");
    assert!(listed.contains("## Done"), "{listed}");
    assert_eq!(listed.matches("_(none)_").count(), 2, "{listed}");
}

#[test]
fn the_surface_renders_the_list_after_it_opens() {
    let fixture = Fixture::new();
    let host = fixture.host();

    assert!(matches!(
        host.render_surface("todo", "todo"),
        Some(SurfaceRender::Closed)
    ));

    host.run_command("todo-status", "on").expect("registered");
    assert!(matches!(
        host.render_surface("todo", "todo"),
        Some(SurfaceRender::Widget(Widget::Text { .. }))
    ));
}

#[test]
fn no_state_file_is_written() {
    let fixture = Fixture::new();
    let host = fixture.host();

    host.run_command("todo-status", "on").expect("registered");
    host.flush();

    let state_file = fixture
        .root
        .join(".aphid")
        .join("plugins")
        .join("state")
        .join("todo.json");
    assert!(!state_file.exists(), "the todo list is session-only");
}
