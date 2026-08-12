//! What the terminal holds, and what it does about what arrives.
//!
//! Everything here is a function of the relay messages that have arrived and
//! the keys that have been pressed, so it is all testable without a terminal,
//! the way `aphid-code`'s renderer is tested without drawing.

use std::collections::HashMap;

use aphid_nostr::nostr::event::{Event, EventBuilder, Kind, Tag};
use aphid_nostr::nostr::filter::{Filter, SingleLetterTag};
use aphid_nostr::nostr::key::{Keys, PublicKey};
use aphid_nostr::nostr::message::RelayMessage;
use aphid_nostr::nostr::types::Timestamp;
use aphid_nostr::{GroupId, chat, direct_id, group};

use super::chats::{Chats, Kind as ChatKind};
use super::log::{Log, Names, name_of};

/// The one subscription the terminal opens.
pub const WATCHING: &str = "colony";

/// How many messages of history to ask for.
const HISTORY: usize = 1_000;

/// What `/help` says.
const HELP: &str = "\
/join <name>     make a channel, or join one that is there
/dm <who>        open a conversation with somebody
/leave           leave the chat on screen
/invite <who>    add somebody to the chat on screen
/kick <who>      remove somebody from it
/who             who is in the chat on screen
/chats           every group this colony has
/me <name>       say what you are called
/keys            this terminal's public key
/time            show or hide the times
/clear           clear this log; the colony keeps it
/help  /quit";

/// One thing the terminal wants sent.
#[derive(Debug)]
pub enum Send {
    Publish(Box<Event>),
    Subscribe(String, Vec<Filter>),
    Unsubscribe(String),
}

/// Everything the colony's terminal holds.
pub struct App {
    /// Who the person is. A key of its own, so they are a participant and not
    /// an operator looking in.
    pub me: Keys,
    pub chats: Chats,
    /// One log for each chat that has been drawn. Made on the first message for
    /// a group and never eagerly, so a colony with two hundred groups draws two.
    pub logs: HashMap<GroupId, Log>,
    /// Names for keys, from kind 0. A colony is a handful of participants, so
    /// the whole map fits and nothing is evicted.
    pub names: Names,
    /// Who is in each group, as the last 39002 said. This is what answers
    /// `/who`, and what tells a chat this terminal may write in from one it may
    /// only read.
    pub members: HashMap<GroupId, Vec<PublicKey>>,
    /// Where this colony is, for the status line.
    pub url: String,
    pub show_time: bool,
    pub quit: bool,
    /// A note with nowhere better to go, before any chat exists.
    pub notice: Option<String>,
}

impl App {
    #[must_use]
    pub fn new(me: Keys, url: String) -> Self {
        Self {
            me,
            chats: Chats::default(),
            logs: HashMap::new(),
            names: Names::new(),
            members: HashMap::new(),
            url,
            show_time: true,
            quit: false,
            notice: None,
        }
    }

    /// What to ask the colony for when the connection opens.
    ///
    /// One `REQ` with three filters, so it is one round trip and one `EOSE`:
    /// the metadata builds the nav, the kind 0s name it, and the chat fills the
    /// logs.
    #[must_use]
    pub fn opening(&self) -> Send {
        Send::Subscribe(
            WATCHING.to_owned(),
            vec![
                Filter::new()
                    .kinds([Kind::GroupMetadata, Kind::GroupMembers])
                    .limit(HISTORY),
                Filter::new().kind(Kind::Metadata).limit(HISTORY),
                Filter::new().kind(chat::CHAT).limit(HISTORY),
            ],
        )
    }

    /// Say what this terminal is called, if the configuration named it.
    #[must_use]
    pub fn naming(&self, name: Option<&str>) -> Option<Send> {
        let name = name?;
        let content = serde_json::json!({ "name": name }).to_string();
        self.sign(EventBuilder::new(Kind::Metadata, content))
    }

    /// Something the colony said.
    ///
    /// Answers with whatever has to go back — nothing, for everything except a
    /// backfill that has run out.
    pub fn apply(&mut self, message: &RelayMessage<'_>) -> Vec<Send> {
        match message {
            RelayMessage::Event { event, .. } => {
                self.arrived(event);
                Vec::new()
            }
            RelayMessage::Ok {
                status: false,
                message,
                ..
            } => {
                self.note(message);
                Vec::new()
            }
            RelayMessage::Closed { message, .. } => {
                self.note(&format!("the colony closed a subscription: {message}"));
                // Whatever went wrong, the answer NIP-01 leaves is to ask
                // again, and the terminal is the thing that knows what it
                // wanted.
                vec![self.opening()]
            }
            RelayMessage::Notice(text) => {
                self.note(text);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// An event arrived.
    fn arrived(&mut self, event: &Event) {
        match event.kind {
            Kind::Metadata => self.named(event),
            Kind::GroupMetadata => {
                if let Ok(metadata) = group::read_metadata(event) {
                    self.chats.know(&metadata.id, &self.me.public_key());
                }
            }
            Kind::GroupMembers => {
                if let Ok((id, everybody)) = group::read_members(event) {
                    self.chats.know(&id, &self.me.public_key());
                    self.chats
                        .membership(&id, everybody.contains(&self.me.public_key()));
                    self.members.insert(id, everybody);
                }
            }
            kind if kind == chat::CHAT => self.said(event),
            _ => {}
        }
    }

    /// Somebody said what they are called.
    fn named(&mut self, event: &Event) {
        let Ok(profile) = serde_json::from_str::<serde_json::Value>(&event.content) else {
            return;
        };
        if let Some(name) = profile.get("name").and_then(serde_json::Value::as_str)
            && !name.trim().is_empty()
        {
            self.names.insert(event.pubkey, name.trim().to_owned());
        }
    }

    /// Somebody said something.
    fn said(&mut self, event: &Event) {
        let Some(named) = chat::group_of(event) else {
            return;
        };
        let Ok(id) = GroupId::parse(named) else {
            return;
        };

        self.chats.know(&id, &self.me.public_key());
        self.logs.entry(id.clone()).or_default().push(event);

        // Its own words are never unread, and neither is the chat on screen.
        let watching = self.chats.selected() == Some(&id);
        let mine = event.pubkey == self.me.public_key();
        self.chats.said(&id, event.created_at, !watching && !mine);
    }

    /// Put a note in the chat on screen, or park it until there is one.
    pub fn note(&mut self, text: &str) {
        match self.chats.selected().cloned() {
            Some(id) => self.logs.entry(id).or_default().note(text),
            None => self.notice = Some(text.to_owned()),
        }
    }

    /// The log for the chat on screen.
    #[must_use]
    pub fn current(&self) -> Option<&Log> {
        self.chats.selected().and_then(|id| self.logs.get(id))
    }

    /// Sign a builder with this terminal's key.
    fn sign(&self, builder: EventBuilder) -> Option<Send> {
        use aphid_nostr::nostr::event::FinalizeEvent;
        builder
            .finalize(&self.me)
            .ok()
            .map(|event| Send::Publish(Box::new(event)))
    }

    /// A line the person typed.
    ///
    /// Plain text is a message in the chat on screen. Anything beginning with a
    /// slash is a command, because a chat that took `/join` as a sentence would
    /// be a chat nobody could join anything from.
    pub fn typed(&mut self, line: &str) -> Vec<Send> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        if let Some(rest) = line.strip_prefix('/') {
            return self.command(rest);
        }

        let Some(id) = self.chats.selected().cloned() else {
            self.note("there is nowhere to say that yet; /join a channel");
            return Vec::new();
        };

        // Every `@name` in the line becomes a `p` tag, because a mention is
        // what wakes an agent — a question that names nobody is a question
        // nobody will answer.
        let mentions = self.mentioned(line);
        let seen = self.logs.get(&id).map(Log::recent).unwrap_or_default();
        self.sign(chat::message(&id, line, &mentions, &seen))
            .into_iter()
            .collect()
    }

    /// The keys an `@name` in a line refers to.
    fn mentioned(&self, line: &str) -> Vec<PublicKey> {
        let mut named = Vec::new();
        for word in line.split_whitespace() {
            let Some(name) = word.strip_prefix('@') else {
                continue;
            };
            let name = name.trim_end_matches([',', '.', ':', ';', '?', '!']);
            if let Some(who) = self.whois(name)
                && !named.contains(&who)
            {
                named.push(who);
            }
        }
        named
    }

    /// Turn a name, or a key in hex, into a key.
    #[must_use]
    pub fn whois(&self, who: &str) -> Option<PublicKey> {
        let who = who.trim().trim_start_matches('@');
        if let Some((key, _)) = self.names.iter().find(|(_, name)| name == &who) {
            return Some(*key);
        }
        PublicKey::parse(who).ok()
    }

    fn command(&mut self, line: &str) -> Vec<Send> {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();

        match verb {
            "help" => {
                self.note(HELP);
                Vec::new()
            }
            "quit" | "exit" => {
                self.quit = true;
                Vec::new()
            }
            "time" => {
                self.show_time = !self.show_time;
                Vec::new()
            }
            "clear" => {
                if let Some(id) = self.chats.selected().cloned() {
                    self.logs.entry(id).or_default().clear();
                }
                Vec::new()
            }
            "keys" => {
                let key = self.me.public_key().to_hex();
                self.note(&format!("this terminal is {key}"));
                Vec::new()
            }
            "me" => {
                if rest.is_empty() {
                    self.note("/me <name> — what to call you here");
                    return Vec::new();
                }
                self.names.insert(self.me.public_key(), rest.to_owned());
                self.naming(Some(rest)).into_iter().collect()
            }
            "chats" => {
                let mut lines = String::from("every group here:");
                for chat in self.chats.rows() {
                    let mark = if chat.joined { "*" } else { " " };
                    lines.push_str(&format!("\n{mark} {}", chat.label(&self.names)));
                }
                self.note(&lines);
                Vec::new()
            }
            "who" => {
                self.who();
                Vec::new()
            }
            "join" => self.join(rest),
            "dm" => self.dm(rest),
            "leave" => self.moderate(Kind::GroupLeaveRequest, None),
            "invite" => self.about(Kind::GroupPutUser, rest, "/invite <who>"),
            "kick" => self.about(Kind::GroupRemoveUser, rest, "/kick <who>"),
            other => {
                self.note(&format!("no command /{other}; try /help"));
                Vec::new()
            }
        }
    }

    fn who(&mut self) {
        let Some(id) = self.chats.selected().cloned() else {
            return;
        };
        let Some(everybody) = self.members.get(&id).cloned() else {
            self.note("the colony has not said who is here yet");
            return;
        };
        let mut lines = format!("{} of them here:", everybody.len());
        for who in everybody {
            lines.push_str(&format!("\n  {}", name_of(who, &self.names)));
        }
        self.note(&lines);
    }

    /// `/join <name>` — make the channel, or ask to be let into it.
    fn join(&mut self, name: &str) -> Vec<Send> {
        let name = name.trim().trim_start_matches('#');
        let Ok(id) = GroupId::parse(name) else {
            self.note("a channel is named with letters, digits, dash, dot and underscore");
            return Vec::new();
        };
        if id.is_direct() {
            self.note("that is a conversation, not a channel; try /dm");
            return Vec::new();
        }

        // A group the colony has never heard of has to be made before anybody
        // can join it. Both are idempotent, so asking the wrong one costs an
        // `OK true` and nothing else.
        let kind = if self.chats.rows().iter().any(|chat| chat.id == id) {
            Kind::GroupJoinRequest
        } else {
            Kind::GroupCreateGroup
        };
        self.chats.know(&id, &self.me.public_key());
        self.chats.select(&id);
        self.moderate(kind, Some(id))
    }

    /// `/dm <who>` — open the conversation with somebody.
    fn dm(&mut self, who: &str) -> Vec<Send> {
        let Some(other) = self.whois(who) else {
            self.note(&format!("nobody here is called {who}"));
            return Vec::new();
        };
        let id = direct_id(&self.me.public_key(), &other);
        self.chats.know(&id, &self.me.public_key());
        self.chats.select(&id);
        // Creating is idempotent for a member, so this both opens a new
        // conversation and switches to one that is already open.
        self.moderate(Kind::GroupCreateGroup, Some(id))
    }

    /// A moderation event about somebody, in the chat on screen.
    fn about(&mut self, kind: Kind, who: &str, usage: &str) -> Vec<Send> {
        let Some(other) = self.whois(who) else {
            self.note(&format!("{usage} — nobody here is called {who}"));
            return Vec::new();
        };
        let Some(id) = self.chats.selected().cloned() else {
            return Vec::new();
        };
        self.sign(EventBuilder::new(kind, "").tags([chat::h(&id), Tag::public_key(other)]))
            .into_iter()
            .collect()
    }

    /// A moderation event about the chat itself.
    fn moderate(&mut self, kind: Kind, id: Option<GroupId>) -> Vec<Send> {
        let Some(id) = id.or_else(|| self.chats.selected().cloned()) else {
            self.note("there is no chat on screen");
            return Vec::new();
        };
        self.sign(EventBuilder::new(kind, "").tags([chat::h(&id)]))
            .into_iter()
            .collect()
    }

    /// Ask for what came before the top of this log.
    #[must_use]
    pub fn backfill(&self) -> Option<Send> {
        let id = self.chats.selected()?;
        let oldest = self.logs.get(id)?.oldest()?;
        let letter = SingleLetterTag::from_char('h').ok()?;
        Some(Send::Subscribe(
            "backfill".to_owned(),
            vec![
                Filter::new()
                    .kind(chat::CHAT)
                    .custom_tags(letter, [id.to_string()])
                    .until(Timestamp::from_secs(oldest.as_secs().saturating_sub(1)))
                    .limit(200),
            ],
        ))
    }
}

/// What the pane on the right is called.
#[must_use]
pub fn heading(app: &App) -> String {
    match app.chats.current() {
        None => "colony".to_owned(),
        Some(chat) => {
            let label = chat.label(&app.names);
            match chat.kind {
                ChatKind::Direct if !chat.joined => format!("{label} (reading only)"),
                _ if !chat.joined => format!("{label} — /join to talk"),
                _ => label,
            }
        }
    }
}
