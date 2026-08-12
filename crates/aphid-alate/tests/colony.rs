//! The colony bridge: what a mention becomes, and what does not.
//!
//! The daemon is not needed for any of this. The bridge is a gateway client, so
//! a bare [`Server`] and two lines playing the daemon's part — open a session
//! for a connection and greet it — is the whole of the far side. The colony is
//! a [`Relay`] the test drives by hand.

#![cfg(feature = "colony")]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_alate::colony::{self, Bridge, Colony, Outbound, Relay, RelayFn};
use aphid_alate::config::Colony as Config;
use aphid_alate::gateway::wire::{Envelope, Frame, Request};
use aphid_alate::gateway::{Event, Server};
use aphid_nostr::nostr::event::{Event as Note, EventBuilder, FinalizeEvent, Kind, Tag};
use aphid_nostr::nostr::filter::Filter;
use aphid_nostr::nostr::key::Keys;
use aphid_nostr::nostr::message::{RelayMessage, SubscriptionId};
use aphid_nostr::{GroupId, chat, direct_id};
use common::Temp;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const SESSION: &str = "s-1";

fn id(name: &str) -> GroupId {
    GroupId::parse(name).expect("a group id")
}

/// A colony that says what a test feeds it, and remembers what it was told.
///
/// The messages arrive through a channel rather than a script, so a test says
/// **when** each one is delivered — which is what lets it assert that one thing
/// woke the agent and another did not, without ever asserting an absence.
struct Fake {
    incoming: tokio::sync::Mutex<UnboundedReceiver<RelayMessage<'static>>>,
    published: Mutex<Vec<Note>>,
    subscribed: Mutex<Vec<(String, Vec<Filter>)>>,
}

impl Fake {
    fn new() -> (Arc<Self>, UnboundedSender<RelayMessage<'static>>) {
        let (feed, incoming) = unbounded_channel();
        (
            Arc::new(Self {
                incoming: tokio::sync::Mutex::new(incoming),
                published: Mutex::new(Vec::new()),
                subscribed: Mutex::new(Vec::new()),
            }),
            feed,
        )
    }

    fn published(&self) -> Vec<Note> {
        self.published.lock().expect("lock").clone()
    }

    /// Wait for the nth event of a kind, rather than for a fixed time.
    async fn nth(&self, kind: Kind, index: usize) -> Note {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let found: Vec<Note> = self
                .published()
                .into_iter()
                .filter(|event| event.kind == kind)
                .collect();
            if let Some(event) = found.get(index) {
                return event.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no kind {kind} number {index} within five seconds; published: {:?}",
                self.published()
                    .iter()
                    .map(|event| event.kind)
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn subscription(&self, id: &str) -> Vec<Filter> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some((_, filters)) = self
                .subscribed
                .lock()
                .expect("lock")
                .iter()
                .find(|(named, _)| named == id)
            {
                return filters.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no subscription {id} within five seconds"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Relay for Fake {
    fn publish<'a>(&'a self, event: Note) -> colony::Ask<'a> {
        self.published.lock().expect("lock").push(event);
        Box::pin(async { Ok(()) })
    }

    fn subscribe<'a>(&'a self, id: &'a str, filters: Vec<Filter>) -> colony::Ask<'a> {
        self.subscribed
            .lock()
            .expect("lock")
            .push((id.to_owned(), filters));
        Box::pin(async { Ok(()) })
    }

    fn unsubscribe<'a>(&'a self, _id: &'a str) -> colony::Ask<'a> {
        Box::pin(async { Ok(()) })
    }

    fn recv(&self) -> colony::Next<'_> {
        // Nothing until the test says so, which is what a quiet colony does.
        Box::pin(async move { self.incoming.lock().await.recv().await })
    }
}

/// Everything one test needs, wired together.
struct Colonised {
    server: Server,
    events: UnboundedReceiver<Event>,
    api: Arc<Fake>,
    feed: UnboundedSender<RelayMessage<'static>>,
    colony: Arc<Colony>,
    running: tokio::task::JoinHandle<()>,
    me: Keys,
    relay: Keys,
}

impl Drop for Colonised {
    fn drop(&mut self) {
        self.running.abort();
    }
}

fn start(temp: &Temp, channels: &[&str]) -> Colonised {
    let me = Keys::generate();
    let relay = Keys::generate();
    let (api, feed) = Fake::new();
    let socket = temp.path("gateway.sock");
    let (server, events) = Server::bind(&socket, None).expect("bind");

    let (outbound, receiver) = unbounded_channel();
    let colony = Arc::new(Colony::new(outbound, me.public_key()));

    let handed = api.clone();
    let connect: colony::Connect = Arc::new(move || {
        let api = handed.clone();
        Box::pin(async move { Ok(api as RelayFn) })
    });

    let running = colony::spawn(Bridge {
        socket,
        config: Config {
            channels: channels.iter().map(|name| (*name).to_owned()).collect(),
            ..Config::default()
        },
        keys: me.clone(),
        name: "scout".to_owned(),
        connect,
        notices: server.publisher(),
        colony: colony.clone(),
        outbound: receiver,
    });

    Colonised {
        server,
        events,
        api,
        feed,
        colony,
        running,
        me,
        relay,
    }
}

impl Colonised {
    async fn event(&mut self) -> Event {
        tokio::time::timeout(Duration::from_secs(5), self.events.recv())
            .await
            .expect("an event within five seconds")
            .expect("the server did not close")
    }

    /// What the daemon does when a client attaches: give it a session, say so.
    fn greet(&self, connection: u64) {
        self.server.watch(connection, SESSION);
        self.server.reply(
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

    /// Tell the bridge this agent is in a group.
    fn belongs(&self, group: &GroupId) {
        let mut tags = vec![Tag::identifier(group.as_str())];
        tags.push(Tag::public_key(self.me.public_key()));
        let members = EventBuilder::new(Kind::GroupMembers, "")
            .tags(tags)
            .finalize(&self.relay)
            .expect("signs");
        self.say(members);
    }

    fn say(&self, event: Note) {
        self.feed
            .send(RelayMessage::event(SubscriptionId::new("colony"), event))
            .expect("feed");
    }

    /// Somebody else says something in a group.
    fn spoken(&self, who: &Keys, group: &GroupId, text: &str, mentions: &[&Keys]) -> Note {
        let mentions: Vec<_> = mentions.iter().map(|keys| keys.public_key()).collect();
        let event = chat::message(group, text, &mentions, &[])
            .finalize(who)
            .expect("signs");
        self.say(event.clone());
        event
    }

    /// Wait for the session opened for a group, and greet it.
    async fn attached(&mut self) -> (u64, Option<String>) {
        let Event::Opened { connection } = self.event().await else {
            panic!("the next event is a group attaching");
        };
        self.greet(connection);
        (connection, None)
    }

    /// The next prompt the daemon is asked for.
    async fn prompted(&mut self) -> String {
        loop {
            if let Event::Asked {
                request: Request::Prompt { text },
                ..
            } = self.event().await
            {
                return text;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn it_says_what_it_is_called_and_asks_for_its_channels() {
    let temp = Temp::new("colony-announce");
    let held = start(&temp, &["general", "build"]);

    let name = held.api.nth(Kind::Metadata, 0).await;
    assert!(name.content.contains("scout"), "{}", name.content);

    // Create and join for each channel: both are idempotent, so asking for the
    // wrong one costs an `OK true` and needs no knowledge of what is there.
    let made = held.api.nth(Kind::GroupCreateGroup, 1).await;
    assert!(chat::group_of(&made).is_some());
    let joined = held.api.nth(Kind::GroupJoinRequest, 1).await;
    assert!(chat::group_of(&joined).is_some());

    let filters = held.api.subscription("colony").await;
    assert_eq!(filters.len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mention_becomes_a_prompt() {
    let temp = Temp::new("colony-mention");
    let mut held = start(&temp, &["general"]);
    let general = id("general");
    let other = Keys::generate();

    held.api.subscription("colony").await;
    held.belongs(&general);
    held.spoken(&other, &general, "the build is red", &[&held.me.clone()]);

    let (_, _) = held.attached().await;
    let prompt = held.prompted().await;

    assert!(prompt.contains("<colony"), "{prompt}");
    assert!(prompt.contains("group=\"#general\""), "{prompt}");
    assert!(prompt.contains("the build is red"), "{prompt}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_message_becomes_a_prompt_without_being_named() {
    let temp = Temp::new("colony-direct");
    let mut held = start(&temp, &[]);
    let other = Keys::generate();
    let group = direct_id(&held.me.public_key(), &other.public_key());

    held.api.subscription("colony").await;
    held.belongs(&group);
    // No mention: a message to you and to nobody else is not something to read
    // later.
    held.spoken(&other, &group, "are you there", &[]);

    held.attached().await;
    let prompt = held.prompted().await;
    assert!(prompt.contains("are you there"), "{prompt}");
    assert!(
        prompt.contains("group=\"@"),
        "a direct chat is named by who it is with: {prompt}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ordinary_channel_line_wakes_nobody() {
    // Asserted in the strong form. A plain line and then a mention: the next
    // thing the daemon is asked is the mention's, so the plain line woke
    // nothing. Asserting an absence with a sleep would be flaky.
    let temp = Temp::new("colony-quiet");
    let mut held = start(&temp, &["general"]);
    let general = id("general");
    let other = Keys::generate();

    held.api.subscription("colony").await;
    held.belongs(&general);
    held.spoken(&other, &general, "morning everyone", &[]);
    held.spoken(&other, &general, "scout, look at this", &[&held.me.clone()]);

    held.attached().await;
    let prompt = held.prompted().await;
    assert!(prompt.contains("look at this"), "{prompt}");
    assert!(!prompt.contains("morning everyone"), "{prompt}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_does_not_wake_on_its_own_words() {
    // A `colony_send` that woke the sender is a loop that never stops.
    let temp = Temp::new("colony-self");
    let mut held = start(&temp, &["general"]);
    let general = id("general");
    let me = held.me.clone();
    let other = Keys::generate();

    held.api.subscription("colony").await;
    held.belongs(&general);
    // Its own words, naming itself, which is the worst case.
    held.spoken(&me, &general, "I said this", &[&me]);
    held.spoken(&other, &general, "and somebody answered", &[&me]);

    held.attached().await;
    let prompt = held.prompted().await;
    assert!(prompt.contains("somebody answered"), "{prompt}");
    assert!(!prompt.contains("I said this"), "{prompt}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_channels_are_two_sessions() {
    let temp = Temp::new("colony-sessions");
    let mut held = start(&temp, &["general", "build"]);
    let other = Keys::generate();
    let me = held.me.clone();

    held.api.subscription("colony").await;
    held.belongs(&id("general"));
    held.belongs(&id("build"));
    held.spoken(&other, &id("general"), "one", &[&me]);
    held.spoken(&other, &id("build"), "two", &[&me]);

    let mut opened = 0;
    let mut prompts = Vec::new();
    while prompts.len() < 2 {
        match held.event().await {
            Event::Opened { connection } => {
                opened += 1;
                held.greet(connection);
            }
            Event::Asked {
                request: Request::Prompt { text },
                ..
            } => prompts.push(text),
            _ => {}
        }
    }

    assert_eq!(opened, 2, "one gateway connection for each group");
    assert!(prompts.iter().any(|text| text.contains("#general")));
    assert!(prompts.iter().any(|text| text.contains("#build")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_posted_when_a_turn_ends() {
    // The decision, pinned. A turn that writes prose and never calls
    // `colony_send` says nothing in the colony.
    let temp = Temp::new("colony-silent");
    let mut held = start(&temp, &["general"]);
    let general = id("general");
    let other = Keys::generate();
    let me = held.me.clone();

    held.api.subscription("colony").await;
    held.belongs(&general);
    held.spoken(&other, &general, "anybody there", &[&me]);

    let (connection, _) = held.attached().await;
    held.prompted().await;

    let before = held.api.published().len();
    held.server.send(Envelope::from(
        SESSION,
        Frame::Text {
            text: "Yes, I am here.".to_owned(),
        },
    ));
    held.server.send(Envelope::from(
        SESSION,
        Frame::TurnEnded {
            usage: aphid_core::Usage::default(),
            stop: aphid_core::StopReason::Stop,
            error: None,
        },
    ));
    let _ = connection;

    // Ask the bridge for something that does publish, and assert it is the very
    // next thing published — so the prose above published nothing.
    let (done, answer) = tokio::sync::oneshot::channel();
    held.colony
        .directory_mut()
        .groups
        .insert(general.clone(), true);
    let outbound = Outbound::Say {
        group: general.clone(),
        text: "on purpose".to_owned(),
        mentions: Vec::new(),
        done,
    };
    held.colony.request(outbound).expect("the bridge is there");
    answer
        .await
        .expect("the bridge answers")
        .expect("published");

    let published = held.api.published();
    assert_eq!(published.len(), before + 1, "only the tool call published");
    assert_eq!(published[before].content, "on purpose");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn colony_send_publishes_a_kind_nine_with_its_mentions() {
    let temp = Temp::new("colony-send");
    let held = start(&temp, &["general"]);
    let general = id("general");
    let scout = Keys::generate();

    held.api.subscription("colony").await;
    {
        let mut directory = held.colony.directory_mut();
        directory.groups.insert(general.clone(), true);
        directory
            .names
            .insert(scout.public_key(), "aria".to_owned());
    }

    let (done, answer) = tokio::sync::oneshot::channel();
    held.colony
        .request(Outbound::Say {
            group: general.clone(),
            text: "the build is green".to_owned(),
            mentions: vec![scout.public_key()],
            done,
        })
        .expect("the bridge is there");
    answer
        .await
        .expect("the bridge answers")
        .expect("published");

    let said = held.api.nth(chat::CHAT, 0).await;
    assert_eq!(said.content, "the build is green");
    assert_eq!(chat::group_of(&said), Some("general"));
    assert!(chat::mentions_key(&said, &scout.public_key()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn colony_read_opens_a_subscription_and_answers_when_it_ends() {
    let temp = Temp::new("colony-read");
    let held = start(&temp, &["general"]);
    let general = id("general");
    let other = Keys::generate();

    held.api.subscription("colony").await;

    let (done, answer) = tokio::sync::oneshot::channel();
    held.colony
        .request(Outbound::Read {
            filter: Box::new(Filter::new().kind(chat::CHAT)),
            done,
        })
        .expect("the bridge is there");

    let filters = held.api.subscription("read-1").await;
    assert_eq!(filters.len(), 1);

    // Two events on that subscription, and then the end of it.
    for text in ["one", "two"] {
        let event = chat::message(&general, text, &[], &[])
            .finalize(&other)
            .expect("signs");
        held.feed
            .send(RelayMessage::event(SubscriptionId::new("read-1"), event))
            .expect("feed");
    }
    held.feed
        .send(RelayMessage::eose(SubscriptionId::new("read-1")))
        .expect("feed");

    let found = tokio::time::timeout(Duration::from_secs(5), answer)
        .await
        .expect("the read finishes")
        .expect("the bridge answers")
        .expect("a result");
    assert_eq!(found.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_colony_that_cannot_be_reached_is_a_notice_and_not_a_failure() {
    let temp = Temp::new("colony-down");
    let socket = temp.path("gateway.sock");
    let watched = socket.clone();
    let (server, mut events) = Server::bind(&socket, None).expect("bind");
    let (outbound, receiver) = unbounded_channel();
    let me = Keys::generate();
    let colony = Arc::new(Colony::new(outbound, me.public_key()));

    // A terminal is where a notice goes, so a terminal is what reads it — and
    // it attaches **before** the bridge starts, because a notice published to
    // nobody reaches nobody afterwards.
    let mut watching = aphid_alate::gateway::Client::connect(&watched)
        .await
        .expect("attach");
    // Connecting returns when the line is written, not when the server has
    // taken it — and a notice published before the server subscribes this
    // connection reaches nobody. Waiting for the server to say it opened is
    // what makes this a test and not a race.
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("the server takes the connection")
        .expect("the server did not close");

    // A colony that is never there.
    let connect: colony::Connect =
        Arc::new(|| Box::pin(async { Err("nothing is listening".to_owned()) }));

    let running = colony::spawn(Bridge {
        socket,
        config: Config::default(),
        keys: me,
        name: "scout".to_owned(),
        connect,
        notices: server.publisher(),
        colony,
        outbound: receiver,
    });

    let notice = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match watching.recv().await {
                Ok(Some(envelope)) => {
                    if let Frame::Notice { text } = &envelope.frame
                        && text.contains("nothing is listening")
                    {
                        return text.clone();
                    }
                }
                Ok(None) | Err(_) => panic!("the gateway went"),
            }
        }
    })
    .await
    .expect("a notice within five seconds");

    assert!(notice.starts_with("colony:"), "{notice}");
    // And the bridge is still going, waiting to dial again.
    assert!(!running.is_finished());
    running.abort();
}
