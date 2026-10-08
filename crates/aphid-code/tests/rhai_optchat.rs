//! The OptChat plugin, driven through real runs with a scripted agent and a
//! compactor that answers from what it is asked.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aphid_agent::rt::Composition;
use aphid_agent::testing::{Responder, Script, Turn, responding, scripted};
use aphid_agent::{Agent, ToolCx, ToolOutcome, exec, tool_fn};
use aphid_code::Catalog;
use aphid_code::events::{Session, SessionStart};
use aphid_code::registries::Registries;
use aphid_code::scripting::{
    Action, Capabilities, ModelAccess, Parts, PluginHost, ScriptHost, explicit,
};
use aphid_core::catalog::ModelEntry;
use aphid_core::{Model, StopReason};
use serde_json::Value;

const PLUGIN: &str = include_str!("../examples/plugins/optchat.rhai");

fn entry(id: &str) -> ModelEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "base_url": "http://localhost:8080/v1",
        "context_window": 32768,
        "max_tokens": 4096,
    }))
    .expect("a valid entry")
}

fn session_model() -> Model {
    Model::try_from(&entry("agent-model")).expect("a valid model")
}

/// A compactor that writes a short line for each request: the first words of
/// what it was asked to compress or merge. `long_first` makes the first try of
/// every node too long, so the retry path runs.
fn compactor(long_first: bool) -> (aphid_agent::StreamFn, Arc<Responder>) {
    responding(move |body| {
        let body: Value = serde_json::from_str(body).expect("a JSON body");
        let messages = body["messages"].as_array().expect("messages");
        let content = |message: &Value| message["content"].as_str().unwrap_or_default().to_owned();
        let last = content(messages.last().expect("a message"));
        if long_first && !last.contains("← LIMIT") {
            return Turn::text("x".repeat(500));
        }
        // The step is the first user message; a retry adds more after it.
        let step = messages
            .iter()
            .find(|message| message["role"] == "user")
            .map(content)
            .unwrap_or_default();
        let task = step.rsplit(" bytes:\n").next().unwrap_or_default();
        let words: String = task
            .split_whitespace()
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
        Turn::text(format!("sum[{words}]"))
    })
}

/// Prints what the plugin says, so a failing test shows why.
#[derive(Default)]
struct Said;

impl aphid_agent::Sink for Said {
    fn notify(&self, plugin: &str, text: &str) {
        eprintln!("{plugin}: {text}");
    }
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(settings: &Value) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aphid-optchat-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let plugins = root.join(".aphid").join("plugins");
        std::fs::create_dir_all(&plugins).expect("create");
        std::fs::write(plugins.join("optchat.rhai"), PLUGIN).expect("write the plugin");
        let mut settings = settings.clone();
        settings["dir"] = Value::String(root.join("chat").display().to_string());
        std::fs::write(plugins.join("optchat.json"), settings.to_string()).expect("settings");
        Self { root }
    }

    fn chat(&self) -> PathBuf {
        self.root.join("chat")
    }

    /// Every line of one of the two logs, oldest first.
    fn lines(&self, kind: &str) -> Vec<Value> {
        let dir = self.chat().join(kind);
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        names.sort();
        names
            .iter()
            .flat_map(|path| {
                std::fs::read_to_string(path)
                    .expect("readable")
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| serde_json::from_str(line).expect("JSON"))
                    .collect::<Vec<Value>>()
            })
            .collect()
    }

    async fn open(&self, backend: aphid_agent::StreamFn) -> Chat {
        let file = explicit(&self.root.join(".aphid/plugins/optchat.rhai")).expect("readable");
        let mut caps = Capabilities::full(&self.root);
        caps.models = Some(Arc::new(ModelAccess::new(
            Catalog::from_parts(&[entry("cheap-model"), entry("agent-model")]),
            backend,
            Arc::new(|_| Some("key".into())),
        )));
        let (host, problems) = PluginHost::load(
            &[file],
            &caps,
            Arc::new(Said),
            &Arc::new(exec::Registry::new()),
        );
        assert!(problems.is_empty(), "{problems:?}");
        let host = Arc::new(host);

        let composition = Composition::new();
        let registries = Registries::for_composition(&composition);
        composition
            .add(
                Arc::clone(&registries) as Arc<dyn aphid_agent::rt::Component>,
                Value::Null,
            )
            .await
            .expect("the registries mount");
        composition
            .add(
                Arc::new(ScriptHost::new(Arc::clone(&host), &composition)),
                Value::Null,
            )
            .await
            .expect("the script host mounts");
        Chat {
            host,
            registries,
            composition,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Chat {
    host: Arc<PluginHost>,
    registries: Arc<Registries>,
    composition: Composition,
}

impl Chat {
    fn command(&self, args: &str) -> Vec<Action> {
        self.host
            .run_command(self.registries.commands(), "optchat", args)
            .expect("the command is offered")
    }

    fn notice(&self, args: &str) -> String {
        self.command(args)
            .into_iter()
            .find_map(|action| match action {
                Action::Notice(text) => Some(text),
                Action::NewSession => None,
            })
            .unwrap_or_default()
    }

    fn session(&self, reason: &str) {
        self.composition.bus.emit(&mut SessionStart(Session {
            id: Some("s".to_owned()),
            path: None,
            reason: reason.to_owned(),
            restored: 0,
        }));
    }

    /// `/optchat on`, and the new session it asks for.
    fn turn_on(&self) {
        let actions = self.command("on");
        assert!(actions.contains(&Action::NewSession), "{actions:?}");
        self.session("new");
    }

    fn agent(&self, backend: aphid_agent::StreamFn) -> Agent {
        let agent = Agent::builder()
            .model(session_model())
            .system("base prompt")
            .tool(tool_fn(
                "shout",
                "Say something long.",
                serde_json::json!({ "type": "object", "properties": {} }),
                |_: Value, _cx: ToolCx| async move { ToolOutcome::text("y".repeat(300)) },
            ))
            .compose(&self.composition)
            .stream_fn(backend)
            .build();
        // What the harness hands its plugins when it builds the agent.
        self.host.prompt_parts().set(Parts {
            system: "base prompt\n\nCurrent working directory: /work".to_owned(),
            tools: Some(Arc::clone(agent.tools())),
            ..Parts::default()
        });
        agent
    }

    /// Wait until the compactor has built every node of the view.
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(self.notice("status").contains(" 0 running")
            && self.notice("status").contains("all summarized"))
        {
            assert!(Instant::now() < deadline, "{}", self.notice("status"));
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn settings() -> Value {
    serde_json::json!({
        "compactor_model": "cheap-model",
        "compactor_thinking": "off",
        "node": 60,
        "view": 2000,
        "jobs": 2,
        "tries": 3,
        "retry_ms": 20,
        "cap": 100,
        "browser": "true",
    })
}

/// The messages one encoded request carried, as `(role, content)`.
fn messages(body: &str) -> Vec<(String, String)> {
    let body: Value = serde_json::from_str(body).expect("a JSON body");
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| {
            (
                message["role"].as_str().unwrap_or_default().to_owned(),
                message["content"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn script(turns: Vec<Turn>) -> (aphid_agent::StreamFn, Arc<Script>) {
    scripted(turns)
}

const LONG_ONE: &str = "please rewrite the config parser as a single pass over the bytes";
const LONG_TWO: &str = "now add tests for the unicode keys and the windows line endings";

#[tokio::test(flavor = "multi_thread")]
async fn every_prompt_starts_fresh_with_the_view_of_the_whole_chat() {
    let fixture = Fixture::new(&settings());
    let (compactor, _asked) = compactor(false);
    let chat = fixture.open(compactor).await;
    chat.turn_on();

    let (backend, agent_script) = script(vec![
        Turn::text("I will write a state machine with six states for the parser"),
        Turn::text("Added two tests"),
    ]);
    let mut agent = chat.agent(backend);

    agent.prompt(LONG_ONE).await;
    chat.settle();
    agent.prompt(LONG_TWO).await;

    let second = messages(&agent_script.requests()[1]);
    assert_eq!(second.len(), 2, "system and prompt only: {second:?}");
    let (_, system) = &second[0];
    assert!(system.starts_with("You are aphid-optchat"), "{system}");
    assert!(system.contains("Available tools:\n"), "{system}");
    assert!(
        system.contains("\n- shout: Say something long."),
        "{system}"
    );
    assert!(
        system
            .trim_end()
            .ends_with("date(id) gives the date and time of message id.")
    );
    let (role, prompt) = &second[1];
    assert_eq!(role, "user");
    assert!(
        prompt.starts_with(
            "<chat>\n0+1|sum[user: please rewrite the]\n1+1|sum[talk: I will write]\n</chat>"
        ),
        "{prompt}"
    );
    assert!(prompt.ends_with(&format!("\n\n{LONG_TWO}")), "{prompt}");
    assert!(!prompt.contains("not summarized yet"));

    let log: Vec<(String, String)> = fixture
        .lines("main")
        .iter()
        .map(|line| {
            (
                line["kind"].as_str().unwrap_or_default().to_owned(),
                line["text"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert_eq!(log[0], ("user".to_owned(), LONG_ONE.to_owned()));
    assert_eq!(log[1].0, "talk");
    assert_eq!(log[2], ("user".to_owned(), LONG_TWO.to_owned()));
    let first = &fixture.lines("main")[0];
    assert_eq!(first["i"], 0);
    assert_eq!(first["size"], ("user: ".len() + LONG_ONE.len()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_message_is_its_own_line_and_two_short_lines_merge_for_free() {
    let fixture = Fixture::new(&settings());
    let (compactor, asked) = compactor(false);
    let chat = fixture.open(compactor).await;
    chat.turn_on();

    let (backend, _) = script(vec![Turn::text("ok")]);
    let mut agent = chat.agent(backend);
    agent.prompt("hi").await;
    chat.settle();

    let tree = fixture.lines("tree");
    let texts: Vec<(i64, i64, &str)> = tree
        .iter()
        .map(|node| {
            (
                node["l"].as_i64().unwrap_or(-1),
                node["i"].as_i64().unwrap_or(-1),
                node["text"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert!(texts.contains(&(0, 0, "user: hi")), "{texts:?}");
    assert!(texts.contains(&(0, 1, "talk: ok")), "{texts:?}");
    assert!(texts.contains(&(1, 0, "user: hi\ntalk: ok")), "{texts:?}");
    assert!(asked.requests().is_empty(), "no model was needed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_line_over_the_limit_is_asked_for_again_in_the_same_conversation() {
    let fixture = Fixture::new(&settings());
    let (compactor, asked) = compactor(true);
    let chat = fixture.open(compactor).await;
    chat.turn_on();

    let (backend, _) = script(vec![Turn::text("ok")]);
    let mut agent = chat.agent(backend);
    agent.prompt(LONG_ONE).await;
    chat.settle();

    let retry = messages(&asked.requests()[1]);
    assert_eq!(retry[1].0, "user");
    assert!(retry[1].1.starts_with("<chat>\n</chat>"), "{}", retry[1].1);
    assert_eq!(retry[2], ("assistant".to_owned(), "x".repeat(500)));
    assert!(
        retry[3]
            .1
            .starts_with("That line is 500 bytes; the limit is 60.")
    );
    assert!(
        retry[3]
            .1
            .ends_with(&format!("{}| ← LIMIT", "x".repeat(60)))
    );

    let tree = fixture.lines("tree");
    assert_eq!(tree[0]["text"], "sum[user: please rewrite the]");
}

#[tokio::test(flavor = "multi_thread")]
async fn zoom_opens_a_line_and_date_tells_when() {
    let fixture = Fixture::new(&settings());
    let (compactor, _) = compactor(false);
    let chat = fixture.open(compactor).await;
    chat.turn_on();

    let (backend, agent_script) = script(vec![
        Turn::text("ok"),
        Turn::call("c1", "zoom", r#"{"id":0,"n":1}"#),
        Turn::call("c2", "zoom", r#"{"id":0,"n":2}"#),
        Turn::call("c3", "date", r#"{"id":0}"#),
        Turn::call("c4", "zoom", r#"{"id":1,"n":2}"#),
        Turn::text("done"),
    ]);
    let mut agent = chat.agent(backend);
    agent.prompt("hi").await;
    chat.settle();
    agent.prompt("look back").await;

    let results: Vec<String> = agent_script
        .requests()
        .iter()
        .skip(2)
        .map(|body| messages(body).last().cloned().unwrap_or_default().1)
        .collect();
    assert_eq!(results[0], "0+1|user: hi");
    assert_eq!(results[1], "0+1|user: hi\n1+1|talk: ok");
    assert!(results[2].starts_with("20"), "an ISO date: {}", results[2]);
    assert_eq!(results[3], "No line 1+2.");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_tool_result_is_cut_to_its_head_and_tail() {
    let fixture = Fixture::new(&settings());
    let (compactor, _) = compactor(false);
    let chat = fixture.open(compactor).await;
    chat.turn_on();

    let (backend, agent_script) =
        script(vec![Turn::call("c1", "shout", "{}"), Turn::text("heard")]);
    let mut agent = chat.agent(backend);
    agent.prompt("shout").await;

    let sent = messages(&agent_script.requests()[1])
        .last()
        .cloned()
        .unwrap_or_default()
        .1;
    assert_eq!(
        sent,
        format!(
            "{}\n[… 200 characters cut …]\n{}",
            "y".repeat(50),
            "y".repeat(50)
        )
    );
    let echo = fixture
        .lines("main")
        .into_iter()
        .find(|line| line["kind"] == "echo")
        .expect("the result was logged");
    assert_eq!(echo["text"], sent);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_waits_for_the_compactor_and_a_cancel_sends_nothing() {
    let fixture = Fixture::new(&settings());
    // A compactor that never succeeds: the view can never settle.
    let (compactor, _) = responding(|_| Turn::failed("HTTP 500"));
    let chat = fixture.open(compactor).await;
    chat.turn_on();

    let (backend, agent_script) =
        script(vec![Turn::text("an answer long enough to need the model")]);
    let mut agent = chat.agent(backend);
    // The first prompt sees an empty view, so it goes. What it left in the log
    // then needs the model, which never answers.
    agent.prompt(LONG_ONE).await;
    let handle = agent.handle();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        handle.cancel();
    });
    let outcome = agent.prompt(LONG_TWO).await;
    canceller.join().expect("cancelled");

    assert_eq!(outcome.stop, StopReason::Aborted);
    assert_eq!(agent_script.request_count(), 1);
    let log = fixture.lines("main");
    assert_eq!(log.last().expect("a line")["text"], LONG_TWO);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_chat_comes_back_after_a_restart() {
    let fixture = Fixture::new(&settings());
    let status = {
        let (compactor, _) = compactor(false);
        let chat = fixture.open(compactor).await;
        chat.turn_on();
        let (backend, _) = script(vec![Turn::text("first answer"), Turn::text("second")]);
        let mut agent = chat.agent(backend);
        agent.prompt(LONG_ONE).await;
        chat.settle();
        agent.prompt(LONG_TWO).await;
        chat.settle();
        let status = chat.notice("status");
        chat.command("off");
        status
    };

    // Once from the snapshot, once from message 0.
    for snapshot in [true, false] {
        if !snapshot {
            let _ = std::fs::remove_file(fixture.chat().join("view.json"));
        }
        let (compactor, _) = compactor(false);
        let chat = fixture.open(compactor).await;
        chat.turn_on();
        let again = chat.notice("status");
        assert_eq!(
            again.split(" Compactor").next(),
            status.replace("is off", "is on").split(" Compactor").next(),
            "snapshot: {snapshot}"
        );
        chat.command("off");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_line_at_the_end_of_the_log_is_skipped_and_ended() {
    let fixture = Fixture::new(&settings());
    let main = fixture.chat().join("main");
    std::fs::create_dir_all(&main).expect("create");
    std::fs::write(
        main.join("2026-01-01.jsonl"),
        "{\"i\":0,\"kind\":\"user\",\"text\":\"hi\",\"size\":8,\"date\":\"2026-01-01T00:00:00+00:00\"}\n{\"i\":1,\"ki",
    )
    .expect("write");

    let (compactor, _) = compactor(false);
    let chat = fixture.open(compactor).await;
    chat.turn_on();

    assert!(
        chat.notice("status").contains(" 1 messages"),
        "{}",
        chat.notice("status")
    );
    let text = std::fs::read_to_string(main.join("2026-01-01.jsonl")).expect("read");
    assert!(text.ends_with('\n'));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_one_process_writes_the_chat() {
    let fixture = Fixture::new(&settings());
    let (first_compactor, _) = compactor(false);
    let first = fixture.open(first_compactor).await;
    first.turn_on();

    let (second_compactor, _) = compactor(false);
    let second = fixture.open(second_compactor).await;
    assert!(second.notice("on").starts_with("Another aphid is using"));

    first.command("off");
    assert!(second.notice("on").starts_with("OptChat is on"));
}

#[tokio::test(flavor = "multi_thread")]
async fn another_session_turns_the_mode_off() {
    let fixture = Fixture::new(&settings());
    let (compactor, _) = compactor(false);
    let chat = fixture.open(compactor).await;
    chat.turn_on();
    assert!(chat.notice("status").starts_with("OptChat is on"));

    chat.session("switch");
    assert!(chat.notice("status").starts_with("OptChat is off"));

    // And with it off, a request is the ordinary one, without its tools.
    let (backend, agent_script) = script(vec![Turn::text("plain")]);
    let mut agent = chat.agent(backend);
    agent.prompt("hello").await;
    let body = &agent_script.requests()[0];
    assert!(body.contains("base prompt"));
    assert!(!body.contains("\"zoom\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn browse_writes_the_memory_as_a_page() {
    let fixture = Fixture::new(&settings());
    let (compactor, _) = compactor(false);
    let chat = fixture.open(compactor).await;
    chat.turn_on();
    let (backend, _) = script(vec![Turn::text("ok")]);
    let mut agent = chat.agent(backend);
    agent.prompt("hi <there>").await;
    chat.settle();

    assert!(chat.notice("browse").starts_with("Opened"));
    let page = std::fs::read_to_string(fixture.chat().join("browse.html")).expect("the page");
    assert!(page.contains("hi &lt;there&gt;"));
    assert!(page.contains("<h2>Level 1</h2>"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_view_stays_near_its_budget_and_changes_only_at_its_end() {
    let mut small = settings();
    small["view"] = serde_json::json!(400);
    let fixture = Fixture::new(&small);
    let (compactor, _) = compactor(false);
    let chat = fixture.open(compactor).await;
    chat.turn_on();

    let mut turns = Vec::new();
    for n in 0..24 {
        turns.push(Turn::text(format!(
            "answer number {n} with a few more words to it"
        )));
    }
    let (backend, agent_script) = script(turns);
    let mut agent = chat.agent(backend);
    for n in 0..24 {
        agent
            .prompt(&format!(
                "question number {n} that is long enough to need a model"
            ))
            .await;
        chat.settle();
    }

    let views: Vec<String> = agent_script
        .requests()
        .iter()
        .map(|body| {
            let prompt = messages(body)[1].1.clone();
            prompt
                .split("</chat>")
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    let last = views.last().expect("a view");
    let lines: Vec<&str> = last.lines().skip(1).collect();
    let bytes: usize = lines
        .iter()
        .map(|line| line.split_once('|').map_or(0, |(_, text)| text.len()))
        .sum();
    assert!(bytes <= 400 + 60, "the view is {bytes} bytes: {last}");
    assert_eq!(lines.first().map(|line| line.starts_with("0+")), Some(true));

    // Each view shares most of its start with the one before it.
    let shared: Vec<usize> = views
        .windows(2)
        .map(|pair| {
            pair[0]
                .lines()
                .zip(pair[1].lines())
                .take_while(|(a, b)| a == b)
                .count()
        })
        .collect();
    // A view this small merges near its start too, so only the oldest line is
    // sure to stay; on a real budget most of the view does.
    assert!(shared.iter().skip(8).all(|&n| n >= 1), "{shared:?}");
}
