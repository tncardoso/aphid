//! What the coding harness announces, and the state a plugin keeps.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aphid_agent::Sink;
use aphid_agent::exec;
use aphid_agent::rt::Composition;
use aphid_code::events::{
    Ask, Change, FileChange, Notice, Permission, Session, SessionEnd, SessionStart, SystemPrompt,
};
use aphid_code::plugins::Risk;
use aphid_code::scripting::{Capabilities, PluginHost, ScriptComponent, explicit};

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

    /// Load the plugins and mount them on a fresh composition.
    ///
    /// Two returns because a test wants both: the host for its state, and the
    /// composition to announce on.
    fn mount(&self, paths: &[PathBuf], sink: Arc<dyn Sink>) -> (Arc<PluginHost>, Composition) {
        let host = Arc::new(self.load(paths, sink));
        let composition = Composition::new();
        for plugin in host.plugins() {
            futures_lite_block(composition.add(
                Arc::new(ScriptComponent::new(Arc::clone(plugin), &composition)),
                serde_json::Value::Null,
            ))
            .expect("a plugin with no dependencies mounts");
        }
        (host, composition)
    }

    fn load(&self, paths: &[PathBuf], sink: Arc<dyn Sink>) -> PluginHost {
        let files: Vec<_> = paths
            .iter()
            .map(|path| explicit(path).expect("readable"))
            .collect();
        let (host, diagnostics) = PluginHost::load(
            &files,
            &Capabilities::full(&self.root),
            sink,
            &Arc::new(exec::Registry::new()),
        );
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

/// Drive a future to completion on the current thread.
///
/// Everything here is synchronous once the plugin has compiled — mounting a
/// component with no dependencies never actually waits — so this never blocks.
fn futures_lite_block<T>(future: impl std::future::Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    loop {
        if let std::task::Poll::Ready(value) = std::future::Future::poll(future.as_mut(), &mut cx) {
            return value;
        }
    }
}

/// Put a permission question to whatever is listening.
fn ask(composition: &Composition, tool: &str, risk: Risk) -> Option<Permission> {
    composition.bus.bail(&Ask {
        tool: tool.to_owned(),
        summary: format!("{tool} something"),
        risk,
    })
}

fn session(reason: &str) -> Session {
    Session {
        id: None,
        path: None,
        reason: reason.to_owned(),
        restored: 0,
    }
}

#[test]
fn a_script_appends_to_and_replaces_the_system_prompt() {
    let fixture = Fixture::new();
    let append = fixture.write(
        "append",
        r#"fn apply(ctx) { on("code/system-prompt", |text| #{ append: "Be terse." }); }"#,
    );
    let (_host, composition) = fixture.mount(&[append], Arc::new(Recorder::default()));

    let prompt = composition
        .bus
        .waterfall::<SystemPrompt>("You are an agent.".to_owned(), &|text| text);
    assert_eq!(prompt, "You are an agent.\n\nBe terse.");

    let fixture = Fixture::new();
    let replace = fixture.write(
        "replace",
        r#"fn apply(ctx) { on("code/system-prompt", |text| #{ replace: "Only this." }); }"#,
    );
    let (_host, composition) = fixture.mount(&[replace], Arc::new(Recorder::default()));

    let prompt = composition
        .bus
        .waterfall::<SystemPrompt>("You are an agent.".to_owned(), &|text| text);
    assert_eq!(prompt, "Only this.");
}

#[test]
fn a_script_decides_a_permission_and_otherwise_defers() {
    let fixture = Fixture::new();
    let policy = fixture.write(
        "policy",
        r#"
        fn apply(ctx) {
            on("code/permission", |request| {
                if request.risk == "destructive" { return "deny"; }
                if request.tool == "read" { return "allow"; }
                return "ask";
            });
        }
        "#,
    );
    let (_host, composition) = fixture.mount(&[policy], Arc::new(Recorder::default()));

    assert_eq!(
        ask(&composition, "bash", Risk::Destructive),
        Some(Permission::Deny)
    );
    assert_eq!(
        ask(&composition, "read", Risk::Read),
        Some(Permission::Allow)
    );
    assert_eq!(
        ask(&composition, "write", Risk::Mutate),
        None,
        "`ask` means the next decider gets it"
    );
}

#[test]
fn a_permission_hook_that_raises_denies() {
    let fixture = Fixture::new();
    let broken = fixture.write(
        "broken",
        r#"fn apply(ctx) { on("code/permission", |request| { throw "cannot decide"; }); }"#,
    );
    let sink = Recorder::default();
    let (_host, composition) = fixture.mount(&[broken], Arc::new(sink.clone()));

    assert_eq!(
        ask(&composition, "bash", Risk::Mutate),
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
        fn apply(ctx) {
            on("code/file-change", |change| {
                notify(change.kind + " " + change.path + ": " + change.before + " -> " + change.after);
            });
        }
        "#,
    );
    let sink = Recorder::default();
    let (_host, composition) = fixture.mount(&[watcher], Arc::new(sink.clone()));

    composition.bus.emit(&mut FileChange {
        path: Path::new("/w/a.txt").to_path_buf(),
        kind: Change::Edit,
        before: Some("old".to_owned()),
        after: "new".to_owned(),
    });

    assert_eq!(sink.lines(), vec!["edit /w/a.txt: old -> new"]);
}

#[test]
fn a_script_keeps_state_across_loads() {
    let fixture = Fixture::new();
    let counter = fixture.write(
        "counter",
        r#"
        fn apply(ctx) {
            on("code/session-start", |session| {
                let s = state();
                s.runs = if "runs" in s { s.runs + 1 } else { 1 };
                save_state(s);
                notify("run " + s.runs);
            });
        }
        "#,
    );

    let sink = Recorder::default();
    let (host, composition) = fixture.mount(std::slice::from_ref(&counter), Arc::new(sink.clone()));
    composition.bus.emit(&mut SessionStart(session("new")));
    composition.bus.emit(&mut SessionEnd(session("end")));
    host.flush();

    assert_eq!(sink.lines(), vec!["run 1"]);
    assert!(
        fixture
            .state("counter")
            .is_some_and(|text| text.contains("runs")),
        "the state was written back"
    );

    // A fresh host, as a later session would be.
    let sink = Recorder::default();
    let (_host, composition) = fixture.mount(&[counter], Arc::new(sink.clone()));
    composition.bus.emit(&mut SessionStart(session("new")));

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
        fn apply(ctx) {
            on("code/session-start", |session| {
                let c = config();
                notify(c.greeting + " x" + c.times);
            });
        }
        "#,
    );

    let sink = Recorder::default();
    let (_host, composition) = fixture.mount(&[greeter], Arc::new(sink.clone()));
    composition.bus.emit(&mut SessionStart(session("new")));

    assert_eq!(sink.lines(), vec!["hello x2"]);
}

#[test]
fn a_notice_listener_cannot_call_itself_back() {
    let fixture = Fixture::new();
    // Notifying from within a notice listener would recur forever without the
    // event's own reentrancy policy.
    let echo = fixture.write(
        "echo",
        r#"fn apply(ctx) { on("code/notice", |text| { notify("saw: " + text); }); }"#,
    );

    let sink = Recorder::default();
    let (_host, composition) = fixture.mount(&[echo], Arc::new(sink.clone()));
    composition
        .bus
        .emit(&mut Notice("something happened".to_owned()));

    assert_eq!(sink.lines(), vec!["saw: something happened"]);
}

#[test]
fn state_is_not_written_when_nothing_changed() {
    let fixture = Fixture::new();
    let quiet = fixture.write(
        "quiet",
        r#"fn apply(ctx) { on("code/session-start", |session| { let s = state(); }); }"#,
    );

    let (host, composition) = fixture.mount(&[quiet], Arc::new(Recorder::default()));
    composition.bus.emit(&mut SessionStart(session("new")));
    composition.bus.emit(&mut SessionEnd(session("end")));
    host.flush();

    assert!(
        fixture.state("quiet").is_none(),
        "a read-only plugin touches the disk not at all"
    );
}
