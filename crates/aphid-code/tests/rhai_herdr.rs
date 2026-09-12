//! The herdr reporter, against a herdr that only records what it is told.
//!
//! Every test points the plugin at a script with the `bin` and `pane` settings,
//! so no test depends on the environment it runs in. That matters here: a test
//! run started from inside a herdr pane inherits `HERDR_ENV`, and the reporter
//! would then report the test into a real sidebar.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aphid_agent::exec;
use aphid_agent::rt::Composition;
use aphid_agent::testing::{Turn, scripted};
use aphid_agent::{Agent, Silent, ToolCx, ToolOutcome, tool_fn};
use aphid_code::events::{Ask, Permission, Session, SessionEnd, SessionStart, Tick};
use aphid_code::plugins::scripts::AskFirst;
use aphid_code::plugins::{Confirmer, Decision, PermissionGate, Permissions, Risk};
use aphid_code::scripting::{Capabilities, PluginHost, ScriptComponent, explicit};
use aphid_core::providers::deepseek;

/// The plugin under test, read from the example that ships with aphid.
const PLUGIN: &str = include_str!("../examples/plugins/herdr.rhai");

/// A scratch workspace holding the plugin, its settings and a fake herdr.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let root = std::env::temp_dir().join(format!(
            "aphid-herdr-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(".aphid").join("plugins")).expect("create");
        let fixture = Self { root };
        std::fs::write(fixture.plugins().join("herdr.rhai"), PLUGIN).expect("write the plugin");
        fixture
    }

    fn plugins(&self) -> PathBuf {
        self.root.join(".aphid").join("plugins")
    }

    /// Where the fake herdr writes the commands it was given.
    fn calls(&self) -> PathBuf {
        self.root.join("calls")
    }

    /// Points the plugin at `body` as its herdr binary.
    fn herdr(&self, body: &str) {
        let path = self.root.join("fake-herdr");
        let script = format!(
            "#!/bin/bash\nprintf '%s;' \"$@\" >> {}\nprintf '\\n' >> {}\n{body}\n",
            shell(self.calls()),
            shell(self.calls())
        );
        std::fs::write(&path, script).expect("write the fake herdr");
        make_executable(&path);
    }

    /// The path of the fake herdr, for the settings.
    fn bin(&self) -> PathBuf {
        self.root.join("fake-herdr")
    }

    /// The settings the plugin reads, as JSON.
    fn config(&self, json: &str) {
        std::fs::write(self.plugins().join("herdr.json"), json).expect("write settings");
    }

    /// Every command the fake herdr was given, in order. Arguments are
    /// separated by a semicolon, so a message that holds a space stays one
    /// argument all the way to the assertion.
    fn recorded(&self) -> Vec<String> {
        std::fs::read_to_string(self.calls())
            .unwrap_or_default()
            .lines()
            .filter(|line| *line != "--version;")
            .map(str::to_owned)
            .collect()
    }

    async fn mount(&self) -> (Arc<PluginHost>, Composition) {
        let files = vec![explicit(&self.plugins().join("herdr.rhai")).expect("readable")];
        let (host, diagnostics) = PluginHost::load(
            &files,
            &Capabilities::full(&self.root),
            Arc::new(Silent),
            &Arc::new(exec::Registry::new()),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let host = Arc::new(host);

        let composition = Composition::new();
        for plugin in host.plugins() {
            composition
                .add(
                    Arc::new(ScriptComponent::new(Arc::clone(plugin), &composition)),
                    serde_json::Value::Null,
                )
                .await
                .expect("a plugin with no dependencies mounts");
        }
        (host, composition)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A path as a JSON string.
fn json(value: &Path) -> String {
    serde_json::to_string(&value.display().to_string()).expect("a path is a string")
}

/// A path as one shell word, for the script that writes it.
fn shell(path: PathBuf) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// A file a shell can run.
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).expect("stat").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod");
}

/// A herdr that answers, records, and does as it is told.
const HERDR: &str = "if [ \"$1\" = \"--version\" ]; then echo 'herdr test'; fi\nexit 0";

fn session(reason: &str) -> Session {
    Session {
        id: None,
        path: None,
        reason: reason.to_owned(),
        restored: 0,
    }
}

fn ask(composition: &Composition, tool: &str, summary: &str, risk: Risk) -> Option<Permission> {
    composition.bus.bail(&Ask {
        tool: tool.to_owned(),
        summary: summary.to_owned(),
        risk,
    })
}

/// Allows whatever it is asked, so the run continues past the question.
struct Allow;

impl Confirmer for Allow {
    fn confirm(&self, _tool: &str, _summary: &str, _risk: Risk) -> Decision {
        Decision::Allow
    }
}

/// Writes a file, as far as the permission gate is concerned.
fn write_tool() -> impl aphid_agent::ToolHandler {
    #[derive(serde::Deserialize)]
    struct Args {
        #[allow(dead_code)]
        path: String,
    }

    tool_fn(
        "write",
        "Write a file.",
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
        |_args: Args, _cx: ToolCx| async move { ToolOutcome::text("written") },
    )
}

/// The whole life of a session: it opens, a run works, a question blocks it, the
/// answer lets it carry on, the run ends and the session closes.
#[tokio::test]
async fn a_session_is_reported_from_the_first_prompt_to_the_last_report() {
    let fixture = Fixture::new();
    fixture.herdr(HERDR);
    fixture.config(&format!(
        "{{\"bin\": {}, \"pane\": \"w1:p1\", \"heartbeat\": 0}}",
        json(&fixture.bin())
    ));

    let (_host, composition) = fixture.mount().await;
    // The same wiring the terminal interface uses: the gate asks, the plugins
    // answer first, and the user is asked only about what is left.
    let permissions = Arc::new(Permissions::new(AskFirst::wrap(
        &composition.bus,
        Arc::new(Allow),
    )));
    composition
        .add(
            Arc::new(PermissionGate::new(None, permissions, &composition)),
            serde_json::Value::Null,
        )
        .await
        .expect("the gate has no dependencies");

    composition.bus.emit(&mut SessionStart(session("new")));

    let (backend, _script) = scripted([
        Turn::call("call_1", "write", r#"{"path":"notes.txt"}"#),
        Turn::text("done"),
    ]);
    let mut agent = Agent::builder()
        .model(deepseek::flash())
        .tool(write_tool())
        .compose(&composition)
        .stream_fn(backend)
        .build();
    agent.prompt("go").await;

    composition.bus.emit(&mut SessionEnd(session("end")));

    let expected = [
        "pane;report-agent;w1:p1;--source;custom:aphid;--agent;aphid;--state;idle;",
        "pane;report-agent;w1:p1;--source;custom:aphid;--agent;aphid;--state;working;",
        "pane;report-agent;w1:p1;--source;custom:aphid;--agent;aphid;--state;blocked;--message;write notes.txt;",
        "pane;report-agent;w1:p1;--source;custom:aphid;--agent;aphid;--state;working;--message;write;",
        "pane;report-agent;w1:p1;--source;custom:aphid;--agent;aphid;--state;idle;--message;done, 2 turns;",
        "notification;show;aphid;--body;done, 2 turns;--sound;done;",
        "pane;release-agent;w1:p1;--source;custom:aphid;--agent;aphid;",
    ];
    assert_eq!(fixture.recorded(), expected);
}

/// A herdr that answers `--version` and then refuses everything else.
///
/// This is the case that matters: `code/permission` is a bail whose failure
/// **denies the tool**, so a reporter that let its own failure out would veto
/// permissions on a machine where herdr is not running.
#[tokio::test]
async fn a_refused_report_leaves_the_decision_to_the_user() {
    let fixture = Fixture::new();
    fixture.herdr(
        "if [ \"$1\" = \"--version\" ]; then echo 'herdr test'; exit 0; fi\necho '{\"error\":\"pane_not_found\"}' >&2\nexit 1",
    );
    fixture.config(&format!(
        "{{\"bin\": {}, \"pane\": \"w1:p1\"}}",
        json(&fixture.bin())
    ));

    let (_host, composition) = fixture.mount().await;

    assert_eq!(
        ask(&composition, "write", "write notes.txt", Risk::Mutate),
        None,
        "the plugin has no opinion, so the user is asked"
    );
    assert!(
        fixture
            .recorded()
            .iter()
            .any(|line| line.contains("--state;blocked;")),
        "the report was attempted: {:?}",
        fixture.recorded()
    );
}

/// A herdr that is there for `--version` and gone for everything after it.
///
/// `exec` raises when the command cannot be started at all, which is the other
/// way a listener can fail, and the one a `try` has to cover.
#[tokio::test]
async fn a_herdr_that_vanishes_does_not_deny_the_tool() {
    let fixture = Fixture::new();
    fixture
        .herdr("rm -f \"$0\"\nif [ \"$1\" = \"--version\" ]; then echo 'herdr test'; fi\nexit 0");
    fixture.config(&format!(
        "{{\"bin\": {}, \"pane\": \"w1:p1\"}}",
        json(&fixture.bin())
    ));

    let (_host, composition) = fixture.mount().await;

    assert_eq!(
        ask(&composition, "write", "write notes.txt", Risk::Mutate),
        None
    );
}

/// A plugin that is switched off reports nothing at all.
#[tokio::test]
async fn the_plugin_can_be_switched_off() {
    let fixture = Fixture::new();
    fixture.herdr(HERDR);
    fixture.config(&format!(
        "{{\"enabled\": false, \"bin\": {}, \"pane\": \"w1:p1\"}}",
        json(&fixture.bin())
    ));

    let (_host, composition) = fixture.mount().await;
    composition.bus.emit(&mut SessionStart(session("new")));
    composition.bus.emit(&mut SessionEnd(session("end")));

    assert!(fixture.recorded().is_empty(), "{:?}", fixture.recorded());
}

/// The heartbeat repeats what herdr was last told, for a herdr that restarted
/// and forgot it. The terminal interface is the only place ticks happen.
#[tokio::test]
async fn the_heartbeat_repeats_the_last_report() {
    let fixture = Fixture::new();
    fixture.herdr(HERDR);
    fixture.config(&format!(
        "{{\"bin\": {}, \"pane\": \"w1:p1\", \"heartbeat\": 1, \"notify\": false}}",
        json(&fixture.bin())
    ));

    let (_host, composition) = fixture.mount().await;
    composition.bus.emit(&mut SessionStart(session("new")));

    // A second is four ticks, at a quarter of a second each.
    for _ in 0..3 {
        composition.bus.emit(&mut Tick);
    }
    assert_eq!(
        fixture.recorded().len(),
        1,
        "a second is not up yet: {:?}",
        fixture.recorded()
    );

    composition.bus.emit(&mut Tick);
    let recorded = fixture.recorded();
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert_eq!(recorded[0], recorded[1], "the same state, said again");
}
