//! What the agent can do about the colony.
//!
//! Two tools, and neither of them holds the socket. They put a request on a
//! channel and wait for the bridge task to answer it, so a tool called from a
//! session running on one task never touches the connection another task is
//! reading from.
//!
//! Nothing reaches the colony unless the model calls `colony_send`. That is a
//! decision, not an oversight: an agent that posted its every thought would
//! make a hub unreadable, and one that posts on purpose is one you can put in a
//! channel with three others. The cost is that a turn which writes prose and
//! forgets the tool says nothing anywhere, so the description says so plainly
//! and the prose is still visible in `aphid alate attach`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use aphid_agent::rt::{Component, Composition, Context};
use aphid_agent::{ToolHandler, ToolOutcome, Toolbox, tool_fn};
use aphid_nostr::nostr::event::Event;
use aphid_nostr::nostr::filter::{Filter, SingleLetterTag};
use aphid_nostr::nostr::key::PublicKey;
use aphid_nostr::nostr::types::Timestamp;
use aphid_nostr::{GroupId, chat};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

/// How long a tool waits for the colony before it gives up and says so.
const ANSWER: Duration = Duration::from_secs(15);

/// How many messages `colony_read` gives back when nothing says, and the most
/// it will give.
const READ: usize = 50;
const READ_MOST: usize = 200;

/// What the bridge is asked to do.
#[derive(Debug)]
pub enum Outbound {
    /// Say something in a group.
    Say {
        group: GroupId,
        text: String,
        mentions: Vec<PublicKey>,
        done: oneshot::Sender<Result<(), String>>,
    },
    /// A short-lived `REQ`: open it, collect until `EOSE`, close it.
    Read {
        filter: Box<Filter>,
        done: oneshot::Sender<Result<Vec<Event>, String>>,
    },
}

/// What the tools know about the colony without asking it.
///
/// Kept up to date by the bridge from the metadata the relay signs, so a tool
/// can turn `#general` into a group id, and a name into a key, with no round
/// trip and no waiting.
#[derive(Debug, Default)]
pub struct Directory {
    /// Every group, and whether this agent is in it.
    pub groups: HashMap<GroupId, bool>,
    /// Names for keys, from their kind 0 events.
    pub names: HashMap<PublicKey, String>,
}

impl Directory {
    /// Turn what a model typed into a group.
    ///
    /// `#general`, `general`, `@somebody` or a group id. A name is matched
    /// against the groups this agent is in first, so a channel it has joined
    /// wins over one that merely exists.
    #[must_use]
    pub fn group(&self, named: &str, me: &PublicKey) -> Option<GroupId> {
        let named = named.trim();
        if let Some(who) = named.strip_prefix('@') {
            let other = self.whois(who)?;
            return Some(aphid_nostr::direct_id(me, &other));
        }

        let name = named.trim_start_matches('#');
        let id = GroupId::parse(name).ok()?;
        if self.groups.contains_key(&id) {
            return Some(id);
        }
        // A group nobody has mentioned yet is still a group: the colony will
        // say so if it is not.
        Some(id)
    }

    /// Turn a name, or a key in hex, into a key.
    #[must_use]
    pub fn whois(&self, who: &str) -> Option<PublicKey> {
        let who = who.trim().trim_start_matches('@');
        if let Some((key, _)) = self.names.iter().find(|(_, name)| name.as_str() == who) {
            return Some(*key);
        }
        PublicKey::parse(who).ok()
    }

    /// What to call a key.
    #[must_use]
    pub fn name_of(&self, who: &PublicKey) -> String {
        self.names
            .get(who)
            .cloned()
            .unwrap_or_else(|| who.to_hex()[..8].to_owned())
    }
}

/// A handle on the bridge, shared by the tools and the loop.
pub type Shared = Arc<Colony>;

/// What a session can reach of the colony.
#[derive(Debug)]
pub struct Colony {
    outbound: mpsc::UnboundedSender<Outbound>,
    directory: RwLock<Directory>,
    me: PublicKey,
}

impl Colony {
    #[must_use]
    pub fn new(outbound: mpsc::UnboundedSender<Outbound>, me: PublicKey) -> Self {
        Self {
            outbound,
            directory: RwLock::new(Directory::default()),
            me,
        }
    }

    #[must_use]
    pub fn me(&self) -> PublicKey {
        self.me
    }

    /// Read the directory, taking the lock even when a writer panicked holding
    /// it: a stale name is better than a tool that never answers again.
    pub fn directory(&self) -> std::sync::RwLockReadGuard<'_, Directory> {
        match self.directory.read() {
            Ok(directory) => directory,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn directory_mut(&self) -> std::sync::RwLockWriteGuard<'_, Directory> {
        match self.directory.write() {
            Ok(directory) => directory,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The one-line summary that goes in the system prompt.
    ///
    /// The groups this agent is in, and the rule about `colony_send`, which is
    /// the thing a model has to be told because nothing else in a turn implies
    /// it.
    #[must_use]
    pub fn prompt_section(&self) -> Option<String> {
        let directory = self.directory();
        let mut joined: Vec<String> = directory
            .groups
            .iter()
            .filter(|(_, inside)| **inside)
            .map(|(id, _)| {
                if id.is_direct() {
                    let other = id
                        .direct_members()
                        .map(|(one, two)| if one == self.me { two } else { one });
                    match other {
                        Some(other) => format!("@{}", directory.name_of(&other)),
                        None => id.to_string(),
                    }
                } else {
                    format!("#{id}")
                }
            })
            .collect();
        joined.sort();

        let where_it_is = if joined.is_empty() {
            "You are in no channels yet.".to_owned()
        } else {
            format!("You are in {}.", joined.join(", "))
        };

        Some(format!(
            "\n<colony>\nYou share a colony with other agents and with the person who runs \
             them. {where_it_is} Nothing you write reaches anybody there unless you call \
             `colony_send`. Naming somebody with `mention` is what wakes them, so a question \
             that names nobody is a question nobody will answer.\n</colony>\n"
        ))
    }

    /// Put a request on the bridge's queue.
    ///
    /// The tools go through [`Colony::ask`], which also waits for the answer.
    /// This is separate because the waiting is the part with a timeout on it,
    /// and a caller that already holds the receiving end has no use for it.
    ///
    /// # Errors
    ///
    /// Fails when the bridge has stopped.
    pub fn request(&self, outbound: Outbound) -> Result<(), String> {
        self.outbound
            .send(outbound)
            .map_err(|_| "the colony bridge has stopped".to_owned())
    }

    /// Ask the bridge for something and wait for the answer.
    async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, String>>) -> Outbound,
    ) -> Result<T, String> {
        let (done, answer) = oneshot::channel();
        self.request(make(done))?;

        match tokio::time::timeout(ANSWER, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the colony bridge dropped the question".to_owned()),
            Err(_) => Err("the colony did not answer in time".to_owned()),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SendParams {
    /// `#general`, `@somebody`, or a group id.
    pub to: String,
    pub text: String,
    #[serde(default)]
    pub mention: Vec<String>,
}

/// `colony_send` — say something in a channel, or to somebody.
#[must_use]
pub fn send_tool(colony: Shared) -> impl ToolHandler {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "to": {
                "type": "string",
                "description": "Where to say it: `#general` for a channel, or `@name` for one \
                                person."
            },
            "text": { "type": "string", "description": "What to say." },
            "mention": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Who to name, by name. A mention is what wakes an agent, so name \
                                whoever the message is for."
            }
        },
        "required": ["to", "text"],
        "additionalProperties": false
    });
    let description = "Say something in the colony. Nothing you write reaches anybody unless \
                       you call this. Use `mention` to name somebody: a mention is what wakes \
                       them, so a question that names nobody is a question nobody will answer."
        .to_owned();

    tool_fn(
        "colony_send",
        description,
        schema,
        move |params: SendParams, _cx| {
            let colony = colony.clone();
            async move {
                let (group, mentions) = {
                    let directory = colony.directory();
                    let Some(group) = directory.group(&params.to, &colony.me()) else {
                        return ToolOutcome::error(format!(
                            "{:?} is not a channel or a person here",
                            params.to
                        ));
                    };

                    let mut mentions = Vec::new();
                    for who in &params.mention {
                        match directory.whois(who) {
                            Some(key) => mentions.push(key),
                            None => {
                                return ToolOutcome::error(format!(
                                    "nobody in this colony is called {who}"
                                ));
                            }
                        }
                    }
                    // Saying something to one person is a mention of them,
                    // whether or not the model thought to say so.
                    if let Some((one, two)) = group.direct_members() {
                        let other = if one == colony.me() { two } else { one };
                        if !mentions.contains(&other) {
                            mentions.push(other);
                        }
                    }
                    (group, mentions)
                };

                let text = params.text.clone();
                match colony
                    .ask(|done| Outbound::Say {
                        group: group.clone(),
                        text,
                        mentions,
                        done,
                    })
                    .await
                {
                    Ok(()) => ToolOutcome::text(format!("said in {group}")),
                    Err(why) => ToolOutcome::error(why),
                }
            }
        },
    )
}

#[derive(Debug, Deserialize)]
pub struct ReadParams {
    /// `#general`, `@somebody`, or a group id. Absent reads every group.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub since_minutes: Option<u64>,
}

/// `colony_read` — catch up on what was said.
#[must_use]
pub fn read_tool(colony: Shared) -> impl ToolHandler {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "from": {
                "type": "string",
                "description": "Which channel or person to read. Every group you can see when \
                                this is left out."
            },
            "limit": {
                "type": "integer",
                "description": "How many messages, newest last. Fifty when left out."
            },
            "since_minutes": {
                "type": "integer",
                "description": "Only what was said in the last this many minutes."
            }
        },
        "additionalProperties": false
    });
    let description = "Read what has been said in the colony. You are only woken when somebody \
                       names you or writes to you directly, so this is how you catch up on a \
                       channel you have been quiet in."
        .to_owned();

    tool_fn(
        "colony_read",
        description,
        schema,
        move |params: ReadParams, _cx| {
            let colony = colony.clone();
            async move {
                let limit = params.limit.unwrap_or(READ).clamp(1, READ_MOST);
                let mut filter = Filter::new().kind(chat::CHAT).limit(limit);

                if let Some(named) = &params.from {
                    let group = {
                        let directory = colony.directory();
                        directory.group(named, &colony.me())
                    };
                    let Some(group) = group else {
                        return ToolOutcome::error(format!(
                            "{named:?} is not a channel or a person here"
                        ));
                    };
                    let Ok(letter) = SingleLetterTag::from_char('h') else {
                        return ToolOutcome::error("h is not a letter, which cannot happen");
                    };
                    filter = filter.custom_tags(letter, [group.to_string()]);
                }

                if let Some(minutes) = params.since_minutes {
                    let ago = Timestamp::now()
                        .as_secs()
                        .saturating_sub(minutes.saturating_mul(60));
                    filter = filter.since(Timestamp::from_secs(ago));
                }

                let found = colony
                    .ask(|done| Outbound::Read {
                        filter: Box::new(filter),
                        done,
                    })
                    .await;

                match found {
                    Err(why) => ToolOutcome::error(why),
                    Ok(events) => ToolOutcome::text(transcript(&colony, &events)),
                }
            }
        },
    )
}

/// What `colony_read` gives the model: `#group  name: text`, oldest first.
fn transcript(colony: &Colony, events: &[Event]) -> String {
    if events.is_empty() {
        return "nothing has been said there".to_owned();
    }

    let directory = colony.directory();
    let mut events: Vec<&Event> = events.iter().collect();
    events.sort_by_key(|event| (event.created_at, event.id));

    let mut lines = String::new();
    for event in events {
        let group = chat::group_of(event).unwrap_or("?");
        let who = directory.name_of(&event.pubkey);
        // One line for each, with the newlines in a message flattened: a
        // transcript the model reads as a list must not have rows that are
        // secretly several.
        let text = event.content.replace('\n', " ");
        lines.push_str(&format!("#{group}  {who}: {text}\n"));
    }
    lines
}

/// Ships `colony_send` and `colony_read`, and nothing else.
///
/// It subscribes to nothing, exactly as [`CronComponent`] does: the connection is
/// driven by the bridge task, and nothing inside a run touches it.
///
/// [`CronComponent`]: crate::cron::CronComponent
pub struct ColonyComponent {
    colony: Shared,
    tools: Arc<Toolbox>,
}

impl ColonyComponent {
    #[must_use]
    pub fn new(colony: Shared, composition: &Composition) -> Self {
        Self {
            colony,
            tools: Arc::clone(&composition.tools),
        }
    }
}

impl Component for ColonyComponent {
    fn name(&self) -> &str {
        "colony"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        self.tools
            .contribute(ctx, Arc::new(send_tool(self.colony.clone())));
        self.tools
            .contribute(ctx, Arc::new(read_tool(self.colony.clone())));
        Ok(())
    }
}
