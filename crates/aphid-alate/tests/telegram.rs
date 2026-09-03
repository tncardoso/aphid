//! The Telegram bridge: what a chat says becomes a request, and what the
//! session says becomes a message.
//!
//! The daemon is not needed for any of this. The bridge is a gateway client, so
//! a bare [`Server`] and two lines playing the daemon's part — open a session
//! for a connection and greet it — is the whole of the far side.

#![cfg(feature = "telegram")]

mod common;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_alate::config::Telegram;
use aphid_alate::gateway::wire::{Envelope, Frame, Request, Risk};
use aphid_alate::gateway::{Event, Server};
use aphid_alate::telegram::{self, Api, Bridge, Call, Fetch};
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
    /// The next message id to give out, like a real chat would.
    messages: AtomicI64,
    /// What a download hands back, and the paths that were asked for.
    file: Mutex<Result<Vec<u8>, String>>,
    fetched: Mutex<Vec<String>>,
}

impl Fake {
    fn new() -> (Arc<Self>, UnboundedSender<Value>) {
        let (feed, updates) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(Self {
                updates: tokio::sync::Mutex::new(updates),
                calls: Mutex::new(Vec::new()),
                messages: AtomicI64::new(1),
                file: Mutex::new(Ok(b"pretend this is Opus".to_vec())),
                fetched: Mutex::new(Vec::new()),
            }),
            feed,
        )
    }

    /// Every file path that was asked for, in order.
    fn fetched(&self) -> Vec<String> {
        self.fetched.lock().expect("lock").clone()
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
            .push((method.to_owned(), body.clone()));
        Box::pin(async move {
            // Where a file is, in the shape Telegram answers it: a path under
            // the file root, which the download then asks for.
            if method == "getFile" {
                let id = body["file_id"].as_str().unwrap_or("unknown").to_owned();
                return Ok(json!({ "file_path": format!("voice/{id}.oga"), "file_size": 8_192 }));
            }
            if method != "getUpdates" {
                let message_id = self.messages.fetch_add(1, Ordering::Relaxed);
                return Ok(json!({ "message_id": message_id }));
            }
            // Nothing until the test says so, which is what a long poll with a
            // quiet chat behind it does.
            match self.updates.lock().await.recv().await {
                Some(updates) => Ok(updates),
                None => Ok(json!([])),
            }
        })
    }

    fn fetch(&self, path: &str) -> Fetch<'_> {
        self.fetched.lock().expect("lock").push(path.to_owned());
        let answer = self.file.lock().expect("lock").clone();
        Box::pin(async move { answer })
    }
}

/// A transcriber that says what a test told it to, and remembers it was asked.
#[cfg(feature = "voice")]
struct Heard {
    text: Result<String, String>,
    asked: Mutex<u32>,
}

#[cfg(feature = "voice")]
impl Heard {
    fn saying(text: &str) -> Arc<Self> {
        Arc::new(Self {
            text: Ok(text.to_owned()),
            asked: Mutex::new(0),
        })
    }

    fn failing(why: &str) -> Arc<Self> {
        Arc::new(Self {
            text: Err(why.to_owned()),
            asked: Mutex::new(0),
        })
    }

    fn asked(&self) -> u32 {
        *self.asked.lock().expect("lock")
    }
}

#[cfg(feature = "voice")]
impl aphid_alate::voice::Transcribe for Heard {
    fn transcribe(&self, _audio: Vec<u8>) -> aphid_alate::voice::Transcription<'_> {
        *self.asked.lock().expect("lock") += 1;
        let answer = self.text.clone();
        Box::pin(async move { answer })
    }
}

fn message(id: i64, chat: i64, text: &str) -> Value {
    json!({ "update_id": id, "message": { "chat": { "id": chat }, "text": text } })
}

/// A voice message, which is what the microphone button sends.
fn recording(id: i64, chat: i64, file: &str) -> Value {
    json!({
        "update_id": id,
        "message": {
            "chat": { "id": chat },
            "voice": { "file_id": file, "duration": 3, "mime_type": "audio/ogg" },
        },
    })
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
    listening(
        temp,
        config,
        api,
        #[cfg(feature = "voice")]
        None,
    )
}

/// The same, with a transcriber on it.
fn listening(
    temp: &Temp,
    config: Telegram,
    api: Arc<Fake>,
    #[cfg(feature = "voice")] voice: Option<aphid_alate::voice::TranscribeFn>,
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
        #[cfg(feature = "voice")]
        voice,
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
async fn a_tool_call_is_an_announcement_when_it_is_wanted() {
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

    let announcement = text(&api.nth("sendMessage", 0).await);
    assert!(announcement.contains("Tool Call: bash"), "{announcement}");
    assert!(announcement.contains("ls -la"), "{announcement}");
    assert!(
        !announcement.contains("(x"),
        "the first call carries no count: {announcement}"
    );
    assert_eq!(api.calls("editMessageText").len(), 0);

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn more_calls_edit_the_same_announcement() {
    let temp = Temp::new("telegram-tools-edit");
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
            arguments: "{\"command\":\"ls -la\"}".to_owned(),
        },
    ));
    server.send(Envelope::from(
        SESSION,
        Frame::ToolCall {
            id: "call-2".to_owned(),
            name: "rg".to_owned(),
            arguments: "{\"pattern\":\"fn main\"}".to_owned(),
        },
    ));

    // One message was sent; the second call edited it instead of sending.
    let edited = api.nth("editMessageText", 0).await;
    assert_eq!(api.calls("sendMessage").len(), 1);
    assert_eq!(edited["chat_id"], MINE);
    let text = edited["text"].as_str().expect("a text");
    assert!(text.contains("Tool Call: rg"), "{text}");
    assert!(text.contains("(x2)"), "{text}");
    assert!(
        text.contains("rg {\"pattern\":\"fn main\"}"),
        "the last call, whole: {text}"
    );
    assert!(
        !text.contains("ls -la"),
        "the earlier call is gone, not accumulated: {text}"
    );

    bridge.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_turn_opens_a_new_announcement() {
    let temp = Temp::new("telegram-tools-turn");
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

    let call = |id: &str, name: &str| Frame::ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: "{}".to_owned(),
    };

    server.send(Envelope::from(SESSION, Frame::TurnStarted));
    server.send(Envelope::from(SESSION, call("call-1", "bash")));
    server.send(Envelope::from(
        SESSION,
        Frame::TurnEnded {
            usage: Usage::default(),
            stop: StopReason::Stop,
            error: None,
        },
    ));
    server.send(Envelope::from(SESSION, Frame::TurnStarted));
    server.send(Envelope::from(SESSION, call("call-2", "rg")));

    // Each turn's first call sends a fresh message; nothing is edited. The
    // second send is what proves both turns ran before the counts are read.
    let _ = api.nth("sendMessage", 1).await;
    assert_eq!(api.calls("sendMessage").len(), 2);
    assert_eq!(api.calls("editMessageText").len(), 0);

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

/// A voice message is fetched, read, echoed, and asked as a prompt.
#[cfg(feature = "voice")]
#[tokio::test]
async fn a_recording_becomes_an_echo_and_a_prompt() {
    let temp = Temp::new("telegram-recording");
    let (api, feed) = Fake::new();
    let heard = Heard::saying("boa tarde");
    let (server, mut events, bridge) =
        listening(&temp, allowed(), api.clone(), Some(heard.clone()));
    feed.send(json!([recording(1, MINE, "file-1")]))
        .expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("the first event is the chat attaching");
    };
    greet(&server, connection);

    // The words the agent is given are the words that were said, and they go
    // down the socket as an ordinary prompt: the wire knows nothing of audio.
    assert_eq!(
        event(&mut events).await,
        Event::Asked {
            connection,
            session: Some(SESSION.to_owned()),
            request: Request::Prompt {
                text: "boa tarde".to_owned()
            },
        }
    );

    // Telegram was asked where the file is, and then for the file itself.
    let asked = api.nth("getFile", 0).await;
    assert_eq!(asked["file_id"], json!("file-1"));
    assert_eq!(api.fetched(), vec!["voice/file-1.oga".to_owned()]);
    assert_eq!(heard.asked(), 1);

    // And the chat saw what was heard before it saw any answer, because
    // recognition is wrong often enough that it has to be visible.
    let echoed = api.calls("sendMessage");
    assert!(
        echoed.iter().any(|body| text(body) == "🎤 boa tarde"),
        "{echoed:?}"
    );

    bridge.abort();
}

/// The poll loop keeps serving while a recording is being read.
#[cfg(feature = "voice")]
#[tokio::test]
async fn a_recording_does_not_stop_the_other_chats() {
    let temp = Temp::new("telegram-not-blocked");
    let (api, feed) = Fake::new();
    // A transcriber that never answers, which is the worst a slow one can be.
    struct Never;
    impl aphid_alate::voice::Transcribe for Never {
        fn transcribe(&self, _audio: Vec<u8>) -> aphid_alate::voice::Transcription<'_> {
            Box::pin(std::future::pending())
        }
    }
    let (server, mut events, bridge) =
        listening(&temp, allowed(), api.clone(), Some(Arc::new(Never)));

    feed.send(json!([recording(1, MINE, "file-1")]))
        .expect("feed");
    let Event::Opened { connection } = event(&mut events).await else {
        panic!("the first event is the chat attaching");
    };
    greet(&server, connection);

    // The update after it is served, though the one before it never finishes.
    feed.send(json!([message(2, MINE, "hello")])).expect("feed");
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

/// A recording with no speech in it does not cost the agent a turn.
#[cfg(feature = "voice")]
#[tokio::test]
async fn a_silent_recording_is_said_to_be_silent() {
    let temp = Temp::new("telegram-silence");
    let (api, feed) = Fake::new();
    let heard = Heard::saying("   ");
    let (server, mut events, bridge) = listening(&temp, allowed(), api.clone(), Some(heard));
    feed.send(json!([recording(1, MINE, "file-1")]))
        .expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("the first event is the chat attaching");
    };
    greet(&server, connection);

    let said = text(&api.nth("sendMessage", 0).await);
    assert!(said.contains("could not make out"), "{said}");

    // Nothing was asked of the agent, and the chat is still attached.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .is_err(),
        "a silent recording must not become a prompt"
    );

    bridge.abort();
}

/// A transcriber that fails says so in the chat and the bridge carries on.
#[cfg(feature = "voice")]
#[tokio::test]
async fn a_recording_that_cannot_be_read_is_a_sentence() {
    let temp = Temp::new("telegram-unreadable");
    let (api, feed) = Fake::new();
    let heard = Heard::failing("the file is not audio this build can read");
    let (server, mut events, bridge) = listening(&temp, allowed(), api.clone(), Some(heard));
    feed.send(json!([recording(1, MINE, "file-1")]))
        .expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("the first event is the chat attaching");
    };
    greet(&server, connection);

    let said = text(&api.nth("sendMessage", 0).await);
    assert!(said.contains("not audio"), "{said}");

    // And the next message is still served.
    feed.send(json!([message(2, MINE, "hello")])).expect("feed");
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

/// A download that fails is a sentence too, and the file is never read.
#[cfg(feature = "voice")]
#[tokio::test]
async fn a_file_that_cannot_be_fetched_is_a_sentence() {
    let temp = Temp::new("telegram-unfetchable");
    let (api, feed) = Fake::new();
    *api.file.lock().expect("lock") = Err("the file stopped coming".to_owned());
    let heard = Heard::saying("never said");
    let (server, mut events, bridge) =
        listening(&temp, allowed(), api.clone(), Some(heard.clone()));
    feed.send(json!([recording(1, MINE, "file-1")]))
        .expect("feed");

    let Event::Opened { connection } = event(&mut events).await else {
        panic!("the first event is the chat attaching");
    };
    greet(&server, connection);

    let said = text(&api.nth("sendMessage", 0).await);
    assert!(said.contains("stopped coming"), "{said}");
    assert_eq!(heard.asked(), 0, "nothing to read, so nothing was read");

    bridge.abort();
}

/// An alate with no transcriber says so, and says it one time.
#[tokio::test]
async fn a_recording_with_nothing_to_read_it_is_refused_once() {
    let temp = Temp::new("telegram-deaf");
    let (api, feed) = Fake::new();
    let (_server, _events, bridge) = bridge(&temp, allowed(), api.clone());

    feed.send(json!([recording(1, MINE, "file-1")]))
        .expect("feed");
    let said = text(&api.nth("sendMessage", 0).await);
    assert!(said.contains("does not listen"), "{said}");
    assert!(said.contains("voice"), "{said}");

    // A second recording says nothing more: a chat that only sends audio must
    // not get the same sentence for every one of them.
    feed.send(json!([recording(2, MINE, "file-2")]))
        .expect("feed");
    feed.send(json!([message(3, MINE, "hello")])).expect("feed");
    // The text message opens a connection, which is how this waits for the
    // second recording to have been handled and passed over.
    tokio::time::timeout(Duration::from_secs(5), async {
        while api.calls("sendMessage").len() > 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no second refusal within five seconds");
    assert_eq!(api.calls("sendMessage").len(), 1);
    // And nothing was fetched, because there was nothing to read it with.
    assert!(api.fetched().is_empty());

    bridge.abort();
}

/// A file that is not audio is left alone, exactly as a photo is.
#[tokio::test]
async fn a_file_that_is_not_audio_is_ignored() {
    let temp = Temp::new("telegram-not-audio");
    let (api, feed) = Fake::new();
    let (_server, _events, bridge) = bridge(&temp, allowed(), api.clone());

    feed.send(json!([{
        "update_id": 1,
        "message": {
            "chat": { "id": MINE },
            "document": { "file_id": "z", "mime_type": "application/zip" },
        },
    }]))
    .expect("feed");
    feed.send(json!([message(2, MINE, "hello")])).expect("feed");

    // The text after it is served, and the archive drew no sentence at all.
    api.nth("getUpdates", 2).await;
    assert!(
        api.calls("sendMessage").is_empty(),
        "{:?}",
        api.calls("sendMessage")
    );

    bridge.abort();
}
