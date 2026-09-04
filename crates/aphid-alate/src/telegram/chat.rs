//! One Telegram chat, and the gateway connection behind it.
//!
//! A chat is a client, in the plain sense of [`crate::gateway::client`]: it
//! attaches, the daemon opens a session for it, and it sees that session's
//! frames and nothing else. So two chats are two conversations without the
//! bridge keeping any map of who is talking to what — the gateway already
//! does that, and it is the same thing it does for two terminals.
//!
//! The task here is therefore only translation: frames in, messages out. It is
//! the Telegram counterpart of [`crate::tui::App::update`], and it drops far more
//! than it shows. A phone is not a terminal: thinking and tool results belong in
//! `aphid alate attach`, not in a chat. A tool call gets one short announcement
//! per turn, edited as further calls of the same turn arrive.

use std::collections::VecDeque;

use base64::Engine;
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
    let client = Client::connect_as_with_attachments(
        &shared.socket,
        Some(&format!("telegram: {chat}")),
        true,
    )
    .await?;
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
            tool: None,
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
    /// The tool-call announcement being edited, when one is open.
    tool: Option<ToolNote>,
}

/// The one announcement a turn's tool calls share.
///
/// The first call of a turn sends it; each call after that edits it in place,
/// so a turn of five calls reads as one message that ends with the fifth.
struct ToolNote {
    /// The message being edited, as Telegram numbered it.
    message_id: i64,
    /// How many calls this announcement has covered.
    total: u32,
    /// The last call's name and arguments, which are what the message shows.
    name: String,
    arguments: String,
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
                self.tool = None;
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
                self.tool = None;
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
                self.tool_call(&name, &arguments).await;
            }

            Frame::Attachment {
                id,
                name,
                data,
                caption,
            } if mine => {
                let result = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|error| format!("invalid attachment data: {error}"));
                let result = match result {
                    Ok(data) => self
                        .shared
                        .api
                        .document(self.id, name, data, caption)
                        .await
                        .map(|_| ()),
                    Err(error) => Err(error),
                };
                self.hold(Request::AttachmentResult {
                    id,
                    error: result.err(),
                });
            }

            Frame::TurnEnded { error, .. } if mine => {
                self.tool = None;
                self.flush().await;
                self.failed(error).await;
            }
            Frame::RunEnded { error, .. } if mine => {
                self.running = false;
                self.tool = None;
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

    /// Announce a tool call: send the first, edit the rest.
    ///
    /// One message a turn, whatever the count: the second call edits the first
    /// message to say `(x2)` and show the second call, and so on. A message
    /// full past [`LIMIT`] stops being edited and a fresh one takes over.
    async fn tool_call(&mut self, name: &str, arguments: &str) {
        let Some(note) = self.tool.take() else {
            self.open_tool(name, arguments).await;
            return;
        };

        // Another call: count it and put the new call in place of the old.
        let next = ToolNote {
            message_id: note.message_id,
            total: note.total + 1,
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        };
        let text = tool_block(&next.name, &next.arguments, next.total);
        if text.len() <= LIMIT {
            self.call(
                "editMessageText",
                json!({
                    "chat_id": self.id,
                    "message_id": next.message_id,
                    "text": text,
                }),
            )
            .await;
            self.tool = Some(next);
        } else {
            // Full: close it, and let a fresh message take over.
            self.open_tool(name, arguments).await;
        }
    }

    /// Open the turn's tool announcement with its first call.
    async fn open_tool(&mut self, name: &str, arguments: &str) {
        let text = tool_block(name, arguments, 1);
        let message_id = self.send_message(&text).await;
        if let Some(message_id) = message_id {
            self.tool = Some(ToolNote {
                message_id,
                total: 1,
                name: name.to_owned(),
                arguments: arguments.to_owned(),
            });
        }
    }

    /// One message, as plain text.
    async fn say(&self, text: &str) {
        self.send(json!({ "chat_id": self.id, "text": text })).await;
    }

    /// Send one message, and say what Telegram numbered it.
    async fn send_message(&self, text: &str) -> Option<i64> {
        let sent = self
            .shared
            .api
            .call("sendMessage", json!({ "chat_id": self.id, "text": text }))
            .await
            .ok()?;
        sent.get("message_id").and_then(Value::as_i64)
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

/// The two lines a tool call gets when `tools` is on.
///
/// The count is only there from the second call on: the first reads as a plain
/// announcement, and each edit makes it say how many calls the turn has made
/// so far and show the latest one. The arguments are flattened onto one line
/// but never cut short — reading them is what `aphid alate attach` is for, but
/// the chat still gets to see them whole.
fn tool_block(name: &str, arguments: &str, total: u32) -> String {
    let count = if total > 1 {
        format!(" (x{total})")
    } else {
        String::new()
    };
    let flat = flatten(arguments);
    let second = if flat.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {flat}")
    };
    format!("🛠️ Tool Call: {name}{count}\n{second}")
}

/// One line, whitespace run together, nothing cut.
fn flatten(text: &str) -> String {
    let mut flat = String::with_capacity(text.len());
    let mut spaced = false;
    for character in text.chars() {
        if character.is_whitespace() {
            spaced = true;
            continue;
        }
        if spaced && !flat.is_empty() {
            flat.push(' ');
        }
        spaced = false;
        flat.push(character);
    }
    flat
}
