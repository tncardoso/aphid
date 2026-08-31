//! The whole thing, end to end: a daemon, terminals, sessions and a scheduled
//! job.
//!
//! The provider is replaced with [`aphid_agent::testing::scripted`], so a run is
//! exact and offline, and everything else — the socket, the memory on disk, the
//! transcripts — is real.

mod common;

use std::path::Path;
use std::time::Duration;

use aphid_agent::testing::{Turn, scripted};
use aphid_alate::config::{Config, Permissions};
use aphid_alate::cron::Crontab;
use aphid_alate::daemon::{self, Options};
use aphid_alate::gateway::wire::{Envelope, Frame, Request};
use aphid_alate::gateway::{Client, is_listening};
use aphid_alate::home::Home;
use aphid_alate::memory::Memory;
use aphid_core::Model;
use aphid_core::catalog::ModelEntry;
use common::Temp;

/// Wait for a daemon to come up, rather than for a fixed time.
async fn attach(socket: &Path) -> Client {
    for _ in 0..200 {
        if is_listening(socket)
            && let Ok(client) = Client::connect(socket).await
        {
            return client;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the daemon never started listening on {}", socket.display());
}

/// Envelopes until one matches, so a test says what it is waiting for and not
/// how many others it expects to pass first.
async fn until(client: &mut Client, mut matches: impl FnMut(&Envelope) -> bool) -> Envelope {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let envelope = client
                .recv()
                .await
                .expect("read")
                .expect("the daemon hung up");
            if matches(&envelope) {
                return envelope;
            }
        }
    })
    .await
    .expect("the envelope never arrived")
}

/// The session this terminal was given, from its greeting.
async fn greeting(client: &mut Client) -> String {
    let envelope = until(client, |envelope| {
        matches!(envelope.frame, Frame::Hello { .. })
    })
    .await;
    envelope.session.expect("the greeting names the session")
}

fn home(temp: &Temp, config: &Config) -> Home {
    let home = Home::open_in(&temp.root, "test").expect("open");
    config.save(&home.config_file()).expect("save");
    // These tests are about the daemon, not about Bubblewrap. Leaving the
    // sandbox on would tie them to a kernel that grants unprivileged user
    // namespaces — which CI runners and every non-Linux machine do not.
    let sandbox = home.sandbox_file();
    std::fs::create_dir_all(sandbox.parent().expect("parent")).expect("dirs");
    std::fs::write(
        &sandbox,
        serde_json::json!({ "version": 1, "enabled": false }).to_string(),
    )
    .expect("write");
    home
}

/// A configuration that keeps a test to itself: no heartbeat, nothing to ask.
fn quiet() -> Config {
    Config {
        permissions: Permissions::Allow,
        heartbeat: aphid_alate::config::Heartbeat {
            every: "off".to_owned(),
            prompt: None,
        },
        ..Config::default()
    }
}

/// A model the daemon can resolve without any `~/.aphid/models.json` on the
/// machine running the tests. The scripted backend replaces the provider, so
/// this model is never contacted — it only has to resolve.
fn dummy_model() -> Model {
    let entry: ModelEntry = serde_json::from_value(serde_json::json!({
        "id": "test-model",
        "base_url": "http://localhost:8080/v1",
        "context_window": 32768,
        "max_tokens": 4096,
    }))
    .expect("a valid entry");
    Model::try_from(&entry).expect("a valid model")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_gets_a_conversation_of_its_own() {
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();

    let (stream_fn, _script) = scripted([Turn::text("I am awake.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut client = attach(&socket).await;
    let session = greeting(&mut client).await;

    client
        .send(&Request::Prompt {
            text: "are you there".to_owned(),
        })
        .await
        .expect("send");

    // The prompt is echoed into the session it went to, so two terminals in one
    // conversation agree on what was said.
    let echoed = until(
        &mut client,
        |envelope| matches!(&envelope.frame, Frame::Prompt { text } if text == "are you there"),
    )
    .await;
    assert_eq!(echoed.session.as_deref(), Some(session.as_str()));

    let reply = until(&mut client, |envelope| {
        matches!(envelope.frame, Frame::Text { .. })
    })
    .await;
    assert_eq!(reply.session.as_deref(), Some(session.as_str()));
    let Frame::Text { text } = reply.frame else {
        unreachable!("matched above")
    };
    assert_eq!(text, "I am awake.");

    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_is_not_put_in_the_resident_session() {
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();

    let (stream_fn, _script) = scripted([Turn::text("Hello.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut client = attach(&socket).await;
    let mine = greeting(&mut client).await;

    client.send(&Request::Sessions).await.expect("send");
    let Frame::Sessions { live, .. } = until(&mut client, |envelope| {
        matches!(envelope.frame, Frame::Sessions { .. })
    })
    .await
    .frame
    else {
        unreachable!("matched above")
    };

    // Two: the resident one the alate started with, and this terminal's.
    assert_eq!(live.len(), 2, "{live:?}");
    let resident = live
        .iter()
        .find(|info| info.kind == "resident")
        .expect("a resident session");
    assert_ne!(resident.id, mine, "a terminal gets its own conversation");
    assert!(live.iter().any(|info| info.id == mine));

    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_runs_in_a_session_of_its_own() {
    // The whole point of the change: a scheduled job neither reads nor
    // disturbs the conversation somebody is having.
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();

    // Written with a `last` well in the past, so the job is already overdue and
    // fires on the first clock tick rather than when the wall clock next
    // crosses a minute — which would make this test take up to a minute for no
    // extra confidence.
    let yesterday = (chrono::Local::now() - chrono::Duration::days(1)).to_rfc3339();
    std::fs::write(
        home.cron_file(),
        format!(
            r#"{{"version":1,"entries":[
                {{"name":"sweep","schedule":"* * * * *",
                  "prompt":"Check on things.","last":"{yesterday}"}}
            ]}}"#
        ),
    )
    .expect("write");
    // And it is a crontab this build can read, checked here so a failure below
    // is about sessions and not about a typo in the line above.
    let (crontab, problems) = Crontab::open(&home.cron_file());
    assert!(problems.is_empty(), "{problems:?}");
    assert!(crontab.find("sweep").is_some());
    drop(crontab);

    let (stream_fn, _script) = scripted([
        Turn::text("First."),
        Turn::text("Second."),
        Turn::text("Third."),
    ]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut client = attach(&socket).await;
    let mine = greeting(&mut client).await;

    // Asked for rather than watched for. The job is overdue, so it may well
    // have fired before this terminal finished attaching — and with no backlog
    // kept, a frame sent before a terminal arrives is one it never sees. What
    // the daemon *knows* does not expire, so that is what this asks.
    let job = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            client.send(&Request::Sessions).await.expect("send");
            let Frame::Sessions { live, stored } = until(&mut client, |envelope| {
                matches!(envelope.frame, Frame::Sessions { .. })
            })
            .await
            .frame
            else {
                unreachable!("matched above")
            };

            // Running, or finished and on disk: either proves it happened. A
            // session is listed as stored only once it is closed, and the only
            // thing that closes in this test is the job.
            if let Some(info) = live.iter().find(|info| info.kind.starts_with("cron")) {
                return info.clone();
            }
            if let Some(info) = stored.first() {
                return info.clone();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the job never ran");

    // A conversation of its own, and not the one being typed into.
    assert_ne!(job.id, mine, "a job does not land in a terminal's session");

    // Closed, not merely seen: `live` can catch the session before its run has
    // written a word, since starting it is a spawned task and not something
    // this loop waits on. Only a session in `stored` is guaranteed to have its
    // transcript flushed all the way to `on_run_end`.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            client.send(&Request::Sessions).await.expect("send");
            let Frame::Sessions { stored, .. } = until(&mut client, |envelope| {
                matches!(envelope.frame, Frame::Sessions { .. })
            })
            .await
            .frame
            else {
                unreachable!("matched above")
            };
            if stored.iter().any(|info| info.id == job.id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the job never closed");

    daemon.abort();

    // And what it did is in its own transcript, not in this terminal's. The
    // alate's home is "test" (`Home::open_in(&temp.root, "test")`), so that
    // is the project prefix on the session's filename.
    let path = temp.path("sessions").join(format!("test-{}.jsonl", job.id));
    let text = std::fs::read_to_string(&path).expect("the job's transcript");
    assert!(text.contains("Check on things."), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_the_agent_remembers_is_on_disk_afterwards() {
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();
    let memory_dir = home.memory_dir();

    let (stream_fn, _script) = scripted([
        Turn::call(
            "call-1",
            "remember",
            r#"{"path": "/projects/aphid", "fact": "The gateway is a Unix socket."}"#,
        ),
        Turn::text("Noted."),
    ]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut client = attach(&socket).await;
    greeting(&mut client).await;
    client
        .send(&Request::Prompt {
            text: "remember how the gateway works".to_owned(),
        })
        .await
        .expect("send");

    let Frame::ToolResult { name, is_error, .. } = until(&mut client, |envelope| {
        matches!(envelope.frame, Frame::ToolResult { .. })
    })
    .await
    .frame
    else {
        unreachable!("matched above")
    };
    assert_eq!(name, "remember");
    assert!(!is_error, "the tool should have succeeded");

    until(&mut client, |envelope| {
        matches!(envelope.frame, Frame::RunEnded { .. })
    })
    .await;
    daemon.abort();

    // The point of the whole crate: a later process still knows it.
    let hits = Memory::open(&memory_dir)
        .expect("open")
        .recall("gateway", None, 5)
        .expect("recall");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].fact.contains("Unix socket"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stored_fact_and_the_crontab_reach_the_model() {
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();

    // Written before the daemon starts, as a previous session would have.
    Memory::open(&home.memory_dir())
        .expect("open")
        .store("/people/thiago", "Thiago prefers a very small plugin API.")
        .expect("store");
    let (mut crontab, _) = Crontab::open(&home.cron_file());
    crontab
        .set("nightly", "0 3 * * *", "Tidy the notes.")
        .expect("set");
    drop(crontab);

    let (stream_fn, script) = scripted([Turn::text("I remember.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut client = attach(&socket).await;
    greeting(&mut client).await;
    client
        .send(&Request::Prompt {
            text: "what do you know about the plugin API".to_owned(),
        })
        .await
        .expect("send");
    until(&mut client, |envelope| {
        matches!(envelope.frame, Frame::RunEnded { .. })
    })
    .await;

    // What actually went on the wire, which is the only place this can be
    // checked without trusting the code that put it there.
    let sent = script.requests();
    let body = sent.last().expect("one request");

    // The recall arrives as a system note beside the prompt, never folded into
    // it, so the model can always tell what it was told now from what it knew.
    assert!(body.contains("recalled_facts"), "{body}");
    assert!(body.contains("very small plugin API"), "{body}");
    // The map of the memory and the list of jobs are in the system prompt; the
    // contents of neither are.
    assert!(body.contains("memory_paths"), "{body}");
    assert!(body.contains("/people/thiago"), "{body}");
    assert!(body.contains("scheduled_jobs"), "{body}");
    assert!(body.contains("nightly"), "{body}");

    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_heartbeat_wakes_the_resident_session() {
    let temp = Temp::new("daemon");
    let mut config = quiet();
    config.heartbeat.every = "1s".to_owned();
    config.heartbeat.prompt = Some("Look at your notes.".to_owned());
    let home = home(&temp, &config);
    let socket = home.socket();

    let (stream_fn, _script) = scripted([Turn::text("Nothing is due.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut client = attach(&socket).await;
    let mine = greeting(&mut client).await;

    let woke = until(&mut client, |envelope| {
        matches!(envelope.frame, Frame::Heartbeat { .. })
    })
    .await;
    let Frame::Heartbeat { note, .. } = woke.frame else {
        unreachable!("matched above")
    };
    assert_eq!(note, "Look at your notes.");
    // The alate's own frame, so every terminal sees it however it is occupied.
    assert_eq!(woke.session, None);
    let _ = mine;

    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_in_one_session_is_not_published_as_another_sessions() {
    // A terminal must see only the conversation it is having. This is the
    // cross-talk behind the "every heartbeat update" bug: every session mounts
    // its own `GatewayComponent` onto the one composition bus the daemon
    // shares, and a bus event is broadcast to every listener whatever session's
    // run produced it. So a run in one session is published once per mounted
    // session — tagged with the others' ids as well as its own — and a terminal
    // that never said anything sees a run under its own session id.
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();

    let (stream_fn, _script) = scripted([Turn::text("Said in the first session.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut first = attach(&socket).await;
    let mine = greeting(&mut first).await;

    // A second terminal, keeping its own conversation.
    let mut second = attach(&socket).await;
    let theirs = greeting(&mut second).await;

    // Only the first terminal speaks. Its prompt echoes into its own session…
    first
        .send(&Request::Prompt {
            text: "say something".to_owned(),
        })
        .await
        .expect("send");
    until(
        &mut first,
        |envelope| matches!(&envelope.frame, Frame::Prompt { text } if text == "say something"),
    )
    .await;

    // … and its run ends in its own session. Waiting for this means the run is
    // over before the second terminal's silence is judged: with the bug, the
    // leaked frames were published under both ids at the same moment, so
    // anything that was going to leak is already in the second terminal's
    // channel.
    let ended = until(&mut first, |envelope| {
        matches!(envelope.frame, Frame::RunEnded { .. })
    })
    .await;
    assert_eq!(ended.session.as_deref(), Some(mine.as_str()));

    // The second terminal never said anything, so nothing under its own
    // session id may arrive — that run belongs to the first's conversation.
    // Daemon-level frames (no session) are this terminal's business too, which
    // is why the scan skips them rather than complaining about the first
    // envelope that arrives.
    let leaked = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let envelope = second
                .recv()
                .await
                .expect("read")
                .expect("the daemon hung up");
            if envelope.session.as_deref() == Some(theirs.as_str()) {
                return Some(envelope);
            }
        }
    })
    .await;
    if let Ok(Some(envelope)) = leaked {
        panic!("the first session's run leaked into the second's own session: {envelope:?}");
    }

    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_heartbeat_run_does_not_leak_into_an_attached_sessions_chat() {
    // The user-visible face of the cross-talk: the resident session wakes on a
    // heartbeat, and the reply must not arrive in a conversation that never
    // asked for anything. With the bug, the resident's turn frames were
    // published under the attached terminal's session id, so the terminal
    // received a run that was never its own.
    let temp = Temp::new("daemon");
    let mut config = quiet();
    config.heartbeat.every = "3s".to_owned();
    config.heartbeat.prompt = Some("Look at your notes.".to_owned());
    let home = home(&temp, &config);
    let socket = home.socket();

    let (stream_fn, _script) = scripted([Turn::text("Nothing is due.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut client = attach(&socket).await;
    let mine = greeting(&mut client).await;

    // The daemon announces the wake to every terminal; that is its own frame
    // and every terminal's business. It also names the resident, whose run we
    // wait for by watching the run flag through `/sessions` — the resident
    // goes running, then idle, and only then is its run over.
    let Frame::Heartbeat { .. } = until(&mut client, |envelope| {
        matches!(envelope.frame, Frame::Heartbeat { .. })
    })
    .await
    .frame
    else {
        unreachable!("matched above")
    };

    tokio::time::timeout(Duration::from_secs(30), async {
        // A run that has not started yet also reads as idle, so first wait for
        // it to start, then for it to end.
        let mut saw_running = false;
        loop {
            client.send(&Request::Sessions).await.expect("send");
            let Frame::Sessions { live, .. } = until(&mut client, |envelope| {
                matches!(envelope.frame, Frame::Sessions { .. })
            })
            .await
            .frame
            else {
                unreachable!("matched above")
            };
            let resident = live
                .iter()
                .find(|info| info.kind == "resident")
                .expect("the resident session");
            if resident.running {
                saw_running = true;
            } else if saw_running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the resident's heartbeat run never finished");

    // The resident's run is over, so anything that was going to leak is already
    // in this terminal's channel. It must not see a single frame under its own
    // session id — it never said anything.
    let leaked = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let envelope = client
                .recv()
                .await
                .expect("read")
                .expect("the daemon hung up");
            if envelope.session.as_deref() == Some(mine.as_str()) {
                return Some(envelope);
            }
        }
    })
    .await;
    if let Ok(Some(envelope)) = leaked {
        panic!("the resident's heartbeat run leaked into this conversation: {envelope:?}");
    }

    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_transcript_contains_only_its_own_session() {
    // The same shared-subscription flaw, one channel further out: every
    // session's transcript component heard every session's announcements, so
    // each session file ended up holding every conversation. Each file must
    // hold exactly the conversation it is named after.
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();
    let sessions = temp.path("sessions");

    let (stream_fn, _script) = scripted([Turn::text("Reply one."), Turn::text("Reply two.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut first = attach(&socket).await;
    let a = greeting(&mut first).await;
    let mut second = attach(&socket).await;
    let b = greeting(&mut second).await;

    first
        .send(&Request::Prompt {
            text: "first conversation".to_owned(),
        })
        .await
        .expect("send");
    until(&mut first, |envelope| {
        matches!(envelope.frame, Frame::RunEnded { .. })
    })
    .await;

    second
        .send(&Request::Prompt {
            text: "second conversation".to_owned(),
        })
        .await
        .expect("send");
    until(&mut second, |envelope| {
        matches!(envelope.frame, Frame::RunEnded { .. })
    })
    .await;

    daemon.abort();

    // The transcripts are already on disk — the session component appends as
    // messages are committed — and each is named after its session.
    let path = |id: &str| sessions.join(format!("test-{id}.jsonl"));
    let first_text = std::fs::read_to_string(path(&a)).expect("the first transcript");
    let second_text = std::fs::read_to_string(path(&b)).expect("the second transcript");

    assert!(first_text.contains("first conversation"), "{first_text}");
    assert!(first_text.contains("Reply one."), "{first_text}");
    assert!(
        !first_text.contains("second conversation"),
        "the second conversation leaked into the first's transcript: {first_text}"
    );
    assert!(
        !first_text.contains("Reply two."),
        "the second conversation leaked into the first's transcript: {first_text}"
    );

    assert!(second_text.contains("second conversation"), "{second_text}");
    assert!(second_text.contains("Reply two."), "{second_text}");
    assert!(
        !second_text.contains("first conversation"),
        "the first conversation leaked into the second's transcript: {second_text}"
    );
    assert!(
        !second_text.contains("Reply one."),
        "the first conversation leaked into the second's transcript: {second_text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_script_hook_fires_once_per_event_whatever_session_runs() {
    // Scripts are daemon-wide: a hook must fire once per event, wherever the
    // run happens, and never once per mounted session. Mounting the script
    // host in every session made each event hit every hook N times — two
    // conversations meant two notifications per turn. This test plants a
    // plugin that announces every turn start, then runs the resident session
    // (a heartbeat) and an attached one, and counts the notices.
    let temp = Temp::new("daemon");
    let mut config = quiet();
    config.heartbeat.every = "3s".to_owned();
    config.heartbeat.prompt = Some("Look at your notes.".to_owned());
    let home = home(&temp, &config);
    let socket = home.socket();

    // Discovered from the workspace, which is the home unless configured
    // otherwise.
    temp.write(
        "test/.aphid/plugins/counter.rhai",
        "fn apply(ctx) { on(\"agent/turn-start\", |cx| { notify(\"turn-start\") }); }\n",
    );

    let (stream_fn, _script) = scripted([Turn::text("Nothing is due."), Turn::text("A reply.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut client = attach(&socket).await;
    let mine = greeting(&mut client).await;

    // The resident wakes on the heartbeat. Its one turn must fire the hook
    // exactly once; anything a second mount would have added is queued in the
    // same instant, so a short silence after the first notice settles it.
    let mut seen: Vec<Envelope> = Vec::new();
    let resident = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let envelope = client
                .recv()
                .await
                .expect("read")
                .expect("the daemon hung up");
            if matches!(&envelope.frame, Frame::Notice { text } if text.contains("turn-start")) {
                return envelope;
            }
            seen.push(envelope);
        }
    })
    .await
    .expect("the hook never fired for the resident's heartbeat");

    match tokio::time::timeout(Duration::from_millis(500), client.recv()).await {
        Ok(Ok(Some(envelope))) => {
            panic!("the resident's one turn fired the hook more than once: {envelope:?}")
        }
        Ok(Ok(None)) => panic!("the daemon hung up"),
        Ok(Err(error)) => panic!("read error: {error}"),
        // Silence: one firing per turn, as daemon-wide scripts should be.
        Err(_) => {}
    }

    // The attached conversation's turn must fire it once more, and only once:
    // the hook still hears every session, but each event exactly one time.
    client
        .send(&Request::Prompt {
            text: "say something".to_owned(),
        })
        .await
        .expect("send");
    loop {
        let envelope = client
            .recv()
            .await
            .expect("read")
            .expect("the daemon hung up");
        if matches!(envelope.frame, Frame::RunEnded { .. }) {
            break;
        }
        seen.push(envelope);
    }

    // The notice was published during the run, before its end, and a heartbeat
    // is at least 3s away — so whatever the run produced is already in hand.
    assert_eq!(
        seen.iter()
            .filter(|envelope| matches!(&envelope.frame, Frame::Notice { text } if text.contains("turn-start")))
            .count(),
        1,
        "the attached session's one turn must add exactly one more notice"
    );
    let _ = (resident, mine);

    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_can_be_watched_after_it_ended() {
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();

    let (stream_fn, _script) = scripted([Turn::text("Something worth keeping.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut first = attach(&socket).await;
    let ended = greeting(&mut first).await;
    first
        .send(&Request::Prompt {
            text: "say something".to_owned(),
        })
        .await
        .expect("send");
    until(&mut first, |envelope| {
        matches!(envelope.frame, Frame::RunEnded { .. })
    })
    .await;

    // That terminal leaves, so its session ends. The transcript does not.
    drop(first);

    let mut second = attach(&socket).await;
    greeting(&mut second).await;
    second
        .send(&Request::Watch { id: ended.clone() })
        .await
        .expect("send");

    until(
        &mut second,
        |envelope| matches!(&envelope.frame, Frame::HistoryStart { id } if *id == ended),
    )
    .await;
    // Replayed from its transcript: what was asked, and what was answered.
    until(
        &mut second,
        |envelope| matches!(&envelope.frame, Frame::Prompt { text } if text == "say something"),
    )
    .await;
    until(&mut second, |envelope| {
        matches!(&envelope.frame, Frame::Text { text } if text == "Something worth keeping.")
    })
    .await;
    until(
        &mut second,
        |envelope| matches!(&envelope.frame, Frame::HistoryEnd { id } if *id == ended),
    )
    .await;

    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_session_writes_its_own_transcript() {
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();
    let sessions = temp.path("sessions");

    let (stream_fn, _script) = scripted([Turn::text("Said out loud.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    let mut client = attach(&socket).await;
    greeting(&mut client).await;
    client
        .send(&Request::Prompt {
            text: "say something".to_owned(),
        })
        .await
        .expect("send");
    until(&mut client, |envelope| {
        matches!(envelope.frame, Frame::RunEnded { .. })
    })
    .await;
    daemon.abort();

    // One for the resident session, one for the terminal's.
    let files: Vec<_> = std::fs::read_dir(&sessions)
        .expect("sessions")
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert_eq!(files.len(), 2, "{files:?}");

    let said = files.iter().any(|path| {
        std::fs::read_to_string(path)
            .map(|text| text.contains("Said out loud."))
            .unwrap_or(false)
    });
    assert!(said, "the reply should be in exactly one of them");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_daemons_cannot_share_one_alate() {
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();

    let (stream_fn, _script) = scripted([Turn::text("First.")]);
    let first = tokio::spawn(daemon::run(Options {
        home: Home::open_in(&temp.root, "test").expect("open"),
        config: config.clone(),
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));
    let _client = attach(&socket).await;

    let (stream_fn, _script) = scripted([Turn::text("Second.")]);
    let refused = daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    })
    .await
    .expect_err("the second daemon should be refused");
    assert!(refused.contains("already"), "{refused}");

    first.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_is_listed_under_the_channel_it_belongs_to() {
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();

    let (stream_fn, _script) = scripted([Turn::text("hello")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        model: Some(dummy_model()),
        stream_fn: Some(stream_fn),
        sessions_dir: temp.path("sessions"),
    }));

    // A terminal, which says nothing about itself.
    let mut terminal = attach(&socket).await;
    let plain = greeting(&mut terminal).await;

    // And a client that does.
    let mut bot = Client::connect_as(&socket, Some("telegram: 42"))
        .await
        .expect("connect");
    let chat = greeting(&mut bot).await;

    terminal.send(&Request::Sessions).await.expect("send");
    let listed = until(&mut terminal, |envelope| {
        matches!(&envelope.frame, Frame::Sessions { .. })
    })
    .await;
    let Frame::Sessions { live, .. } = listed.frame else {
        panic!("a list");
    };

    let kind = |id: &str| {
        live.iter()
            .find(|info| info.id == id)
            .unwrap_or_else(|| panic!("{id} is listed among {live:?}"))
            .kind
            .clone()
    };
    assert_eq!(kind(&chat), "telegram: 42");
    assert_eq!(kind(&plain), "attached");
    // The resident one belongs to no channel and never did.
    assert!(live.iter().any(|info| info.kind == "resident"));

    // And a second conversation on the same connection keeps the channel: it
    // is the client that is a chat, not the conversation.
    bot.send(&Request::New).await.expect("send");
    let opened = until(&mut terminal, |envelope| {
        matches!(&envelope.frame, Frame::SessionOpened { info } if info.id != chat && info.id != plain)
    })
    .await;
    let Frame::SessionOpened { info } = opened.frame else {
        panic!("a session");
    };
    assert_eq!(info.kind, "telegram: 42");

    drop(terminal);
    drop(bot);
    daemon.abort();
}
