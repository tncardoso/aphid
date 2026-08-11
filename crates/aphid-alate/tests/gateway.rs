//! The socket: the envelopes, who sees what, and the log.

mod common;

use std::time::Duration;

use aphid_alate::gateway::wire::{Answer, Envelope, Frame, Request, Risk};
use aphid_alate::gateway::{Client, Event, Server, is_listening};
use aphid_alate::sessions::Info;
use aphid_code::plugins::permissions::{Decision, Risk as PermissionRisk};
use aphid_core::{StopReason, Usage};
use common::Temp;

/// Wait for an envelope, rather than for a fixed time.
///
/// A test that sleeps is flaky on a loaded machine and slow on an idle one.
async fn next(client: &mut Client) -> Envelope {
    tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .expect("an envelope within five seconds")
        .expect("read")
        .expect("the daemon did not hang up")
}

async fn event(events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("an event within five seconds")
        .expect("the server did not close")
}

fn info(id: &str) -> Info {
    Info {
        id: id.to_owned(),
        kind: "attached".to_owned(),
        started: "2026-08-11 09:00".to_owned(),
        running: false,
    }
}

#[test]
fn every_frame_round_trips() {
    let frames = vec![
        Frame::Hello {
            instance: "work".to_owned(),
            model: "some-model".to_owned(),
            context_window: 128_000,
            thinking: Some("medium".to_owned()),
        },
        Frame::SessionOpened { info: info("s-1") },
        Frame::SessionClosed {
            id: "s-1".to_owned(),
        },
        Frame::Sessions {
            live: vec![info("s-1")],
            stored: vec![info("s-0")],
        },
        Frame::HistoryStart {
            id: "s-1".to_owned(),
        },
        Frame::HistoryEnd {
            id: "s-1".to_owned(),
        },
        Frame::TurnStarted,
        Frame::Text {
            text: "hello".to_owned(),
        },
        Frame::Thinking {
            text: "hmm".to_owned(),
        },
        Frame::ToolStreamStart {
            block: 1,
            name: "bash".to_owned(),
        },
        Frame::ToolStreamDelta { block: 1, bytes: 9 },
        Frame::ToolCall {
            id: "call-1".to_owned(),
            name: "bash".to_owned(),
            arguments: r#"{"command":"ls"}"#.to_owned(),
        },
        Frame::ToolProgress {
            id: "call-1".to_owned(),
            chunk: "a.txt\n".to_owned(),
        },
        Frame::ToolResult {
            id: "call-1".to_owned(),
            name: "bash".to_owned(),
            text: "a.txt".to_owned(),
            is_error: false,
            details: Some(serde_json::json!({ "status": 0 })),
        },
        Frame::TurnEnded {
            usage: Usage::default(),
            stop: StopReason::Stop,
            error: None,
        },
        Frame::RunEnded {
            stop: StopReason::Stop,
            turns: 2,
            error: None,
        },
        Frame::Notice {
            text: "a note".to_owned(),
        },
        Frame::Prompt {
            text: "do the thing".to_owned(),
        },
        Frame::Heartbeat {
            at: "2026-08-11 09:00 -03".to_owned(),
            note: "look at your memory".to_owned(),
        },
        Frame::Confirm {
            id: 7,
            tool: "bash".to_owned(),
            summary: "rm -rf /".to_owned(),
            risk: Risk::Destructive,
        },
    ];

    for frame in frames {
        for envelope in [
            Envelope::daemon(frame.clone()),
            Envelope::from("s-1", frame.clone()),
        ] {
            let line = serde_json::to_string(&envelope).expect("write");
            assert!(!line.contains('\n'), "an envelope is one line: {line}");
            assert_eq!(
                serde_json::from_str::<Envelope>(&line).expect("read"),
                envelope,
                "{line}"
            );
        }
    }
}

#[test]
fn every_request_round_trips() {
    for request in [
        Request::Prompt {
            text: "hello".to_owned(),
        },
        Request::Cancel,
        Request::Answer {
            id: 3,
            decision: Answer::AllowAlways,
        },
        Request::Watch {
            id: "s-1".to_owned(),
        },
        Request::Sessions,
        Request::New,
    ] {
        let line = serde_json::to_string(&request).expect("write");
        assert_eq!(
            serde_json::from_str::<Request>(&line).expect("read"),
            request
        );
    }
}

#[test]
fn an_envelope_goes_to_the_terminals_looking_at_it() {
    let daemon = Envelope::daemon(Frame::TurnStarted);
    let mine = Envelope::from("s-1", Frame::TurnStarted);

    // The daemon's own frames go to everybody, whatever they are watching.
    assert!(daemon.is_for(None));
    assert!(daemon.is_for(Some("s-1")));
    assert!(daemon.is_for(Some("s-2")));

    // A conversation's frames go only to the terminals in it.
    assert!(mine.is_for(Some("s-1")));
    assert!(!mine.is_for(Some("s-2")));
    assert!(!mine.is_for(None));
}

#[test]
fn risk_survives_the_trip_both_ways() {
    for risk in [
        PermissionRisk::Read,
        PermissionRisk::Mutate,
        PermissionRisk::Destructive,
    ] {
        assert_eq!(PermissionRisk::from(Risk::from(risk)), risk);
    }
    assert_eq!(Decision::from(Answer::Allow), Decision::Allow);
    assert_eq!(Decision::from(Answer::AllowAlways), Decision::AllowAlways);
    assert_eq!(Decision::from(Answer::Deny), Decision::Deny);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_is_announced_and_can_ask() {
    let temp = Temp::new("gateway");
    let socket = temp.path("gateway.sock");
    let (server, mut events) = Server::bind(&socket, None).expect("bind");

    // A probe connects and says nothing, so it opens no conversation.
    assert!(is_listening(&socket));
    let mut client = Client::connect(&socket).await.expect("connect");

    // The daemon hears that a terminal arrived, and opens a session for it.
    let Event::Opened { connection } = event(&mut events).await else {
        panic!("expected an opened event");
    };
    server.watch(connection, "s-1");

    client
        .send(&Request::Prompt {
            text: "hello".to_owned(),
        })
        .await
        .expect("send");

    // What it asked for arrives with the session it was looking at.
    assert_eq!(
        event(&mut events).await,
        Event::Asked {
            connection,
            session: Some("s-1".to_owned()),
            request: Request::Prompt {
                text: "hello".to_owned()
            },
        }
    );

    server.send(Envelope::from(
        "s-1",
        Frame::Text {
            text: "hi".to_owned(),
        },
    ));
    assert_eq!(
        next(&mut client).await,
        Envelope::from(
            "s-1",
            Frame::Text {
                text: "hi".to_owned()
            }
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_that_leaves_is_reported() {
    let temp = Temp::new("gateway");
    let socket = temp.path("gateway.sock");
    let (_server, mut events) = Server::bind(&socket, None).expect("bind");

    let client = Client::connect(&socket).await.expect("connect");
    let Event::Opened { connection } = event(&mut events).await else {
        panic!("expected an opened event");
    };

    // Which is what ends the session that terminal owned.
    drop(client);
    assert_eq!(event(&mut events).await, Event::Closed { connection });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn two_terminals_on_two_sessions_do_not_see_each_other() {
    let temp = Temp::new("gateway");
    let socket = temp.path("gateway.sock");
    let (server, mut events) = Server::bind(&socket, None).expect("bind");

    let mut one = Client::connect(&socket).await.expect("connect");
    let Event::Opened { connection: first } = event(&mut events).await else {
        panic!("expected an opened event");
    };
    let mut two = Client::connect(&socket).await.expect("connect");
    let Event::Opened { connection: second } = event(&mut events).await else {
        panic!("expected an opened event");
    };

    server.watch(first, "s-1");
    server.watch(second, "s-2");

    server.send(Envelope::from(
        "s-2",
        Frame::Text {
            text: "for the second".to_owned(),
        },
    ));
    // The daemon's own frames reach everybody, so this is what the first
    // terminal sees next — and not the line above it.
    server.send(Envelope::daemon(Frame::Notice {
        text: "for everybody".to_owned(),
    }));

    assert_eq!(
        next(&mut one).await,
        Envelope::daemon(Frame::Notice {
            text: "for everybody".to_owned()
        })
    );
    assert_eq!(
        next(&mut two).await,
        Envelope::from(
            "s-2",
            Frame::Text {
                text: "for the second".to_owned()
            }
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_reply_reaches_one_terminal_and_not_the_others() {
    let temp = Temp::new("gateway");
    let socket = temp.path("gateway.sock");
    let (server, mut events) = Server::bind(&socket, None).expect("bind");

    let mut one = Client::connect(&socket).await.expect("connect");
    let Event::Opened { connection: first } = event(&mut events).await else {
        panic!("expected an opened event");
    };
    let mut two = Client::connect(&socket).await.expect("connect");
    let Event::Opened { .. } = event(&mut events).await else {
        panic!("expected an opened event");
    };

    // A session list is an answer to one question somebody asked.
    server.reply(
        first,
        Envelope::daemon(Frame::Sessions {
            live: vec![info("s-1")],
            stored: Vec::new(),
        }),
    );
    server.send(Envelope::daemon(Frame::Notice {
        text: "for everybody".to_owned(),
    }));

    // A reply and a broadcast race each other, so this counts rather than
    // ordering: the first terminal gets both, in whichever order.
    let mut seen = vec![next(&mut one).await.frame, next(&mut one).await.frame];
    assert_eq!(
        seen.iter()
            .filter(|frame| matches!(frame, Frame::Sessions { .. }))
            .count(),
        1,
        "{seen:?}"
    );

    // The second terminal never sees the list — only what was for everybody.
    seen = vec![next(&mut two).await.frame];
    assert!(matches!(seen[0], Frame::Notice { .. }), "{seen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tool_waits_for_the_terminal_that_answers_it() {
    let temp = Temp::new("gateway");
    let socket = temp.path("gateway.sock");
    let (server, mut events) = Server::bind(&socket, None).expect("bind");
    let confirmer = server.confirmer();

    let mut client = Client::connect(&socket).await.expect("connect");
    let Event::Opened { .. } = event(&mut events).await else {
        panic!("expected an opened event");
    };

    let asking = tokio::task::spawn_blocking(move || {
        confirmer.confirm("bash", "rm -rf /", PermissionRisk::Destructive)
    });

    // The question is the daemon's, not a conversation's: it reaches a terminal
    // that is watching nothing in particular.
    let envelope = next(&mut client).await;
    assert_eq!(envelope.session, None);
    let Frame::Confirm { id, tool, risk, .. } = envelope.frame else {
        panic!("expected a confirmation");
    };
    assert_eq!(tool, "bash");
    assert_eq!(risk, Risk::Destructive);

    client
        .send(&Request::Answer {
            id,
            decision: Answer::Deny,
        })
        .await
        .expect("send");

    assert_eq!(asking.await.expect("join"), Decision::Deny);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nobody_attached_is_nobody_to_ask() {
    // An unattended alate that allowed instead would be one that talks itself
    // into anything overnight.
    let temp = Temp::new("gateway");
    let socket = temp.path("gateway.sock");
    let (server, _events) = Server::bind(&socket, None).expect("bind");
    let confirmer = server.confirmer();

    let decision = tokio::task::spawn_blocking(move || {
        confirmer.confirm("bash", "rm -rf /", PermissionRisk::Destructive)
    })
    .await
    .expect("join");
    assert_eq!(decision, Decision::Deny);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_happened_unattended_is_still_readable() {
    let temp = Temp::new("gateway");
    let socket = temp.path("gateway.sock");
    let log = temp.path("alate.log");
    let (server, _events) = Server::bind(&socket, Some(&log)).expect("bind");

    server.send(Envelope::from(
        "s-1",
        Frame::Heartbeat {
            at: "2026-08-11 09:00 -03".to_owned(),
            note: "nobody was watching".to_owned(),
        },
    ));

    let text = std::fs::read_to_string(&log).expect("read");
    let envelope: Envelope = serde_json::from_str(text.trim()).expect("one envelope on one line");
    // Which conversation it belonged to is in the log too, so a day of them can
    // be told apart afterwards.
    assert_eq!(envelope.session.as_deref(), Some("s-1"));
    assert!(matches!(envelope.frame, Frame::Heartbeat { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_socket_goes_when_the_server_does() {
    // A socket left behind is one that `attach` tries, fails on, and reports as
    // a daemon misbehaving rather than one that is not there.
    let temp = Temp::new("gateway");
    let socket = temp.path("gateway.sock");

    let (server, _events) = Server::bind(&socket, None).expect("bind");
    assert!(socket.exists());
    drop(server);
    assert!(!socket.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_nobody_is_behind_is_taken_over() {
    let temp = Temp::new("gateway");
    let socket = temp.path("gateway.sock");
    std::fs::write(&socket, "left by a daemon that was killed").expect("write");

    let (_server, _events) = Server::bind(&socket, None).expect("bind");
    assert!(is_listening(&socket));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_somebody_is_behind_is_not() {
    let temp = Temp::new("gateway");
    let socket = temp.path("gateway.sock");

    let (_first, _events) = Server::bind(&socket, None).expect("bind");
    let Err(error) = Server::bind(&socket, None) else {
        panic!("two daemons must not serve one alate");
    };
    // The daemon turns this kind, and only this kind, into "already running".
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_path_too_long_to_be_a_socket_says_so() {
    // The kernel's own error names neither the limit nor the path, so a long
    // $APHID_HOME would otherwise be reported as a mystery — or, worse, as
    // another daemon already running.
    let temp = Temp::new("gateway");
    let socket = temp.path(&format!("{}.sock", "d".repeat(120)));

    let Err(error) = Server::bind(&socket, None) else {
        panic!("a path that long cannot be a socket");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("gateway.socket"), "{error}");
}
