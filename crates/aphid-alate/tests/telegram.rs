//! The Telegram bridge: what a chat says becomes a request, and what the
//! session says becomes a message.
//!
//! The daemon is not needed for any of this. The bridge is a gateway client, so
//! a bare [`Server`] and two lines playing the daemon's part — open a session
//! for a connection and greet it — is the whole of the far side.

#![cfg(feature = "telegram")]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_alate::config::Telegram;
use aphid_alate::gateway::wire::{Envelope, Frame, Request, Risk};
use aphid_alate::gateway::{Event, Server};
use aphid_alate::telegram::{self, Api, Bridge, Call};
use aphid_code::plugins::permissions::{Decision, Risk as PermissionRisk};
use aphid_core::{StopReason, Usage};
use common::Temp;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const SESSION: &str = "s-1";
const MINE: i64 = 42;
const THEIRS: i64 = 7;

/// A Telegram that says what a test feeds it, and remembers what it was told.
///
/// The updates arrive through a channel rather than a script, so a test says
/// **when** each one is delivered. That matters for a button: the press has to
/// come after the question, and a script would hand it over first.
struct Fake {
    updates: tokio::sync::Mutex<UnboundedReceiver<Value>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl Fake {
    fn new() -> (Arc<Self>, UnboundedSender<Value>) {
        let (feed, updates) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(Self {
                updates: tokio::sync::Mutex::new(updates),
                calls: Mutex::new(Vec::new()),
            }),
            feed,
        )
    }

    /// Every call of one method, in order.
    fn calls(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .expect("lock")
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, body)| body.clone())
            .collect()
    }

    /// Wait for the nth call of a method, rather than for a fixed time.
    async fn nth(&self, method: &str, index: usize) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(body) = self.calls(method).get(index) {
                return body.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no {method} number {index} within five seconds; \
                 what was called: {:?}",
                self.calls.lock().expect("lock")
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Api for Fake {
    fn call(&self, method: &'static str, body: Value) -> Call<'_> {
        self.calls
            .lock()
            .expect("lock")
            .push((method.to_owned(), body));
        Box::pin(async move {
            if method != "getUpdates" {
                return Ok(json!({ "message_id": 1 }));
            }
            // Nothing until the test says so, which is what a long poll with a
            // quiet chat behind it does.
            match self.updates.lock().await.recv().await {
                Some(updates) => Ok(updates),
                None => Ok(json!([])),
            }
        })
    }
}

fn message(id: i64, chat: i64, text: &str) -> Value {
    json!({ "update_id": id, "message": { "chat": { "id": chat }, "text": text } })
}

fn allowed() -> Telegram {
    Telegram {
        chats: vec![MINE],
        // Nothing waits on this: the fake answers at once and then holds.
        poll: "1s".to_owned(),
        ..Telegram::default()
    }
}

/// Bind a gateway, and start a bridge on it.
fn bridge(
    temp: &Temp,
    config: Telegram,
    api: Arc<Fake>,
) -> (
    Server,
    UnboundedReceiver<Event>,
    tokio::task::JoinHandle<()>,
) {
    let socket = temp.path("gateway.sock");
    let (server, events) = Server::bind(&socket, None).expect("bind");
    let running = telegram::spawn(Bridge {
        socket,
        config,
        api,
        notices: server.publisher(),
    });
    (server, events, running)
}

async fn event(events: &mut UnboundedReceiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("an event within five seconds")
        .expect("the server did not close")
}

/// What the daemon does when a client attaches: give it a session, and say so.
fn greet(server: &Server, connection: u64) {
    server.watch(connection, SESSION);
    server.reply(
        connection,
        Envelope::from(
            SESSION,
            Frame::Hello {
                instance: "test".to_owned(),
                model: "some-model".to_owned(),
                context_window: 128_000,
                thinking: None,
            },
        ),
    );
}

fn text(body: &Value) -> String {
    body["text"].as_str().expect("a text").to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_becomes_a_prompt() {
    let temp = Temp::new("telegram-prompt");
    let (api, feed) = Fake::new();
    let (server, mut events, bridge) = bridge(&temp, allowed(), api.clone());
    feed.send(json!([message(1, MINE, "hello")])).expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("the first event is the chat attaching");
    };
    greet(&server, connection);

    assert_eq!(
        event(&mut events).await,
        Event::Asked {
            connection,
            session: Some(SESSION.to_owned()),
            request: Request::Prompt {
                text: "hello".to_owned()
            },
        }
    );

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_the_agent_says_arrives_as_one_message() {
    let temp = Temp::new("telegram-reply");
    let (api, feed) = Fake::new();
    let (server, mut events, bridge) = bridge(&temp, allowed(), api.clone());
    feed.send(json!([message(1, MINE, "hello")])).expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("attached");
    };
    greet(&server, connection);
    event(&mut events).await;

    // The turn, as the gateway plugin would publish it.
    server.send(Envelope::from(SESSION, Frame::TurnStarted));
    server.send(Envelope::from(
        SESSION,
        Frame::Text {
            text: "the answer ".to_owned(),
        },
    ));
    server.send(Envelope::from(
        SESSION,
        Frame::Text {
            text: "is 42".to_owned(),
        },
    ));
    server.send(Envelope::from(
        SESSION,
        Frame::TurnEnded {
            usage: Usage::default(),
            stop: StopReason::Stop,
            error: None,
        },
    ));

    let sent = api.nth("sendMessage", 0).await;
    assert_eq!(sent["chat_id"], MINE);
    // The deltas whole, in one message, and no message for each of them.
    assert_eq!(text(&sent), "the answer is 42");
    assert!(
        api.calls("sendChatAction")
            .iter()
            .any(|body| body["action"] == "typing"),
        "the chat is shown that the agent is working"
    );

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_session_is_not_this_chat_s_business() {
    let temp = Temp::new("telegram-elsewhere");
    let (api, feed) = Fake::new();
    let (server, mut events, bridge) = bridge(&temp, allowed(), api.clone());
    feed.send(json!([message(1, MINE, "hello")])).expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("attached");
    };
    greet(&server, connection);
    event(&mut events).await;

    // Somebody else's conversation, and then this one's.
    server.send(Envelope::from(
        "s-2",
        Frame::Text {
            text: "not for you".to_owned(),
        },
    ));
    server.send(Envelope::from(
        SESSION,
        Frame::Text {
            text: "for you".to_owned(),
        },
    ));
    server.send(Envelope::from(
        SESSION,
        Frame::TurnEnded {
            usage: Usage::default(),
            stop: StopReason::Stop,
            error: None,
        },
    ));

    assert_eq!(text(&api.nth("sendMessage", 0).await), "for you");
    assert_eq!(api.calls("sendMessage").len(), 1);

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_that_is_not_allowed_is_told_its_id_once() {
    let temp = Temp::new("telegram-refused");
    let (api, feed) = Fake::new();
    let (_server, mut events, bridge) = bridge(&temp, allowed(), api.clone());
    feed.send(json!([
        message(1, THEIRS, "let me in"),
        message(2, THEIRS, "let me in again"),
    ]))
    .expect("feed");

    let refusal = api.nth("sendMessage", 0).await;
    assert_eq!(refusal["chat_id"], THEIRS);
    assert!(
        text(&refusal).contains(&THEIRS.to_string()),
        "the refusal names the id to add: {}",
        text(&refusal)
    );

    // The second try is ignored, so a stranger cannot make the bot answer for
    // ever, and nothing was attached for either.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(api.calls("sendMessage").len(), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .is_err(),
        "a chat that is not allowed opens no connection"
    );

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_chat_keeps_one_connection() {
    let temp = Temp::new("telegram-reuse");
    let (api, feed) = Fake::new();
    let (server, mut events, bridge) = bridge(&temp, allowed(), api.clone());
    feed.send(json!([message(1, MINE, "first")])).expect("feed");
    feed.send(json!([message(2, MINE, "second")]))
        .expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("attached");
    };
    greet(&server, connection);

    for said in ["first", "second"] {
        assert_eq!(
            event(&mut events).await,
            Event::Asked {
                connection,
                session: Some(SESSION.to_owned()),
                request: Request::Prompt {
                    text: said.to_owned()
                },
            },
            "both messages go down the same connection"
        );
    }

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_commands_are_requests_and_the_rest_is_words() {
    let temp = Temp::new("telegram-commands");
    let (api, feed) = Fake::new();
    let (server, mut events, bridge) = bridge(&temp, allowed(), api.clone());
    feed.send(json!([message(1, MINE, "/new")])).expect("feed");
    feed.send(json!([message(2, MINE, "/cancel")]))
        .expect("feed");
    feed.send(json!([message(3, MINE, "/start")]))
        .expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("attached");
    };
    greet(&server, connection);

    let asked = |event| match event {
        Event::Asked { request, .. } => request,
        other => panic!("expected a request, and got {other:?}"),
    };
    assert_eq!(asked(event(&mut events).await), Request::New);

    // What the daemon does for `new`: open a conversation, point the connection
    // at it and replay it. The replay is empty, and its end is where the chat
    // learns the name.
    server.watch(connection, "s-2");
    server.reply(
        connection,
        Envelope::from(
            "s-2",
            Frame::HistoryStart {
                id: "s-2".to_owned(),
            },
        ),
    );
    server.reply(
        connection,
        Envelope::from(
            "s-2",
            Frame::HistoryEnd {
                id: "s-2".to_owned(),
            },
        ),
    );

    assert_eq!(
        event(&mut events).await,
        Event::Asked {
            connection,
            session: Some("s-2".to_owned()),
            request: Request::Cancel,
        },
        "what follows `new` goes to the conversation `new` made"
    );

    // `/start` is answered by the bridge and never reaches the daemon.
    let said: Vec<String> = {
        let _ = api.nth("sendMessage", 1).await;
        api.calls("sendMessage").iter().map(text).collect()
    };
    assert!(
        said.iter().any(|message| message.contains("/new")),
        "the help names the commands: {said:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .is_err(),
        "help is the bridge's to give"
    );

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permission_question_is_asked_and_answered_with_a_button() {
    let temp = Temp::new("telegram-confirm");
    let (api, feed) = Fake::new();
    let (server, mut events, bridge) = bridge(&temp, allowed(), api.clone());
    feed.send(json!([message(1, MINE, "list the files")]))
        .expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("attached");
    };
    greet(&server, connection);
    event(&mut events).await;

    // A run is in flight, which is what makes the question this chat's.
    server.send(Envelope::from(SESSION, Frame::TurnStarted));

    // The real confirmer, and the real wait. It blocks the caller, exactly as
    // it does on a session's own task, so it goes on a thread of its own.
    let confirmer = server.confirmer();
    let asking = tokio::task::spawn_blocking(move || {
        confirmer.confirm("bash", "rm -rf ./build", PermissionRisk::Destructive)
    });

    let question = api.nth("sendMessage", 0).await;
    assert_eq!(question["chat_id"], MINE);
    assert!(text(&question).contains("bash"), "{}", text(&question));
    assert!(
        text(&question).contains("rm -rf ./build"),
        "{}",
        text(&question)
    );

    // The id is the confirmer's, and the buttons carry it back.
    let buttons = question["reply_markup"]["inline_keyboard"][0].clone();
    let allow_always = buttons[1]["callback_data"].as_str().expect("data");
    assert!(allow_always.starts_with("A:"), "{allow_always}");
    assert!(
        buttons[0]["callback_data"]
            .as_str()
            .expect("data")
            .starts_with("a:")
            && buttons[2]["callback_data"]
                .as_str()
                .expect("data")
                .starts_with("d:"),
        "three answers, in the order somebody reads them"
    );

    feed.send(json!([{
        "update_id": 2,
        "callback_query": {
            "id": "q-1",
            "data": allow_always,
            "message": { "message_id": 5, "chat": { "id": MINE } },
        }
    }]))
    .expect("feed");

    let decided = tokio::time::timeout(Duration::from_secs(5), asking)
        .await
        .expect("the tool is answered within five seconds")
        .expect("the confirmer did not panic");
    assert_eq!(decided, Decision::AllowAlways);

    // The spinner is cleared and the buttons are taken off, so an answered
    // question cannot be pressed again.
    assert_eq!(
        api.nth("answerCallbackQuery", 0).await["callback_query_id"],
        "q-1"
    );
    assert_eq!(api.nth("editMessageReplyMarkup", 0).await["message_id"], 5);

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_for_nobody_here_is_left_alone() {
    let temp = Temp::new("telegram-quiet");
    let (api, feed) = Fake::new();
    let (server, mut events, bridge) = bridge(&temp, allowed(), api.clone());
    feed.send(json!([message(1, MINE, "hello")])).expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("attached");
    };
    greet(&server, connection);
    event(&mut events).await;

    // No turn started here, so this belongs to a terminal or to a job.
    server.send(Envelope::daemon(Frame::Confirm {
        id: 3,
        tool: "bash".to_owned(),
        summary: "ls".to_owned(),
        risk: Risk::Read,
    }));

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        api.calls("sendMessage").is_empty(),
        "a chat with nothing running is not asked: {:?}",
        api.calls("sendMessage")
    );

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tool_call_is_a_line_when_it_is_wanted() {
    let temp = Temp::new("telegram-tools");
    let (api, feed) = Fake::new();
    let config = Telegram {
        tools: true,
        ..allowed()
    };
    let (server, mut events, bridge) = bridge(&temp, config, api.clone());
    feed.send(json!([message(1, MINE, "hello")])).expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("attached");
    };
    greet(&server, connection);
    event(&mut events).await;

    server.send(Envelope::from(SESSION, Frame::TurnStarted));
    server.send(Envelope::from(
        SESSION,
        Frame::ToolCall {
            id: "call-1".to_owned(),
            name: "bash".to_owned(),
            arguments: "{\"command\":\n \"ls -la\"}".to_owned(),
        },
    ));

    let line = text(&api.nth("sendMessage", 0).await);
    assert!(line.contains("bash"), "{line}");
    assert!(line.contains("ls -la"), "{line}");
    assert!(
        !line.contains('\n'),
        "one line, whatever the arguments: {line}"
    );

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failure_is_reported_one_time() {
    let temp = Temp::new("telegram-error");
    let (api, feed) = Fake::new();
    let (server, mut events, bridge) = bridge(&temp, allowed(), api.clone());
    feed.send(json!([message(1, MINE, "hello")])).expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("attached");
    };
    greet(&server, connection);
    event(&mut events).await;

    // A turn that fails ends the run as well, and both frames carry it.
    server.send(Envelope::from(SESSION, Frame::TurnStarted));
    server.send(Envelope::from(
        SESSION,
        Frame::TurnEnded {
            usage: Usage::default(),
            stop: StopReason::Error,
            error: Some("the provider said no".to_owned()),
        },
    ));
    server.send(Envelope::from(
        SESSION,
        Frame::RunEnded {
            stop: StopReason::Error,
            turns: 1,
            error: Some("the provider said no".to_owned()),
        },
    ));

    assert!(text(&api.nth("sendMessage", 0).await).contains("the provider said no"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        api.calls("sendMessage").len(),
        1,
        "the same failure is not said twice: {:?}",
        api.calls("sendMessage")
    );

    bridge.abort();
}
