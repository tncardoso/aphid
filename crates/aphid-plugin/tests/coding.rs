//! The hooks a coding harness dispatches, and the state a plugin keeps.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aphid_plugin::{Capabilities, Change, Permission, PluginHost, SessionInfo, Sink, explicit};

/// A scratch tree holding plugins, config and state, removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let root = std::env::temp_dir().join(format!(
            "aphid-coding-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(".aphid").join("plugins")).expect("create");
        Self { root }
    }

    fn plugins(&self) -> PathBuf {
        self.root.join(".aphid").join("plugins")
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.plugins().join(format!("{name}.rhai"));
        std::fs::write(&path, source).expect("write the plugin");
        path
    }

    fn config(&self, name: &str, json: &str) {
        std::fs::write(self.plugins().join(format!("{name}.json")), json).expect("write config");
    }

    fn state(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.plugins().join("state").join(format!("{name}.json"))).ok()
    }

    fn load(&self, paths: &[PathBuf], sink: Arc<dyn Sink>) -> PluginHost {
        let files: Vec<_> = paths
            .iter()
            .map(|path| explicit(path).expect("readable"))
            .collect();
        let (host, diagnostics) = PluginHost::load(&files, &Capabilities::full(&self.root), sink);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        host
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

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
    fn notify(&self, _plugin: &str, text: &str) {
        self.lines.lock().expect("lock").push(text.to_owned());
    }

    fn log(&self, plugin: &str, text: &str) {
        self.notify(plugin, text);
    }
}

#[test]
fn a_script_appends_to_and_replaces_the_system_prompt() {
    let fixture = Fixture::new();
    let append = fixture.write(
        "append",
        r#"fn on_system_prompt(text) { return #{ append: "Be terse." }; }"#,
    );
    let host = fixture.load(&[append], Arc::new(Recorder::default()));

    let mut prompt = "You are an agent.".to_owned();
    host.system_prompt(&mut prompt);
    assert_eq!(prompt, "You are an agent.\n\nBe terse.");

    let fixture = Fixture::new();
    let replace = fixture.write(
        "replace",
        r#"fn on_system_prompt(text) { return #{ replace: "Only this." }; }"#,
    );
    let host = fixture.load(&[replace], Arc::new(Recorder::default()));

    let mut prompt = "You are an agent.".to_owned();
    host.system_prompt(&mut prompt);
    assert_eq!(prompt, "Only this.");
}

#[test]
fn a_script_decides_a_permission_and_otherwise_defers() {
    let fixture = Fixture::new();
    let policy = fixture.write(
        "policy",
        r#"
        fn on_permission(request) {
            if request.risk == "destructive" { return "deny"; }
            if request.tool == "read" { return "allow"; }
            return "ask";
        }
        "#,
    );
    let host = fixture.load(&[policy], Arc::new(Recorder::default()));

    assert_eq!(
        host.permission("bash", "rm -rf /", "destructive"),
        Some(Permission::Deny)
    );
    assert_eq!(
        host.permission("read", "read a file", "read"),
        Some(Permission::Allow)
    );
    assert_eq!(
        host.permission("write", "write a file", "mutate"),
        None,
        "`ask` means the next decider gets it"
    );
}

#[test]
fn a_permission_hook_that_raises_denies() {
    let fixture = Fixture::new();
    let broken = fixture.write(
        "broken",
        r#"fn on_permission(request) { throw "cannot decide"; }"#,
    );
    let sink = Recorder::default();
    let host = fixture.load(&[broken], Arc::new(sink.clone()));

    assert_eq!(
        host.permission("bash", "anything", "mutate"),
        Some(Permission::Deny),
        "a guard that failed has approved nothing"
    );
    assert!(
        sink.lines()
            .iter()
            .any(|line| line.contains("cannot decide"))
    );
}

#[test]
fn a_script_sees_a_file_change_with_the_text_before_and_after() {
    let fixture = Fixture::new();
    let watcher = fixture.write(
        "watcher",
        r#"
        fn on_file_change(change) {
            notify(change.kind + " " + change.path + ": " + change.before + " -> " + change.after);
        }
        "#,
    );
    let sink = Recorder::default();
    let host = fixture.load(&[watcher], Arc::new(sink.clone()));

    host.file_change(Path::new("/w/a.txt"), Change::Edit, Some("old"), "new");

    assert_eq!(sink.lines(), vec!["edit /w/a.txt: old -> new"]);
}

#[test]
fn a_script_keeps_state_across_loads() {
    let fixture = Fixture::new();
    let counter = fixture.write(
        "counter",
        r#"
        fn on_session_start(session) {
            let s = state();
            s.runs = if "runs" in s { s.runs + 1 } else { 1 };
            save_state(s);
            notify("run " + s.runs);
        }
        "#,
    );

    let sink = Recorder::default();
    let host = fixture.load(std::slice::from_ref(&counter), Arc::new(sink.clone()));
    let info = SessionInfo {
        id: None,
        path: None,
        reason: "new",
        restored: 0,
    };
    host.session_start(&info);
    host.session_end(&info);

    assert_eq!(sink.lines(), vec!["run 1"]);
    assert!(
        fixture
            .state("counter")
            .is_some_and(|text| text.contains("runs")),
        "the state was written back"
    );

    // A fresh host, as a later session would be.
    let sink = Recorder::default();
    let host = fixture.load(&[counter], Arc::new(sink.clone()));
    host.session_start(&info);

    assert_eq!(
        sink.lines(),
        vec!["run 2"],
        "it picked up where it left off"
    );
}

#[test]
fn a_plugin_reads_its_own_settings() {
    let fixture = Fixture::new();
    fixture.config("greeter", r#"{ "greeting": "hello", "times": 2 }"#);
    let greeter = fixture.write(
        "greeter",
        r#"
        fn on_session_start(session) {
            let c = config();
            notify(c.greeting + " x" + c.times);
        }
        "#,
    );

    let sink = Recorder::default();
    let host = fixture.load(&[greeter], Arc::new(sink.clone()));
    host.session_start(&SessionInfo {
        id: None,
        path: None,
        reason: "new",
        restored: 0,
    });

    assert_eq!(sink.lines(), vec!["hello x2"]);
}

#[test]
fn a_notice_hook_cannot_call_itself_back() {
    let fixture = Fixture::new();
    // Notifying from within on_notify would recur forever without the guard.
    let echo = fixture.write("echo", r#"fn on_notify(text) { notify("saw: " + text); }"#);

    let sink = Recorder::default();
    let host = fixture.load(&[echo], Arc::new(sink.clone()));
    host.notice("something happened");

    assert_eq!(sink.lines(), vec!["saw: something happened"]);
}

#[test]
fn state_is_not_written_when_nothing_changed() {
    let fixture = Fixture::new();
    let quiet = fixture.write(
        "quiet",
        r#"fn on_session_start(session) { let s = state(); }"#,
    );

    let host = fixture.load(&[quiet], Arc::new(Recorder::default()));
    let info = SessionInfo {
        id: None,
        path: None,
        reason: "new",
        restored: 0,
    };
    host.session_start(&info);
    host.session_end(&info);

    assert!(
        fixture.state("quiet").is_none(),
        "a read-only plugin touches the disk not at all"
    );
}
