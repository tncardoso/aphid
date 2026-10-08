//! Plugins that call a model, against a scripted backend.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aphid_agent::Sink;
use aphid_agent::exec;
use aphid_agent::testing::{Script, Turn, scripted};
use aphid_code::Catalog;
use aphid_code::scripting::{Capabilities, ModelAccess, PluginHost, explicit};
use aphid_core::catalog::ModelEntry;

/// Collects what a script says, so a test can wait for a reply.
#[derive(Clone, Default)]
struct Recorder {
    lines: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("lock").clone()
    }

    /// Wait until `count` lines arrived, or fail after a few seconds.
    fn wait_for(&self, count: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let lines = self.lines();
            if lines.len() >= count {
                return lines;
            }
            assert!(Instant::now() < deadline, "only got {lines:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Sink for Recorder {
    fn notify(&self, _plugin: &str, text: &str) {
        self.lines.lock().expect("lock").push(text.to_owned());
    }
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(source: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aphid-models-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(".aphid").join("plugins")).expect("create");
        std::fs::write(root.join(".aphid").join("plugins").join("ask.rhai"), source)
            .expect("write the plugin");
        Self { root }
    }

    /// Load the plugin with a model it may call, answered by `turns`.
    fn load(&self, sink: &Recorder, turns: Vec<Turn>) -> (common::Loaded, Arc<Script>) {
        let (backend, script) = scripted(turns);
        let entry: ModelEntry = serde_json::from_value(serde_json::json!({
            "id": "cheap-model",
            "base_url": "http://localhost:8080/v1",
            "context_window": 32768,
            "max_tokens": 4096,
        }))
        .expect("a valid entry");
        let mut caps = Capabilities::full(&self.root);
        caps.models = Some(Arc::new(ModelAccess::new(
            Catalog::from_parts(&[entry]),
            backend,
            Arc::new(|_| Some("test-key".into())),
        )));
        (self.load_with(sink, &caps), script)
    }

    fn load_with(&self, sink: &Recorder, caps: &Capabilities) -> common::Loaded {
        let file =
            explicit(&self.root.join(".aphid").join("plugins").join("ask.rhai")).expect("readable");
        let (host, problems) = PluginHost::load(
            &[file],
            caps,
            Arc::new(sink.clone()),
            &Arc::new(exec::Registry::new()),
        );
        assert!(problems.is_empty(), "{problems:?}");
        common::Loaded::new(Arc::new(host))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const ASKER: &str = r#"const inject = ["commands"];

fn apply(ctx) {
    let mem = #{ replies: 0 };
    command(#{
        name: "ask",
        description: "Ask the cheap model.",
        run: |args| {
            let messages = [#{ role: "user", text: args }];
            if args == "twice" {
                messages = [
                    #{ role: "user", text: "first" },
                    #{ role: "assistant", text: "too long" },
                    #{ role: "user", text: "shorter" },
                ];
            }
            let id = model_ask(#{ model: "cheap", system: "Be short.", messages: messages }, |reply| {
                mem.replies += 1;
                notify(reply.id + " " + reply.ok + " " + reply.text + " " + mem.replies);
            });
            notice("asked " + id + ", busy " + (model_busy() <= 1))
        }
    });
    command(#{
        name: "models",
        description: "List the models.",
        run: |args| notice("" + model_list().map(|m| m.id))
    });
}
"#;

#[test]
fn a_reply_comes_back_to_the_closure_that_asked() {
    let fixture = Fixture::new(ASKER);
    let sink = Recorder::default();
    let (loaded, script) = fixture.load(&sink, vec![Turn::text("hello back")]);

    let actions = loaded.run_command("ask", "hello").expect("the command ran");
    assert_eq!(format!("{actions:?}"), r#"[Notice("asked 1, busy true")]"#);

    assert_eq!(sink.wait_for(1), vec!["1 true hello back 1"]);
    let body = &script.requests()[0];
    assert!(body.contains("Be short."), "{body}");
    assert!(body.contains("test-key") || !body.contains("Authorization"));
}

#[test]
fn a_conversation_carries_the_assistant_turns() {
    let fixture = Fixture::new(ASKER);
    let sink = Recorder::default();
    let (loaded, script) = fixture.load(&sink, vec![Turn::text("ok")]);

    loaded.run_command("ask", "twice");
    sink.wait_for(1);

    let body: serde_json::Value = serde_json::from_str(&script.requests()[0]).expect("a JSON body");
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| message["role"].as_str().unwrap_or_default())
        .collect();
    // The system prompt goes out under whichever name the model's API uses.
    assert_eq!(roles[1..], ["user", "assistant", "user"]);
    assert_eq!(body["messages"][2]["content"], "too long");
}

#[test]
fn many_asks_run_together_and_each_reply_arrives_once() {
    let fixture = Fixture::new(ASKER);
    let sink = Recorder::default();
    let (loaded, _script) = fixture.load(&sink, (0..8).map(|_| Turn::text("x")).collect());

    for _ in 0..8 {
        loaded.run_command("ask", "go");
    }

    let mut counts: Vec<String> = sink
        .wait_for(8)
        .iter()
        .map(|line| line.rsplit(' ').next().unwrap_or_default().to_owned())
        .collect();
    counts.sort_by_key(|count| count.parse::<u32>().unwrap_or_default());
    // Each reply saw the count the one before it left: none was lost to a race.
    assert_eq!(counts, (1..=8).map(|n| n.to_string()).collect::<Vec<_>>());
}

#[test]
fn a_failed_request_says_so_in_the_reply() {
    let fixture = Fixture::new(ASKER);
    let sink = Recorder::default();
    let (loaded, _script) = fixture.load(&sink, vec![Turn::failed("HTTP 500")]);

    loaded.run_command("ask", "hello");

    assert_eq!(sink.wait_for(1), vec!["1 false  1"]);
}

#[test]
fn an_unknown_model_is_an_error_at_once() {
    let fixture = Fixture::new(
        r#"const inject = ["commands"];
fn apply(ctx) {
    command(#{ name: "ask", description: "", run: |args| {
        model_ask(#{ model: "nobody", messages: [#{ role: "user", text: "hi" }] }, |reply| {});
    }});
}
"#,
    );
    let sink = Recorder::default();
    let (loaded, script) = fixture.load(&sink, vec![]);

    loaded.run_command("ask", "");

    let lines = sink.lines();
    assert!(
        lines.iter().any(|line| line.contains("model `nobody`")),
        "{lines:?}"
    );
    assert_eq!(script.request_count(), 0);
}

#[test]
fn without_model_access_the_function_says_why() {
    let fixture = Fixture::new(ASKER);
    let sink = Recorder::default();
    let loaded = fixture.load_with(&sink, &Capabilities::full(&fixture.root));

    loaded.run_command("ask", "hello");
    assert_eq!(
        loaded.run_command("models", ""),
        Some(vec![aphid_code::scripting::Action::Notice("[]".to_owned())])
    );

    let lines = sink.lines();
    assert!(
        lines.iter().any(|line| line.contains("not available")),
        "{lines:?}"
    );
}

#[test]
fn the_models_are_listed() {
    let fixture = Fixture::new(ASKER);
    let sink = Recorder::default();
    let (loaded, _script) = fixture.load(&sink, vec![]);

    assert_eq!(
        loaded.run_command("models", ""),
        Some(vec![aphid_code::scripting::Action::Notice(
            r#"["cheap-model"]"#.to_owned()
        )])
    );
}
