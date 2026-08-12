//! One Telegram chat, and the gateway connection behind it.
//!
//! A chat is a client, in the plain sense of [`crate::gateway::client`]: it
//! attaches, the daemon opens a session for it, and it sees that session's
//! frames and nothing else. So two chats are two conversations without the
//! bridge keeping any map of who is talking to what — the gateway already
//! does that, and it is the same thing it does for two terminals.
//!
//! The task here is therefore only translation: frames in, messages out. It is
//! the Telegram counterpart of [`crate::tui::App::apply`], and it drops far more
//! than it shows. A phone is not a terminal: thinking, tool arguments and tool
//! results belong in `aphid alate attach`, not in a chat.

use std::collections::VecDeque;

use serde_json::{Value, json};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::{LIMIT, Shared, chunks};
use crate::gateway::Client;
use crate::gateway::wire::{Envelope, Frame, Request, Risk};

/// Attach a connection for `chat` and start serving it.
///
/// # Errors
///
/// Fails when the socket cannot be reached, which means the daemon has gone.
pub(super) async fn open(chat: i64, shared: Shared) -> std::io::Result<UnboundedSender<Request>> {
    // Named, so `/sessions` says which chat a conversation belongs to rather
    // than showing several rows that all say the same word.
    let client = Client::connect_as(&shared.socket, Some(&format!("telegram: {chat}"))).await?;
    tracing::info!(chat, "telegram: chat connected");
    let (sender, requests) = mpsc::unbounded_channel();
    tokio::spawn(serve(
        Chat {
            id: chat,
            shared,
            session: None,
            waiting: VecDeque::new(),
            reply: String::new(),
            running: false,
            told: None,
        },
        client,
        requests,
    ));
    Ok(sender)
}

/// What one chat holds between frames.
struct Chat {
    id: i64,
    shared: Shared,
    /// The conversation this chat is in, once the daemon has named it.
    session: Option<String>,
    /// What was said before the daemon named the conversation.
    ///
    /// Nothing can go down the socket until then. A request is stamped with
    /// whatever the connection watches **at the moment the line is read**, so a
    /// prompt sent in the same breath as the attach carries no session and is
    /// dropped by the daemon. Held here, it is asked once there is somewhere to
    /// ask it of.
    waiting: VecDeque<Request>,
    /// What the agent has said this turn. Sent whole when the turn ends.
    reply: String,
    /// Whether a run is in flight, which is what decides if a permission
    /// question is this chat's to answer.
    running: bool,
    /// The last failure the chat was told about.
    ///
    /// A turn that fails ends the run as well, and both frames carry the same
    /// sentence. Held so the chat is told one time.
    told: Option<String>,
}

/// One chat, until the daemon hangs up or the bridge stops.
async fn serve(mut chat: Chat, mut client: Client, mut requests: UnboundedReceiver<Request>) {
    loop {
        tokio::select! {
            request = requests.recv() => match request {
                None => break,
                Some(request) => chat.hold(request),
            },
            envelope = client.recv() => match envelope {
                Ok(Some(envelope)) => chat.apply(envelope).await,
                // The daemon closed, or the connection broke. Either way there
                // is nothing more to translate.
                Ok(None) | Err(_) => break,
            },
        }

        // Everything waiting, for as long as there is a conversation to put it
        // in. One `new` in the queue stops the rest here, which is the point:
        // it moves this connection somewhere the daemon has not named yet.
        if !chat.drain(&mut client).await {
            break;
        }
    }

    // Whatever was said before the daemon went is still worth sending.
    chat.flush().await;
}

impl Chat {
    /// Keep a request until there is a conversation to put it in.
    ///
    /// A daemon always greets, so this queue is short. The limit is there for
    /// the day one does not: a chat that is never greeted must not grow a queue
    /// for as long as the alate runs.
    fn hold(&mut self, request: Request) {
        const WAITING: usize = 32;

        if self.waiting.len() < WAITING {
            self.waiting.push_back(request);
        }
    }

    /// Ask everything that was waiting. `false` when the daemon has gone.
    ///
    /// Every request goes out through here, so it makes no difference whether
    /// somebody typed before the greeting or after it.
    async fn drain(&mut self, client: &mut Client) -> bool {
        while self.session.is_some() {
            let Some(request) = self.waiting.pop_front() else {
                return true;
            };
            let moving = request == Request::New;
            if client.send(&request).await.is_err() {
                return false;
            }
            // The daemon is about to point this connection at another
            // conversation. What follows waits for it to be named.
            if moving {
                self.session = None;
            }
        }
        true
    }

    /// Turn one frame into what the chat should see, if anything.
    async fn apply(&mut self, envelope: Envelope) {
        // The daemon's own frames carry no session. A conversation's frames
        // carry one, and only this chat's own are its business.
        let mine = envelope.session.is_some() && envelope.session == self.session;

        match envelope.frame {
            // The greeting names the session opened for this connection, and
            // is therefore when anything held back can be asked.
            Frame::Hello { .. } => self.session = envelope.session,

            // A replay is starting, so this connection has been pointed at
            // another conversation. What was buffered belongs to the last one.
            Frame::HistoryStart { .. } => {
                self.session = None;
                self.reply.clear();
                self.running = false;
            }
            // The replay itself is dropped — a chat has the old messages
            // already — and its end is where the new session gets its name.
            Frame::HistoryEnd { id } => {
                self.session = Some(id);
                self.say("a new conversation").await;
            }

            Frame::TurnStarted if mine => {
                self.running = true;
                self.told = None;
                self.typing().await;
            }
            // Held, not sent. Telegram takes about one message a second for a
            // chat, and a message for each delta would be throttled within a
            // sentence. One message a turn also reads better on a phone.
            Frame::Text { text } if mine => self.reply.push_str(&text),

            Frame::ToolCall {
                name, arguments, ..
            } if mine && self.shared.tools => {
                // Before the line, so what the agent said stays ahead of what
                // it then did.
                self.flush().await;
                self.say(&tool_line(&name, &arguments)).await;
            }

            Frame::TurnEnded { error, .. } if mine => {
                self.flush().await;
                self.failed(error).await;
            }
            Frame::RunEnded { error, .. } if mine => {
                self.running = false;
                self.flush().await;
                self.failed(error).await;
            }

            // Only this session's. A notice from the daemon reaches every
            // client, and forwarding those would put every start-up problem in
            // every chat.
            Frame::Notice { text } if mine => self.say(&format!("note: {text}")).await,

            // A permission question is the daemon's and goes to everybody, so
            // the run in flight is what says whose it is. With nothing running
            // here, a terminal or the timeout answers it instead.
            Frame::Confirm {
                id,
                tool,
                summary,
                risk,
            } if self.running => self.ask(id, &tool, &summary, risk).await,

            _ => {}
        }
    }

    /// Say what went wrong, unless it has been said already.
    ///
    /// A failed turn ends the run too, so the same sentence arrives twice. It
    /// is still worth taking from both: a run that fails before any turn ends
    /// has only the second one to say it.
    async fn failed(&mut self, error: Option<String>) {
        let Some(error) = error else {
            return;
        };
        if self.told.as_ref() == Some(&error) {
            return;
        }
        tracing::error!(chat = self.id, %error, "telegram: turn failed");
        self.say(&format!("error: {error}")).await;
        self.told = Some(error);
    }

    /// Send what the agent has said, in as many messages as it takes.
    async fn flush(&mut self) {
        let reply = std::mem::take(&mut self.reply);
        let reply = reply.trim();
        if reply.is_empty() {
            return;
        }
        for chunk in chunks(reply, LIMIT) {
            self.say(chunk).await;
        }
    }

    /// One message, as plain text.
    async fn say(&self, text: &str) {
        self.send(json!({ "chat_id": self.id, "text": text })).await;
    }

    /// The `typing…` a chat shows while it waits.
    async fn typing(&self) {
        self.call(
            "sendChatAction",
            json!({ "chat_id": self.id, "action": "typing" }),
        )
        .await;
    }

    /// Put a permission question to the chat, with its answers as buttons.
    async fn ask(&self, id: u64, tool: &str, summary: &str, risk: Risk) {
        let text = format!("{tool} wants to run ({}):\n{summary}", word(risk));
        self.send(json!({
            "chat_id": self.id,
            "text": text,
            "reply_markup": {
                "inline_keyboard": [[
                    { "text": "Allow", "callback_data": format!("a:{id}") },
                    { "text": "Allow always", "callback_data": format!("A:{id}") },
                    { "text": "Deny", "callback_data": format!("d:{id}") },
                ]]
            }
        }))
        .await;
    }

    /// `sendMessage`, without a `parse_mode`.
    ///
    /// Deliberately plain: model output is full of `*`, `_` and backticks that
    /// are not balanced markdown, and Telegram refuses the whole message over
    /// one of them. A reply that arrives unstyled beats a reply that does not
    /// arrive.
    async fn send(&self, body: Value) {
        self.call("sendMessage", body).await;
    }

    /// Make a call, and say nothing when it fails.
    ///
    /// There is nowhere to report to: the chat is what could not be reached.
    /// The poll loop notices the same outage on its own request and reports it
    /// once, which is the right number of times.
    async fn call(&self, method: &'static str, body: Value) {
        let _ = self.shared.api.call(method, body).await;
    }
}

/// A word for how much damage a tool could do.
fn word(risk: Risk) -> &'static str {
    match risk {
        Risk::Read => "reads",
        Risk::Mutate => "changes things",
        Risk::Destructive => "destructive",
    }
}

/// The one line a tool call gets when `tools` is on.
///
/// The arguments as they came, on one line and cut short. Reading them properly
/// is what `aphid alate attach` is for; this only has to say what is happening.
fn tool_line(name: &str, arguments: &str) -> String {
    const ROOM: usize = 160;

    let mut flat = String::with_capacity(arguments.len().min(ROOM + 1));
    let mut spaced = false;
    for character in arguments.chars() {
        if character.is_whitespace() {
            spaced = true;
            continue;
        }
        if spaced && !flat.is_empty() {
            flat.push(' ');
        }
        spaced = false;
        if flat.chars().count() >= ROOM {
            flat.push('…');
            break;
        }
        flat.push(character);
    }

    if flat.is_empty() {
        format!("· {name}")
    } else {
        format!("· {name} {flat}")
    }
}
