//! The connect loop, and what it does with what arrives.
//!
//! Structurally [`crate::telegram`]'s `run`: dial, announce, listen, and on
//! failure back off and dial again — with every failure reported as a
//! [`Frame::Notice`] and none of them fatal. A colony with no relay is a reason
//! to have no colony, not a reason for the alate not to wake up.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aphid_nostr::nostr::event::{Event, EventBuilder, EventId, FinalizeEvent, Kind};
use aphid_nostr::nostr::filter::{Filter, SingleLetterTag};
use aphid_nostr::nostr::key::Keys;
use aphid_nostr::nostr::message::RelayMessage;
use aphid_nostr::nostr::types::Timestamp;
use aphid_nostr::{GroupId, chat as nostr_chat, group};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use super::chat;
use super::relay::RelayFn;
use super::tools::{Outbound, Shared};
use crate::config::Colony as Config;
use crate::gateway::Publisher;
use crate::gateway::wire::{Frame, Request};

/// How long to wait after the first failed dial, and the longest to wait after
/// any of them. The ladder [`crate::telegram`] uses, for the same reason: a
/// bridge with no colony must not dial as fast as it can fail.
const BACKOFF: Duration = Duration::from_secs(5);
const LONGEST: Duration = Duration::from_secs(60);

/// The subscription the bridge holds open.
const WATCHING: &str = "colony";

/// How many ids to keep for the `previous` tag on what is said next.
const RECENT: usize = 8;

/// What to start a bridge with.
pub struct Bridge {
    /// The gateway each group attaches to.
    pub socket: PathBuf,
    pub config: Config,
    /// This agent's key.
    pub keys: Keys,
    /// What to publish as a name.
    pub name: String,
    /// How to reach the colony. A function so a test can hand over something
    /// that is not a socket.
    pub connect: Connect,
    /// Where a problem with the colony is reported: the terminals, and
    /// `alate.log`. A channel cannot be told that the colony is unreachable.
    pub notices: Publisher,
    /// The handle the tools put their requests on.
    pub colony: Shared,
    /// The other end of it.
    pub outbound: UnboundedReceiver<Outbound>,
}

/// How to open a connection. The seam a test replaces.
pub type Connect = Arc<dyn Fn() -> ConnectFuture + Send + Sync>;
pub type ConnectFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<RelayFn, String>> + Send + 'static>>;

/// Start the bridge. It runs until the task is dropped or aborted.
#[must_use]
pub fn spawn(bridge: Bridge) -> JoinHandle<()> {
    tokio::spawn(run(bridge))
}

/// Dial, serve, and dial again.
async fn run(bridge: Bridge) {
    let Bridge {
        socket,
        config,
        keys,
        name,
        connect,
        notices,
        colony,
        mut outbound,
    } = bridge;

    let retry = match config.interval() {
        Ok(retry) => retry,
        Err(why) => {
            notices.send(Frame::Notice {
                text: format!("colony: {why}"),
            });
            return;
        }
    };

    let mut wait = BACKOFF.min(retry);
    // A streak of failures is reported once. A colony that is down for an hour
    // should not write seven hundred lines into `alate.log`.
    let mut complaining = true;

    loop {
        match (connect)().await {
            Err(why) => {
                if complaining {
                    notices.send(Frame::Notice {
                        text: format!("colony: {why}"),
                    });
                    complaining = false;
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(LONGEST);
                continue;
            }
            Ok(relay) => {
                complaining = true;
                wait = BACKOFF.min(retry);
                notices.send(Frame::Notice {
                    text: format!("colony: {} is here", config.relay),
                });

                let mut session = Session {
                    relay,
                    keys: keys.clone(),
                    socket: socket.clone(),
                    config: config.clone(),
                    name: name.clone(),
                    colony: colony.clone(),
                    groups: HashMap::new(),
                    mine: HashSet::new(),
                    recent: HashMap::new(),
                    reading: HashMap::new(),
                    notices: notices.clone(),
                };
                session.serve(&mut outbound).await;

                notices.send(Frame::Notice {
                    text: "colony: the connection went; dialling again".to_owned(),
                });
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// One connection's worth of state.
struct Session {
    relay: RelayFn,
    keys: Keys,
    socket: PathBuf,
    config: Config,
    name: String,
    colony: Shared,
    /// One gateway connection for each group the agent has been woken for.
    ///
    /// Opened lazily on the first waking message, the same "a chat connects
    /// when it first says something" rule Telegram follows — which is what
    /// keeps an unattended alate from having a session open in every channel.
    groups: HashMap<GroupId, UnboundedSender<Request>>,
    /// The groups this agent is in, as the relay's 39002 events said.
    mine: HashSet<GroupId>,
    /// The newest ids in each group, for the `previous` tag.
    recent: HashMap<GroupId, Vec<EventId>>,
    /// A `colony_read` in flight: what it has collected, and who is waiting.
    reading: HashMap<String, Reading>,
    notices: Publisher,
}

/// One short-lived `REQ` a tool is waiting on.
struct Reading {
    filter: Filter,
    found: Vec<Event>,
    done: tokio::sync::oneshot::Sender<Result<Vec<Event>, String>>,
}

impl Session {
    /// Announce, subscribe, and carry messages until the connection goes.
    async fn serve(&mut self, outbound: &mut UnboundedReceiver<Outbound>) {
        if let Err(why) = self.announce().await {
            self.notice(&why);
            return;
        }

        loop {
            tokio::select! {
                message = self.relay.recv() => match message {
                    Some(message) => self.arrived(&message).await,
                    None => break,
                },
                request = outbound.recv() => match request {
                    Some(request) => self.asked(request).await,
                    // The tools' half has gone, which means the alate is
                    // stopping.
                    None => break,
                },
            }
        }
    }

    /// Say what this agent is called, ask to be let into its channels, and
    /// start listening.
    async fn announce(&mut self) -> Result<(), String> {
        let profile = serde_json::json!({ "name": self.name }).to_string();
        let named = self.sign(EventBuilder::new(Kind::Metadata, profile))?;
        self.relay.publish(named).await?;

        for channel in &self.config.channels.clone() {
            let Ok(id) = GroupId::parse(channel.trim_start_matches('#')) else {
                self.notice(&format!("{channel:?} is not a channel name"));
                continue;
            };
            // Creating is idempotent for a member and joining is idempotent
            // full stop, so asking for both is one `OK true` more than asking
            // for the right one, and needs no knowledge of what is there.
            for kind in [Kind::GroupCreateGroup, Kind::GroupJoinRequest] {
                let ask = self.sign(EventBuilder::new(kind, "").tags([nostr_chat::h(&id)]))?;
                self.relay.publish(ask).await?;
            }
        }

        let me = self.keys.public_key();
        let letter = SingleLetterTag::from_char('p').map_err(|_| "p is not a letter".to_owned())?;
        self.relay
            .subscribe(
                WATCHING,
                vec![
                    // Which groups this agent is in, and what they are called.
                    Filter::new()
                        .kind(Kind::GroupMembers)
                        .custom_tags(letter, [me.to_hex()]),
                    Filter::new().kind(Kind::GroupMetadata),
                    // Who everybody is, so a mention can be written by name.
                    Filter::new().kind(Kind::Metadata),
                    // Live traffic only. What was said before this agent woke
                    // up is `colony_read`'s job, not a backlog of prompts.
                    Filter::new().kind(nostr_chat::CHAT).since(Timestamp::now()),
                ],
            )
            .await
    }

    /// Something the colony said.
    async fn arrived(&mut self, message: &RelayMessage<'_>) {
        match message {
            RelayMessage::Event {
                subscription_id,
                event,
            } => {
                if let Some(reading) = self.reading.get_mut(subscription_id.as_str()) {
                    if aphid_nostr::filter::matches_live(&reading.filter, event) {
                        reading.found.push(event.clone().into_owned());
                    }
                    return;
                }
                self.watched(event).await;
            }
            RelayMessage::EndOfStoredEvents(id) => {
                self.finished(id.as_str());
            }
            RelayMessage::Ok {
                status: false,
                message,
                ..
            } => self.notice(message),
            RelayMessage::Closed {
                subscription_id,
                message,
            } => {
                if self.finished(subscription_id.as_str()) {
                    return;
                }
                self.notice(&format!("a subscription closed: {message}"));
                // The one recovery NIP-01 leaves. The bridge knows what it
                // asked for, so it asks again.
                if let Err(why) = self.announce().await {
                    self.notice(&why);
                }
            }
            RelayMessage::Notice(text) => self.notice(text),
            _ => {}
        }
    }

    /// An event on the standing subscription.
    async fn watched(&mut self, event: &Event) {
        match event.kind {
            Kind::Metadata => self.named(event),
            Kind::GroupMembers => self.membership(event),
            Kind::GroupMetadata => {
                if let Ok(metadata) = group::read_metadata(event) {
                    self.colony
                        .directory_mut()
                        .groups
                        .entry(metadata.id)
                        .or_default();
                }
            }
            kind if kind == nostr_chat::CHAT => self.said(event).await,
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
            self.colony
                .directory_mut()
                .names
                .insert(event.pubkey, name.trim().to_owned());
        }
    }

    /// The relay said who is in a group.
    fn membership(&mut self, event: &Event) {
        let Ok((id, everybody)) = group::read_members(event) else {
            return;
        };
        let inside = everybody.contains(&self.keys.public_key());
        if inside {
            self.mine.insert(id.clone());
        } else {
            self.mine.remove(&id);
        }
        self.colony.directory_mut().groups.insert(id, inside);
    }

    /// Somebody said something.
    async fn said(&mut self, event: &Event) {
        let Some(named) = nostr_chat::group_of(event) else {
            return;
        };
        let Ok(id) = GroupId::parse(named) else {
            return;
        };

        // Remembered whether or not it wakes anybody: the `previous` tag on
        // what this agent says next should point at what it actually saw.
        let recent = self.recent.entry(id.clone()).or_default();
        recent.insert(0, event.id);
        recent.truncate(RECENT);

        if !self.wakes(event, &id) {
            return;
        }

        let who = self.colony.directory().name_of(&event.pubkey);
        let label = self.label(&id);
        let text = wrap(&label, &who, event);

        // The connection is opened on the first message that wakes the agent
        // for this group, and not before.
        if !self.groups.contains_key(&id) {
            match chat::open(&id, &label, self.socket.clone()).await {
                Ok(sender) => {
                    self.groups.insert(id.clone(), sender);
                }
                Err(why) => {
                    self.notice(&format!("could not open a session for {label}: {why}"));
                    return;
                }
            }
        }

        if let Some(sender) = self.groups.get(&id)
            && sender.send(Request::Prompt { text }).is_err()
        {
            // The session ended. Forget it, so the next message opens a fresh
            // one rather than going nowhere.
            self.groups.remove(&id);
        }
    }

    /// Whether this message is one to wake the agent for.
    ///
    /// Mentions and direct messages, and nothing else. Everything else is kept
    /// by the colony and readable with `colony_read` when the agent wants it.
    fn wakes(&self, event: &Event, group: &GroupId) -> bool {
        let me = self.keys.public_key();
        // Never on its own words: a `colony_send` that woke the sender is a
        // loop that never stops.
        if event.pubkey == me {
            return false;
        }
        if group.is_direct() && self.mine.contains(group) {
            return true;
        }
        self.config.mentions && nostr_chat::mentions_key(event, &me)
    }

    /// What to call a group in a session listing.
    fn label(&self, id: &GroupId) -> String {
        match id.direct_members() {
            None => format!("#{id}"),
            Some((one, two)) => {
                let me = self.keys.public_key();
                let other = if one == me { two } else { one };
                format!("@{}", self.colony.directory().name_of(&other))
            }
        }
    }

    /// A tool asked for something.
    async fn asked(&mut self, request: Outbound) {
        match request {
            Outbound::Say {
                group,
                text,
                mentions,
                done,
            } => {
                let seen = self.recent.get(&group).cloned().unwrap_or_default();
                let built = self.sign(nostr_chat::message(&group, &text, &mentions, &seen));
                let sent = match built {
                    Ok(event) => self.relay.publish(event).await,
                    Err(why) => Err(why),
                };
                let _ = done.send(sent);
            }

            Outbound::Read { filter, done } => {
                // A subscription of its own, so its events are never confused
                // with the standing one's.
                let id = format!("read-{}", self.reading.len().wrapping_add(1));
                let asked = self
                    .relay
                    .subscribe(&id, vec![filter.as_ref().clone()])
                    .await;

                match asked {
                    Err(why) => {
                        let _ = done.send(Err(why));
                    }
                    Ok(()) => {
                        self.reading.insert(
                            id,
                            Reading {
                                filter: *filter,
                                found: Vec::new(),
                                done,
                            },
                        );
                    }
                }
            }
        }
    }

    /// A short-lived subscription has said everything it is going to.
    ///
    /// Answers whether this was one, so a `CLOSED` for a read is not also
    /// reported as a problem with the standing subscription.
    fn finished(&mut self, id: &str) -> bool {
        let Some(reading) = self.reading.remove(id) else {
            return false;
        };
        let _ = reading.done.send(Ok(reading.found));

        let relay = Arc::clone(&self.relay);
        let id = id.to_owned();
        tokio::spawn(async move {
            let _ = relay.unsubscribe(&id).await;
        });
        true
    }

    fn sign(&self, builder: EventBuilder) -> Result<Event, String> {
        builder
            .finalize(&self.keys)
            .map_err(|why| format!("this agent could not sign for the colony: {why}"))
    }

    fn notice(&self, text: &str) {
        self.notices.send(Frame::Notice {
            text: format!("colony: {text}"),
        });
    }
}

/// What the agent is actually prompted with.
///
/// Wrapped the way the memory and the crontab wrap their sections, so the model
/// can tell what somebody said from what the harness said, and so the group has
/// an obvious address to answer at.
fn wrap(label: &str, who: &str, event: &Event) -> String {
    let when = chrono::DateTime::from_timestamp(
        i64::try_from(event.created_at.as_secs()).unwrap_or(i64::MAX),
        0,
    )
    .map(|utc| {
        chrono::DateTime::<chrono::Local>::from(utc)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    })
    .unwrap_or_default();

    format!(
        "<colony group=\"{label}\" from=\"{who}\" at=\"{when}\">\n{}\n</colony>",
        event.content
    )
}
