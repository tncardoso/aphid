//! The coding tools, against a real temp directory. No network, no API key.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aphid_agent::exec;
use aphid_agent::{ProgressSink, ToolCall, ToolCx, ToolHandler, ToolOutcome};
use aphid_code::Workspace;
use aphid_code::tools::{bash, edit, read, truncate, write};

/// A workspace in a fresh temp directory, removed when the guard drops.
struct Temp {
    root: PathBuf,
    workspace: Workspace,
    /// What the tools started, for the tests that ask.
    processes: Arc<exec::Registry>,
}

impl Temp {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aphid-tools-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("temp dir");
        let root = root.canonicalize().expect("canonical temp dir");
        Self {
            workspace: Workspace::new(&root),
            root,
            processes: Arc::new(exec::Registry::new()),
        }
    }

    fn bash(&self) -> impl ToolHandler {
        bash::tool(&self.workspace, &self.processes)
    }

    fn workspace(&self) -> Workspace {
        self.workspace.clone()
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(&path, contents).expect("write fixture");
        path
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).expect("read back")
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Run a tool the way the agent loop would.
async fn call(tool: &impl ToolHandler, arguments: &str) -> ToolOutcome {
    call_with(tool, arguments, &ToolCx::default()).await
}

async fn call_with(tool: &impl ToolHandler, arguments: &str, cx: &ToolCx) -> ToolOutcome {
    tool.execute(
        ToolCall {
            id: "call_1",
            name: "test",
            arguments,
        },
        cx,
    )
    .await
}

// ---------------------------------------------------------------- bash

#[tokio::test]
async fn bash_runs_in_the_workspace_and_returns_output() {
    let temp = Temp::new();
    temp.write("marker.txt", "hi");
    let tool = temp.bash();

    let outcome = call(&tool, r#"{"command":"ls"}"#).await;

    assert!(!outcome.is_error, "{}", outcome.text_content());
    assert!(outcome.text_content().contains("marker.txt"));
}

#[tokio::test]
async fn bash_merges_stderr_and_reports_a_non_zero_exit_without_failing() {
    let temp = Temp::new();
    let tool = temp.bash();

    let outcome = call(&tool, r#"{"command":"echo out; echo err >&2; exit 3"}"#).await;

    let text = outcome.text_content();
    assert!(text.contains("out"), "{text}");
    assert!(text.contains("err"), "{text}");
    assert!(text.contains("[exit code 3]"), "{text}");
    // A command reporting through its status is not a tool failure.
    assert!(!outcome.is_error);
}

#[tokio::test]
async fn bash_streams_each_line_as_progress() {
    struct Collect(Arc<Mutex<Vec<String>>>);

    impl ProgressSink for Collect {
        fn progress(&self, _call_id: &str, _tool: &str, chunk: &str) {
            self.0.lock().expect("lock").push(chunk.to_owned());
        }
    }

    let temp = Temp::new();
    let tool = temp.bash();
    let chunks = Arc::new(Mutex::new(Vec::new()));
    let cx = ToolCx::default().with_sink(Arc::new(Collect(Arc::clone(&chunks))));

    let outcome = call_with(
        &tool,
        r#"{"command":"echo one; echo two; echo three"}"#,
        &cx,
    )
    .await;

    assert!(!outcome.is_error);
    assert_eq!(
        chunks.lock().expect("lock").clone(),
        vec!["one".to_owned(), "two".to_owned(), "three".to_owned()]
    );
}

#[tokio::test]
async fn bash_times_out_and_kills_the_child() {
    let temp = Temp::new();
    let tool = temp.bash();

    let started = std::time::Instant::now();
    let outcome = call(&tool, r#"{"command":"sleep 30","timeout":0.3}"#).await;

    assert!(outcome.is_error);
    assert!(outcome.text_content().contains("timed out"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the sleep should have been killed, not waited out"
    );
}

#[tokio::test]
async fn bash_stops_when_the_run_is_cancelled() {
    let temp = Temp::new();
    let tool = temp.bash();

    let handle = aphid_agent::AgentHandle::default();
    let cx = ToolCx::for_handle(&handle);
    let canceller = handle.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        canceller.cancel();
    });

    let started = std::time::Instant::now();
    let outcome = call_with(&tool, r#"{"command":"sleep 30"}"#, &cx).await;

    assert!(outcome.is_error);
    assert!(outcome.text_content().contains("cancelled"));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[tokio::test]
async fn bash_truncates_and_spills_long_output() {
    let temp = Temp::new();
    let tool = temp.bash();

    let outcome = call(
        &tool,
        &format!(r#"{{"command":"seq 1 {}"}}"#, truncate::MAX_LINES + 500),
    )
    .await;

    let text = outcome.text_content();
    assert!(text.contains("lines shown"), "{text}");
    let details = outcome.details.expect("details");
    assert_eq!(details["truncated"], true);

    let spill = details["full_output_path"].as_str().expect("spill path");
    let full = std::fs::read_to_string(spill).expect("spilled output");
    assert!(full.starts_with("1\n"));
    // The tail is what survives: the end of a command's output is the useful part.
    assert!(text.contains(&(truncate::MAX_LINES + 500).to_string()));
    let _ = std::fs::remove_file(spill);
}

#[tokio::test]
async fn bash_records_what_it_ran() {
    let temp = Temp::new();
    let tool = temp.bash();

    let outcome = call(&tool, r#"{"command":"echo recorded"}"#).await;
    assert!(!outcome.is_error, "{}", outcome.text_content());

    let recorded = temp.processes.snapshot();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].origin, "bash");
    assert_eq!(recorded[0].command, "echo recorded");
    assert_eq!(recorded[0].status, exec::Status::Exited(0));
    assert!(recorded[0].pid.is_some());
    assert_eq!(recorded[0].bytes, "recorded\n".len() as u64);
}

#[tokio::test]
async fn bash_reports_a_command_stopped_from_the_process_list() {
    let temp = Temp::new();
    let tool = temp.bash();

    let processes = Arc::clone(&temp.processes);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let id = processes.snapshot().first().expect("the sleep").id;
        processes.kill(id);
    });

    let outcome = call(&tool, r#"{"command":"sleep 30"}"#).await;

    assert!(outcome.is_error);
    assert!(
        outcome.text_content().contains("[killed]"),
        "{}",
        outcome.text_content()
    );
}

// ---------------------------------------------------------------- read

#[tokio::test]
async fn read_numbers_lines_and_honours_offset_and_limit() {
    let temp = Temp::new();
    temp.write("src/lib.rs", "alpha\nbravo\ncharlie\ndelta\n");
    let tool = read::tool(&temp.workspace());

    let all = call(&tool, r#"{"path":"src/lib.rs"}"#).await;
    assert!(!all.is_error, "{}", all.text_content());
    assert_eq!(
        all.text_content(),
        "1\talpha\n2\tbravo\n3\tcharlie\n4\tdelta\n"
    );

    let slice = call(&tool, r#"{"path":"src/lib.rs","offset":2,"limit":2}"#).await;
    assert_eq!(slice.text_content(), "2\tbravo\n3\tcharlie\n");
    let details = slice.details.expect("details");
    assert_eq!(details["total_lines"], 4);
    assert_eq!(details["from_line"], 2);
    assert_eq!(details["to_line"], 3);
}

#[tokio::test]
async fn read_refuses_binary_and_missing_files() {
    let temp = Temp::new();
    std::fs::write(temp.root.join("blob.bin"), [0x00, 0x01, 0x02, 0x00]).expect("binary fixture");
    let tool = read::tool(&temp.workspace());

    let binary = call(&tool, r#"{"path":"blob.bin"}"#).await;
    assert!(binary.is_error);
    assert!(binary.text_content().contains("binary"));

    let missing = call(&tool, r#"{"path":"nope.rs"}"#).await;
    assert!(missing.is_error);
    assert!(missing.text_content().contains("could not read"));
}

#[tokio::test]
async fn read_rejects_an_offset_past_the_end() {
    let temp = Temp::new();
    temp.write("short.txt", "one\ntwo\n");
    let tool = read::tool(&temp.workspace());

    let outcome = call(&tool, r#"{"path":"short.txt","offset":99}"#).await;

    assert!(outcome.is_error);
    assert!(outcome.text_content().contains("past the end"));
}

#[tokio::test]
async fn read_refuses_to_leave_the_workspace() {
    let temp = Temp::new();
    let tool = read::tool(&temp.workspace());

    let outcome = call(&tool, r#"{"path":"../../etc/passwd"}"#).await;

    assert!(outcome.is_error);
    assert!(outcome.text_content().contains("outside the workspace"));
}

// ---------------------------------------------------------------- write

#[tokio::test]
async fn write_creates_parents_and_reports_what_it_did() {
    let temp = Temp::new();
    let tool = write::tool(&temp.workspace(), None);

    let created = call(&tool, r#"{"path":"a/b/c.txt","content":"hello\n"}"#).await;
    assert!(!created.is_error, "{}", created.text_content());
    assert!(created.text_content().starts_with("Created a/b/c.txt"));
    assert_eq!(temp.read("a/b/c.txt"), "hello\n");
    assert_eq!(created.details.expect("details")["created"], true);

    let overwritten = call(&tool, r#"{"path":"a/b/c.txt","content":"bye\n"}"#).await;
    assert!(
        overwritten
            .text_content()
            .starts_with("Overwrote a/b/c.txt")
    );
    assert_eq!(temp.read("a/b/c.txt"), "bye\n");
}

// ---------------------------------------------------------------- edit

#[tokio::test]
async fn edit_applies_a_unique_replacement() {
    let temp = Temp::new();
    temp.write("src/main.rs", "fn main() {\n    let n = 0;\n}\n");
    let tool = edit::tool(&temp.workspace(), None);

    let outcome = call(
        &tool,
        r#"{"path":"src/main.rs","edits":[{"old_text":"let n = 0;","new_text":"let n = 42;"}]}"#,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text_content());
    assert_eq!(outcome.text_content(), "Applied 1 edit to src/main.rs");
    assert_eq!(
        temp.read("src/main.rs"),
        "fn main() {\n    let n = 42;\n}\n"
    );

    // Details carry what a renderer needs to draw the diff.
    let details = outcome.details.expect("details");
    assert_eq!(details["edits"][0]["line"], 2);
    assert_eq!(details["edits"][0]["old"], "let n = 0;");
    assert_eq!(details["edits"][0]["new"], "let n = 42;");
}

#[tokio::test]
async fn edit_applies_several_in_order() {
    let temp = Temp::new();
    temp.write("f.txt", "one\ntwo\nthree\n");
    let tool = edit::tool(&temp.workspace(), None);

    let outcome = call(
        &tool,
        r#"{"path":"f.txt","edits":[
            {"old_text":"one","new_text":"1"},
            {"old_text":"three","new_text":"3"}
        ]}"#,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text_content());
    assert_eq!(outcome.text_content(), "Applied 2 edits to f.txt");
    assert_eq!(temp.read("f.txt"), "1\ntwo\n3\n");
}

#[tokio::test]
async fn edit_refuses_an_ambiguous_match_and_changes_nothing() {
    let temp = Temp::new();
    temp.write("f.txt", "x = 1\ny = 2\nx = 1\n");
    let tool = edit::tool(&temp.workspace(), None);

    let outcome = call(
        &tool,
        r#"{"path":"f.txt","edits":[{"old_text":"x = 1","new_text":"x = 9"}]}"#,
    )
    .await;

    assert!(outcome.is_error);
    assert!(outcome.text_content().contains("appears 2 times"));
    assert_eq!(
        temp.read("f.txt"),
        "x = 1\ny = 2\nx = 1\n",
        "file untouched"
    );
}

#[tokio::test]
async fn edit_refuses_a_snippet_that_is_not_there() {
    let temp = Temp::new();
    temp.write("f.txt", "hello\n");
    let tool = edit::tool(&temp.workspace(), None);

    let outcome = call(
        &tool,
        r#"{"path":"f.txt","edits":[{"old_text":"goodbye","new_text":"hi"}]}"#,
    )
    .await;

    assert!(outcome.is_error);
    assert!(outcome.text_content().contains("does not appear"));
    assert_eq!(temp.read("f.txt"), "hello\n");
}

#[tokio::test]
async fn a_later_failing_edit_leaves_the_file_alone() {
    let temp = Temp::new();
    temp.write("f.txt", "one\ntwo\n");
    let tool = edit::tool(&temp.workspace(), None);

    let outcome = call(
        &tool,
        r#"{"path":"f.txt","edits":[
            {"old_text":"one","new_text":"1"},
            {"old_text":"missing","new_text":"x"}
        ]}"#,
    )
    .await;

    assert!(outcome.is_error);
    // The first edit succeeded against the working copy, but nothing was written.
    assert_eq!(temp.read("f.txt"), "one\ntwo\n");
}

#[tokio::test]
async fn edit_rejects_degenerate_replacements() {
    let temp = Temp::new();
    temp.write("f.txt", "hello\n");
    let tool = edit::tool(&temp.workspace(), None);

    let identical = call(
        &tool,
        r#"{"path":"f.txt","edits":[{"old_text":"hello","new_text":"hello"}]}"#,
    )
    .await;
    assert!(identical.is_error);
    assert!(identical.text_content().contains("identical"));

    let empty = call(
        &tool,
        r#"{"path":"f.txt","edits":[{"old_text":"","new_text":"x"}]}"#,
    )
    .await;
    assert!(empty.is_error);
    assert!(empty.text_content().contains("empty"));

    let none = call(&tool, r#"{"path":"f.txt","edits":[]}"#).await;
    assert!(none.is_error);
    assert!(none.text_content().contains("no edits"));
}

// ---------------------------------------------------------------- schemas

#[test]
fn every_tool_is_registered_once_with_a_schema() {
    let temp = Temp::new();
    let tools = aphid_code::tools::all(&temp.workspace(), None, &Arc::new(exec::Registry::new()));

    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool.declaration().name.as_str())
        .collect();
    assert_eq!(names, vec!["bash", "read", "write", "edit"]);

    for tool in &tools {
        let declaration = tool.declaration();
        assert_eq!(declaration.parameters["type"], "object");
        assert!(
            !declaration.description.is_empty(),
            "{} has no description",
            declaration.name
        );
    }
}

#[test]
fn workspace_discovery_finds_the_git_root() {
    // This crate lives in a git repository, so discovery must land on its root.
    let workspace = Workspace::discover();
    assert!(
        workspace.root().join(".git").exists(),
        "expected a git root, got {}",
        workspace.root().display()
    );
    assert!(Path::new(workspace.root()).join("Cargo.toml").exists());
}
