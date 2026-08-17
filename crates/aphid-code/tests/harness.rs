//! The whole harness, end to end: real tools against a real temp workspace,
//! driven by a scripted provider.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use aphid_agent::testing::{Turn, scripted};
use aphid_code::harness::{self, HarnessOptions};
use aphid_code::{Catalog, Workspace};
use aphid_core::catalog::ModelEntry;
use aphid_core::{Model, Role};

/// A minimal config entry, the shape a user would actually type.
fn model_entry(id: &str) -> ModelEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "base_url": "http://localhost:8080/v1",
        "context_window": 32768,
        "max_tokens": 4096,
    }))
    .expect("a valid entry")
}

fn test_model(id: &str) -> Model {
    Model::try_from(&model_entry(id)).expect("a valid model")
}

struct Temp {
    root: PathBuf,
}

impl Temp {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aphid-harness-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("temp dir");
        Self {
            root: root.canonicalize().expect("canonical"),
        }
    }

    fn write(&self, name: &str, contents: &str) {
        let path = self.root.join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, contents).expect("write");
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).expect("read back")
    }

    fn options(&self) -> HarnessOptions {
        let mut options = HarnessOptions::new(Workspace::new(&self.root), test_model("test-model"));
        options.cwd = self.root.clone();
        // Discovery must see this workspace and nothing else. With the real home
        // directory the result changes with whatever the machine holds.
        options.home = None;
        options
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn the_agent_edits_a_file_through_its_tools() {
    let temp = Temp::new();
    temp.write("src/lib.rs", "pub fn answer() -> u32 {\n    0\n}\n");

    let (backend, script) = scripted([
        Turn::call("c1", "read", r#"{"path":"src/lib.rs"}"#),
        Turn::call(
            "c2",
            "edit",
            r#"{"path":"src/lib.rs","edits":[{"old_text":"    0","new_text":"    42"}]}"#,
        ),
        Turn::text("Changed the answer to 42."),
    ]);

    let mut options = temp.options();
    options.stream_fn = Some(backend);
    let mut harness = harness::build(options);

    let outcome = harness.agent.prompt("make answer return 42").await;

    assert_eq!(outcome.turns, 3);
    assert!(!outcome.is_failure(), "{outcome:?}");
    assert_eq!(
        temp.read("src/lib.rs"),
        "pub fn answer() -> u32 {\n    42\n}\n"
    );

    // The tool results were carried back to the provider.
    let requests = script.requests();
    assert!(
        requests[1].contains("pub fn answer"),
        "read result was sent"
    );
    assert!(
        requests[2].contains("Applied 1 edit"),
        "edit result was sent"
    );
}

#[tokio::test]
async fn a_failed_tool_call_is_reported_to_the_model_rather_than_ending_the_run() {
    let temp = Temp::new();
    temp.write("f.txt", "one\ntwo\n");

    let (backend, _script) = scripted([
        // Ambiguous edit: `o` appears in both lines.
        Turn::call(
            "c1",
            "edit",
            r#"{"path":"f.txt","edits":[{"old_text":"o","new_text":"0"}]}"#,
        ),
        Turn::call(
            "c2",
            "edit",
            r#"{"path":"f.txt","edits":[{"old_text":"one","new_text":"1"}]}"#,
        ),
        Turn::text("Fixed."),
    ]);

    let mut options = temp.options();
    options.stream_fn = Some(backend);
    let mut harness = harness::build(options);

    let outcome = harness.agent.prompt("edit it").await;

    assert_eq!(outcome.turns, 3, "the model got to correct itself");
    assert_eq!(temp.read("f.txt"), "1\ntwo\n");

    let errored = harness
        .agent
        .transcript()
        .iter()
        .filter_map(|message| message.tool_result())
        .filter(|meta| meta.is_error)
        .count();
    assert_eq!(errored, 1);
}

#[tokio::test]
async fn the_system_prompt_carries_the_project_context_and_skills() {
    let temp = Temp::new();
    temp.write("AGENTS.md", "Always run cargo fmt.");
    temp.write(
        ".aphid/skills/release/SKILL.md",
        "---\ndescription: How to cut a release\n---\nbody\n",
    );

    let (backend, script) = scripted([Turn::text("ok")]);
    let mut options = temp.options();
    options.stream_fn = Some(backend);
    let mut harness = harness::build(options);

    assert_eq!(harness.context_files.len(), 1);
    assert_eq!(harness.skills.len(), 1);

    harness.agent.prompt("hi").await;

    let request = &script.requests()[0];
    assert!(request.contains("Always run cargo fmt."));
    assert!(request.contains("How to cut a release"));
    // The skill body is advertised, not inlined.
    assert!(!request.contains("body"));
    // Every tool was offered.
    for name in ["bash", "read", "write", "edit"] {
        assert!(request.contains(name), "missing tool {name}");
    }

    let system = harness.agent.transcript().get(0).expect("system message");
    assert_eq!(system.role(), Role::System);
}

#[tokio::test]
async fn the_home_directory_is_the_one_the_caller_names() {
    let temp = Temp::new();
    temp.write(
        "project/.aphid/skills/release/SKILL.md",
        "---\ndescription: How to cut a release\n---\nbody\n",
    );
    temp.write(
        "home/.aphid/AGENTS.md",
        "Global rules the caller asked for.",
    );
    temp.write(
        "home/.agents/skills/grilling.md",
        "---\ndescription: How to grill\n---\nbody\n",
    );

    let (backend, _script) = scripted([Turn::text("ok")]);
    let mut options = HarnessOptions::new(
        Workspace::new(temp.root.join("project")),
        test_model("test-model"),
    );
    options.cwd = temp.root.join("project");
    options.home = Some(temp.root.join("home"));
    options.stream_fn = Some(backend);
    let harness = harness::build(options);

    // The named home was read, and only it: nothing came from the machine the
    // test runs on.
    let names: Vec<&str> = harness
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    assert_eq!(names, vec!["grilling", "release"]);
    let contents: Vec<&str> = harness
        .context_files
        .iter()
        .map(|file| file.content.as_str())
        .collect();
    assert_eq!(contents, vec!["Global rules the caller asked for."]);
}

#[tokio::test]
async fn context_loading_can_be_turned_off() {
    let temp = Temp::new();
    temp.write("AGENTS.md", "Always run cargo fmt.");

    let (backend, script) = scripted([Turn::text("ok")]);
    let mut options = temp.options();
    options.stream_fn = Some(backend);
    options.load_context = false;
    let mut harness = harness::build(options);

    harness.agent.prompt("hi").await;

    assert!(harness.context_files.is_empty());
    assert!(!script.requests()[0].contains("cargo fmt"));
}

#[tokio::test]
async fn switching_model_mid_session_keeps_the_conversation() {
    let temp = Temp::new();
    let (backend, script) = scripted([Turn::text("first"), Turn::text("second")]);

    let catalog = Catalog::from_parts(&[
        model_entry("deepseek-v4-flash"),
        model_entry("deepseek-v4-pro"),
    ]);
    let mut options = temp.options();
    options.stream_fn = Some(backend);
    options.model = catalog.resolve("flash").expect("flash");
    let mut harness = harness::build(options);

    harness.agent.prompt("one").await;
    let before = harness.agent.transcript().len();

    harness
        .agent
        .set_model(catalog.resolve("pro").expect("pro"));
    harness.agent.prompt("two").await;

    // Nothing was reset by the switch.
    assert_eq!(harness.agent.transcript().len(), before + 2);
    assert!(script.requests()[0].contains("deepseek-v4-flash"));
    assert!(script.requests()[1].contains("deepseek-v4-pro"));
}
