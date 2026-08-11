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
        stream_fn: Some(stream_fn),
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
        stream_fn: Some(stream_fn),
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
        stream_fn: Some(stream_fn),
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

    daemon.abort();

    // And what it did is in its own transcript, not in this terminal's.
    let path = temp
        .path("test/.aphid/sessions")
        .join(format!("{}.jsonl", job.id));
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
        stream_fn: Some(stream_fn),
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
        stream_fn: Some(stream_fn),
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
        stream_fn: Some(stream_fn),
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
async fn a_session_can_be_watched_after_it_ended() {
    let temp = Temp::new("daemon");
    let config = quiet();
    let home = home(&temp, &config);
    let socket = home.socket();

    let (stream_fn, _script) = scripted([Turn::text("Something worth keeping.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        stream_fn: Some(stream_fn),
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
    let sessions = home.aphid_dir().join("sessions");

    let (stream_fn, _script) = scripted([Turn::text("Said out loud.")]);
    let daemon = tokio::spawn(daemon::run(Options {
        home,
        config,
        stream_fn: Some(stream_fn),
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
        stream_fn: Some(stream_fn),
    }));
    let _client = attach(&socket).await;

    let (stream_fn, _script) = scripted([Turn::text("Second.")]);
    let refused = daemon::run(Options {
        home,
        config,
        stream_fn: Some(stream_fn),
    })
    .await
    .expect_err("the second daemon should be refused");
    assert!(refused.contains("already"), "{refused}");

    first.abort();
}
