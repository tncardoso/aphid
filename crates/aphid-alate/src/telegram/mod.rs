//! A Telegram bot on the gateway.
//!
//! The bridge is a **client**, not a second door. It attaches to the same
//! socket a terminal does, one connection for each chat, and speaks the same
//! [`wire`] to it. So nothing in the daemon knows Telegram exists: a chat gets
//! an ordinary [`Kind::Attached`] session, two chats are kept apart by the fan
//! out that already keeps two terminals apart, and a permission answer is the
//! [`Request::Answer`] that was always there.
//!
//! ```text
//! Telegram  ──getUpdates──►  poll ──┬─► chat ──Client──► gateway.sock ──► daemon
//!           ◄─sendMessage──         └─◄ frames ─────────────────────────
//! ```
//!
//! A chat connects when it first says something, and not before. An alate
//! nobody has messaged therefore still has nobody attached, and still refuses a
//! tool that asks permission with nobody there — which is the behaviour that
//! keeps an unattended alate from talking itself into anything overnight.
//!
//! # Who may talk
//!
//! Whoever can reach the bot can make the agent run commands, so
//! `gateway.telegram.chats` is an allow list and an empty one allows nobody. A
//! chat that is refused is told its own id once, which is how you find the
//! number to put in the file.
//!
//! [`wire`]: crate::gateway::wire
//! [`Kind::Attached`]: crate::sessions::Kind::Attached

pub mod api;
mod chat;
mod voice;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

pub use api::{API, Api, ApiFn, Call, Fetch, Live};

use crate::config::Telegram;
use crate::gateway::Publisher;
use crate::gateway::wire::{Answer, Frame, Request};

/// The longest message Telegram takes, in characters.
const LIMIT: usize = 4096;

/// How long to wait after the first failed call, and the longest to wait after
/// any of them. A bot with no network must not ask as fast as it can fail.
const BACKOFF: Duration = Duration::from_secs(5);
const LONGEST: Duration = Duration::from_secs(60);

/// What `/start` answers with.
const HELP: &str = "Say anything and the agent answers.\n\
                    /new starts a new conversation.\n\
                    /cancel stops the run in flight.";

/// What to start a bridge with.
pub struct Bridge {
    /// The gateway each chat attaches to.
    pub socket: PathBuf,
    pub config: Telegram,
    pub api: ApiFn,
    /// Where a problem with Telegram is reported: the terminals, and
    /// `alate.log`. A chat cannot be told that Telegram is unreachable.
    pub notices: Publisher,
    /// What turns a recording into words. Absent means this alate does not
    /// listen, and a chat that sends audio is told so.
    #[cfg(feature = "voice")]
    pub voice: Option<crate::voice::TranscribeFn>,
}

/// What a chat task is given, and what the poll loop keeps a copy of.
#[derive(Clone)]
struct Shared {
    api: ApiFn,
    socket: PathBuf,
    tools: bool,
    #[cfg(feature = "voice")]
    voice: Option<crate::voice::TranscribeFn>,
}

/// Start the bridge. It runs until the task is dropped or aborted.
#[must_use]
pub fn spawn(bridge: Bridge) -> JoinHandle<()> {
    tokio::spawn(run(bridge))
}

/// The poll loop: one `getUpdates` after another, for ever.
async fn run(bridge: Bridge) {
    let Bridge {
        socket,
        config,
        api,
        notices,
        #[cfg(feature = "voice")]
        voice,
    } = bridge;

    let seconds = match config.interval() {
        Ok(poll) => poll.as_secs(),
        Err(why) => {
            tracing::error!(%why, "telegram: bad poll interval");
            notices.send(Frame::Notice {
                text: format!("telegram: {why}"),
            });
            return;
        }
    };

    let shared = Shared {
        api: api.clone(),
        socket,
        tools: config.tools,
        #[cfg(feature = "voice")]
        voice,
    };

    let mut state = State::default();
    let mut failures: u32 = 0;

    loop {
        let asked = api
            .call(
                "getUpdates",
                json!({
                    "offset": state.offset,
                    "timeout": seconds,
                    "allowed_updates": ["message", "callback_query"],
                }),
            )
            .await;

        let updates = match asked {
            Ok(updates) => {
                if failures > 0 {
                    tracing::info!("telegram: reachable again");
                    notices.send(Frame::Notice {
                        text: "telegram: reachable again".to_owned(),
                    });
                    failures = 0;
                }
                updates
            }
            Err(why) => {
                // The first failure of a streak, and no more. A bot with no
                // network would otherwise write one line for every wait, and
                // bury whatever else the log had to say.
                if failures == 0 {
                    tracing::warn!(%why, "telegram: getUpdates failed");
                    notices.send(Frame::Notice {
                        text: format!("telegram: {why}"),
                    });
                }
                failures = failures.saturating_add(1);
                tokio::time::sleep(backoff(failures)).await;
                continue;
            }
        };

        let Some(updates) = updates.as_array() else {
            continue;
        };
        for update in updates {
            // Moved past first, whatever happens next: an update that cannot be
            // handled must not be delivered again for ever.
            if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
                state.offset = state.offset.max(id + 1);
            }
            handle(update, &mut state, &config, &shared, &notices).await;
        }
    }
}

/// What the poll loop remembers between calls.
#[derive(Default)]
struct State {
    /// The next update wanted. Telegram forgets everything below it.
    offset: i64,
    /// A connection for each chat that has talked.
    chats: HashMap<i64, UnboundedSender<Request>>,
    /// The chats already told they are not allowed. Told once, so a stranger
    /// cannot make the bot answer them for ever.
    refused: HashSet<i64>,
    /// The chats already told this alate does not listen. Told once, for the
    /// same reason: a chat that only sends recordings must not get the same
    /// sentence for every one of them.
    deafened: HashSet<i64>,
}

/// One update.
async fn handle(
    update: &Value,
    state: &mut State,
    config: &Telegram,
    shared: &Shared,
    notices: &Publisher,
) {
    if let Some(message) = update.get("message") {
        let Some(chat) = message.pointer("/chat/id").and_then(Value::as_i64) else {
            return;
        };
        let text = message
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let recording = voice::recording(message);

        // A photo, a sticker, somebody joining: nothing an agent can read.
        if text.is_empty() && recording.is_none() {
            return;
        }

        if !config.chats.contains(&chat) {
            refuse(chat, state, shared).await;
            return;
        }

        if let Some(file) = recording {
            heard(chat, file, state, shared, notices).await;
            return;
        }

        match command(text) {
            Command::Help => say(shared, chat, HELP).await,
            Command::New => {
                tracing::info!(chat, "telegram: /new");
                to(chat, Request::New, state, shared, notices).await;
            }
            Command::Cancel => {
                tracing::info!(chat, "telegram: /cancel");
                to(chat, Request::Cancel, state, shared, notices).await;
            }
            Command::Say(text) => {
                tracing::info!(chat, "telegram: message");
                to(chat, Request::Prompt { text }, state, shared, notices).await;
            }
        }
        return;
    }

    if let Some(query) = update.get("callback_query") {
        answered(query, state, config, shared, notices).await;
    }
}

/// A button was pressed on a permission question.
async fn answered(
    query: &Value,
    state: &mut State,
    config: &Telegram,
    shared: &Shared,
    notices: &Publisher,
) {
    let chat = query.pointer("/message/chat/id").and_then(Value::as_i64);
    let (Some(chat), Some(data)) = (chat, query.get("data").and_then(Value::as_str)) else {
        return;
    };
    if !config.chats.contains(&chat) {
        return;
    }
    let Some((id, decision)) = decision(data) else {
        return;
    };

    // The spinner on the button stays until this is answered, whatever else
    // happens.
    if let Some(pressed) = query.get("id").and_then(Value::as_str) {
        let _ = shared
            .api
            .call(
                "answerCallbackQuery",
                json!({ "callback_query_id": pressed, "text": told(decision) }),
            )
            .await;
    }

    // Take the buttons off, so an old question cannot be answered twice. The
    // second answer would be dropped by the daemon anyway — the first one for
    // an id wins — but a button that does nothing is worse than no button.
    if let Some(message) = query.pointer("/message/message_id").and_then(Value::as_i64) {
        let _ = shared
            .api
            .call(
                "editMessageReplyMarkup",
                json!({
                    "chat_id": chat,
                    "message_id": message,
                    "reply_markup": { "inline_keyboard": [] },
                }),
            )
            .await;
    }

    to(
        chat,
        Request::Answer { id, decision },
        state,
        shared,
        notices,
    )
    .await;
}

/// Send a request to a chat's connection, opening one if it has none.
async fn to(chat: i64, request: Request, state: &mut State, shared: &Shared, notices: &Publisher) {
    if let Some(sender) = connection(chat, state, shared, notices).await {
        let _ = sender.send(request);
    }
}

/// A chat's connection, opening one if it has none.
///
/// Split out of [`to`] because a recording is read by a task of its own, which
/// needs the sender before it starts and cannot reach [`State`] once it has.
async fn connection(
    chat: i64,
    state: &mut State,
    shared: &Shared,
    notices: &Publisher,
) -> Option<UnboundedSender<Request>> {
    // A connection whose task has ended leaves a closed sender behind. Attach
    // again rather than dropping what was said into it.
    if state
        .chats
        .get(&chat)
        .is_some_and(UnboundedSender::is_closed)
    {
        state.chats.remove(&chat);
    }

    match state.chats.get(&chat) {
        Some(sender) => Some(sender.clone()),
        None => match chat::open(chat, shared.clone()).await {
            Ok(sender) => {
                state.chats.insert(chat, sender.clone());
                Some(sender)
            }
            Err(error) => {
                tracing::error!(chat, %error, "telegram: chat could not attach");
                notices.send(Frame::Notice {
                    text: format!("telegram: chat {chat} could not attach: {error}"),
                });
                say(shared, chat, "the agent cannot be reached").await;
                None
            }
        },
    }
}

/// A chat sent a recording.
///
/// The work is a task of its own and not a step of the poll loop: fetching and
/// reading take seconds, and the loop is what serves every other chat. The
/// price is that order within one chat is no longer promised.
async fn heard(chat: i64, file: String, state: &mut State, shared: &Shared, notices: &Publisher) {
    if !listens(shared) {
        deafen(chat, state, shared).await;
        return;
    }

    let Some(sender) = connection(chat, state, shared, notices).await else {
        return;
    };
    tracing::info!(chat, "telegram: recording");

    #[cfg(feature = "voice")]
    tokio::spawn(voice::read(chat, file, shared.clone(), sender));
    #[cfg(not(feature = "voice"))]
    let _ = (file, sender);
}

/// Whether this alate has anything to read a recording with.
fn listens(shared: &Shared) -> bool {
    #[cfg(feature = "voice")]
    {
        shared.voice.is_some()
    }
    #[cfg(not(feature = "voice"))]
    {
        let _ = shared;
        false
    }
}

/// Tell a chat this alate does not listen, once, and name the block that would
/// make it.
async fn deafen(chat: i64, state: &mut State, shared: &Shared) {
    if !state.deafened.insert(chat) {
        return;
    }
    tracing::warn!(chat, "telegram: recording, and no transcriber");
    say(
        shared,
        chat,
        "This alate does not listen to recordings.\n\
         To make it listen, put a \"voice\" block in alate.json, \
         and start the alate again from a build that has the voice feature.",
    )
    .await;
}

/// Tell a chat it is not on the list, once, and name the id to add.
async fn refuse(chat: i64, state: &mut State, shared: &Shared) {
    if !state.refused.insert(chat) {
        return;
    }
    tracing::warn!(chat, "telegram: chat not on allow-list, refused");
    say(
        shared,
        chat,
        &format!(
            "This chat is not allowed to use this agent.\n\
             To allow it, add {chat} to gateway.telegram.chats in alate.json, \
             and start the alate again."
        ),
    )
    .await;
}

/// One message to a chat, outside any conversation.
async fn say(shared: &Shared, chat: i64, text: &str) {
    let _ = shared
        .api
        .call("sendMessage", json!({ "chat_id": chat, "text": text }))
        .await;
}

/// What somebody typed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Command {
    Help,
    New,
    Cancel,
    Say(String),
}

/// Read the first word as a command, and everything else as words for the
/// agent.
///
/// The `@name` a group chat puts on a command is taken off first: in a group
/// Telegram sends `/new@my_bot`, and only the bare word is worth matching.
fn command(text: &str) -> Command {
    let first = text.split_whitespace().next().unwrap_or_default();
    let bare = first.split('@').next().unwrap_or(first);
    match bare {
        "/start" | "/help" => Command::Help,
        "/new" => Command::New,
        "/cancel" | "/stop" => Command::Cancel,
        _ => Command::Say(text.to_owned()),
    }
}

/// Read the `callback_data` a button carried.
///
/// Short on purpose: Telegram allows 64 bytes for it, and it is written and
/// read in this file alone.
fn decision(data: &str) -> Option<(u64, Answer)> {
    let (letter, id) = data.split_once(':')?;
    let answer = match letter {
        "a" => Answer::Allow,
        "A" => Answer::AllowAlways,
        "d" => Answer::Deny,
        _ => return None,
    };
    Some((id.parse().ok()?, answer))
}

/// The word shown on the button that was pressed.
fn told(answer: Answer) -> &'static str {
    match answer {
        Answer::Allow => "allowed",
        Answer::AllowAlways => "allowed, and from now on",
        Answer::Deny => "denied",
    }
}

/// How long to wait after `failures` failed calls in a row.
fn backoff(failures: u32) -> Duration {
    BACKOFF
        .saturating_mul(1u32 << failures.min(5).saturating_sub(1))
        .min(LONGEST)
}

/// Cut a text into pieces Telegram will take.
///
/// At a line end where there is one, so a broken message reads as two, and at
/// the character otherwise — a single line longer than the limit has nowhere
/// better to break. Never inside a character, which is what makes this worth a
/// function rather than a slice.
fn chunks(text: &str, limit: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        // Bytes first: a text short in bytes is short in characters too, and
        // counting characters is the slow way to learn it.
        if rest.len() <= limit {
            pieces.push(rest);
            break;
        }
        let end = match rest.char_indices().nth(limit) {
            Some((at, _)) => at,
            None => {
                pieces.push(rest);
                break;
            }
        };
        let cut = rest[..end].rfind('\n').map_or(end, |at| at + 1);
        let (head, tail) = rest.split_at(cut);
        pieces.push(head.trim_end_matches('\n'));
        rest = tail;
    }

    pieces.retain(|piece| !piece.is_empty());
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_text_is_one_piece() {
        assert_eq!(chunks("hello", LIMIT), vec!["hello"]);
    }

    #[test]
    fn a_long_text_breaks_at_a_line_end() {
        let text = format!("{}\n{}", "a".repeat(60), "b".repeat(60));
        let pieces = chunks(&text, 100);
        assert_eq!(pieces, vec!["a".repeat(60), "b".repeat(60)]);
    }

    #[test]
    fn a_line_with_no_break_in_it_is_cut_where_it_must_be() {
        let text = "a".repeat(250);
        let pieces = chunks(&text, 100);
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[0].len(), 100);
        assert_eq!(pieces.concat(), text);
    }

    #[test]
    fn a_character_is_never_cut_in_half() {
        // Four bytes each, so a limit in characters and one in bytes disagree.
        let text = "🐜".repeat(50);
        let pieces = chunks(&text, 10);
        assert_eq!(pieces.len(), 5);
        for piece in &pieces {
            assert_eq!(piece.chars().count(), 10);
        }
        assert_eq!(pieces.concat(), text);
    }

    #[test]
    fn commands_are_the_first_word_only() {
        assert_eq!(command("/new"), Command::New);
        assert_eq!(command("/new@some_bot"), Command::New);
        assert_eq!(command("/cancel now"), Command::Cancel);
        assert_eq!(command("/start"), Command::Help);
        assert_eq!(
            command("what is /new"),
            Command::Say("what is /new".to_owned())
        );
        assert_eq!(
            command("write to a@b.com"),
            Command::Say("write to a@b.com".to_owned())
        );
    }

    #[test]
    fn a_button_says_which_answer_it_is() {
        assert_eq!(decision("a:7"), Some((7, Answer::Allow)));
        assert_eq!(decision("A:7"), Some((7, Answer::AllowAlways)));
        assert_eq!(decision("d:7"), Some((7, Answer::Deny)));
        assert_eq!(decision("d:seven"), None);
        assert_eq!(decision("x:7"), None);
        assert_eq!(decision("nonsense"), None);
    }

    #[test]
    fn waiting_grows_but_stops_growing() {
        assert_eq!(backoff(1), BACKOFF);
        assert_eq!(backoff(2), BACKOFF * 2);
        assert_eq!(backoff(3), BACKOFF * 4);
        assert_eq!(backoff(4), BACKOFF * 8);
        assert_eq!(backoff(5), LONGEST);
        assert_eq!(backoff(40), LONGEST);
    }
}
