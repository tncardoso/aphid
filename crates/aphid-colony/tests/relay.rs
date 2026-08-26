//! A relay, in process, with clients on it.
//!
//! Everything here binds port zero and talks over a real socket, because the
//! things worth testing about a relay are the ones that only happen when two
//! connections are doing something at once.

#![cfg(feature = "relay")]

mod common;

use std::time::Duration;

use aphid_colony::client::Client;
use aphid_colony::relay::{Options, Relay};
use aphid_colony::store::Store;
use aphid_nostr::nostr::event::{Event, EventBuilder, FinalizeEvent, Kind, Tag};
use aphid_nostr::nostr::filter::Filter;
use aphid_nostr::nostr::key::Keys;
use aphid_nostr::nostr::message::RelayMessage;
use aphid_nostr::nostr::types::Timestamp;
use aphid_nostr::{GroupId, chat, direct_id, group};
use common::Temp;

/// Nothing here should take a second. A test that hangs is a test that failed.
const PATIENCE: Duration = Duration::from_secs(5);

fn id(name: &str) -> GroupId {
    GroupId::parse(name).expect("a group id")
}

async fn colony(channels: &[&str]) -> (Relay, String) {
    let relay = Relay::bind(Options {
        address: "127.0.0.1:0".parse().expect("an address"),
        store: Store::open_in_memory().expect("a store"),
        keys: Keys::generate(),
        channels: channels.iter().map(|name| (*name).to_owned()).collect(),
        history: 1_000,
    })
    .await
    .expect("a colony starts");

    let url = format!("ws://{}", relay.address());
    (relay, url)
}

async fn join(url: &str, keys: &Keys, group: &GroupId) -> Client {
    let client = Client::connect(url).await.expect("a client connects");
    let ask = EventBuilder::new(Kind::GroupJoinRequest, "")
        .tags([chat::h(group)])
        .finalize(keys)
        .expect("signs");
    client.publish(ask).await.expect("asks to join");
    accepted(&client).await;
    client
}

/// Wait for the next `OK`, and insist it is a good one.
async fn accepted(client: &Client) -> String {
    let message = next(client).await;
    match message {
        RelayMessage::Ok {
            status, message, ..
        } => {
            assert!(status, "the colony refused it: {message}");
            message.into_owned()
        }
        other => panic!("expected an OK, got {other:?}"),
    }
}

/// Wait for the next `OK`, and insist it is a refusal.
async fn refused(client: &Client) -> String {
    match next(client).await {
        RelayMessage::Ok {
            status, message, ..
        } => {
            assert!(!status, "the colony took it: {message}");
            message.into_owned()
        }
        other => panic!("expected an OK, got {other:?}"),
    }
}

async fn next(client: &Client) -> RelayMessage<'static> {
    tokio::time::timeout(PATIENCE, client.recv())
        .await
        .expect("the colony answers")
        .expect("the colony is still there")
}

/// Read until the next `EVENT`, ignoring everything else.
async fn next_event(client: &Client) -> Event {
    loop {
        if let RelayMessage::Event { event, .. } = next(client).await {
            return event.into_owned();
        }
    }
}

/// Read until `EOSE`, answering with everything stored that came before it.
async fn until_eose(client: &Client) -> Vec<Event> {
    let mut stored = Vec::new();
    loop {
        match next(client).await {
            RelayMessage::Event { event, .. } => stored.push(event.into_owned()),
            RelayMessage::EndOfStoredEvents(_) => return stored,
            RelayMessage::Closed { message, .. } => panic!("the subscription closed: {message}"),
            _ => {}
        }
    }
}

fn said(keys: &Keys, group: &GroupId, text: &str) -> Event {
    chat::message(group, text, &[], &[])
        .finalize(keys)
        .expect("signs")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_reaches_the_other_client() {
    let (_relay, url) = colony(&["general"]).await;
    let general = id("general");
    let (one, other) = (Keys::generate(), Keys::generate());

    let speaker = join(&url, &one, &general).await;
    let listener = join(&url, &other, &general).await;

    listener
        .subscribe("chat", vec![Filter::new().kind(chat::CHAT)])
        .await
        .expect("subscribes");
    assert!(until_eose(&listener).await.is_empty(), "nothing said yet");

    speaker
        .publish(said(&one, &general, "morning"))
        .await
        .expect("says something");
    accepted(&speaker).await;

    let heard = next_event(&listener).await;
    assert_eq!(heard.content, "morning");
    assert_eq!(heard.pubkey, one.public_key());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stranger_cannot_post() {
    let (_relay, url) = colony(&["general"]).await;
    let general = id("general");
    let stranger = Keys::generate();

    let client = Client::connect(&url).await.expect("connects");
    client
        .publish(said(&stranger, &general, "let me in"))
        .await
        .expect("tries");

    let why = refused(&client).await;
    assert!(why.starts_with("restricted:"), "{why}");
    assert!(why.contains("join general"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn there_is_no_such_group() {
    let (_relay, url) = colony(&["general"]).await;
    let keys = Keys::generate();

    let client = Client::connect(&url).await.expect("connects");
    client
        .publish(said(&keys, &id("nowhere"), "anybody?"))
        .await
        .expect("tries");

    let why = refused(&client).await;
    assert!(why.contains("there is no group nowhere"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_with_no_h_tag_is_refused_and_a_kind_zero_is_not() {
    let (_relay, url) = colony(&["general"]).await;
    let keys = Keys::generate();
    let client = Client::connect(&url).await.expect("connects");

    let orphan = EventBuilder::new(chat::CHAT, "to nobody")
        .finalize(&keys)
        .expect("signs");
    client.publish(orphan).await.expect("tries");
    let why = refused(&client).await;
    assert!(why.contains("h tag"), "{why}");

    // A kind 0 is how a participant says what it is called, and belongs to no
    // group. It is the one thing a colony carries without an `h`.
    let name = EventBuilder::new(Kind::Metadata, r#"{"name":"scout"}"#)
        .finalize(&keys)
        .expect("signs");
    client.publish(name).await.expect("says its name");
    accepted(&client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_signature_is_refused() {
    let (_relay, url) = colony(&["general"]).await;
    let general = id("general");
    let keys = Keys::generate();

    let mut forged = said(&keys, &general, "not mine");
    forged.content = "mine now".to_owned();

    let client = Client::connect(&url).await.expect("connects");
    client.publish(forged).await.expect("tries");

    let why = refused(&client).await;
    assert!(why.starts_with("invalid:"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clock_from_the_future_is_refused() {
    let (_relay, url) = colony(&["general"]).await;
    let general = id("general");
    let keys = Keys::generate();
    let client = join(&url, &keys, &general).await;

    let ahead = chat::message(&general, "next week", &[], &[])
        .custom_created_at(Timestamp::from_secs(Timestamp::now().as_secs() + 86_400))
        .finalize(&keys)
        .expect("signs");
    client.publish(ahead).await.expect("tries");

    let why = refused(&client).await;
    assert!(why.contains("created_at is in the future"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resent_event_is_accepted_and_explained() {
    let (_relay, url) = colony(&["general"]).await;
    let general = id("general");
    let keys = Keys::generate();
    let client = join(&url, &keys, &general).await;

    let event = said(&keys, &general, "morning");
    client.publish(event.clone()).await.expect("says it");
    assert_eq!(accepted(&client).await, "");

    client.publish(event).await.expect("says it again");
    let note = accepted(&client).await;
    assert!(note.starts_with("duplicate:"), "{note}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_group_is_readable_by_anybody_and_writable_by_two() {
    // The decision a person has to know about: a direct message is a grouping,
    // not a privacy boundary. Pinned from both directions.
    let (_relay, url) = colony(&[]).await;
    let (one, other, stranger) = (Keys::generate(), Keys::generate(), Keys::generate());
    let group = direct_id(&one.public_key(), &other.public_key());

    let first = Client::connect(&url).await.expect("connects");
    first
        .publish(
            EventBuilder::new(Kind::GroupCreateGroup, "")
                .tags([chat::h(&group)])
                .finalize(&one)
                .expect("signs"),
        )
        .await
        .expect("opens the conversation");
    accepted(&first).await;

    first
        .publish(said(&one, &group, "just us"))
        .await
        .expect("says something");
    accepted(&first).await;

    // Anybody may read it.
    let onlooker = Client::connect(&url).await.expect("connects");
    onlooker
        .subscribe(
            "peek",
            vec![Filter::new().custom_tags(
                aphid_nostr::nostr::filter::SingleLetterTag::from_char('h').expect("a letter"),
                [group.to_string()],
            )],
        )
        .await
        .expect("subscribes");
    let seen = until_eose(&onlooker).await;
    assert!(
        seen.iter().any(|event| event.content == "just us"),
        "a direct group is world-readable"
    );

    // Only the two may write in it.
    let intruder = Client::connect(&url).await.expect("connects");
    intruder
        .publish(said(&stranger, &group, "hello?"))
        .await
        .expect("tries");
    let why = refused(&intruder).await;
    assert!(why.starts_with("restricted:"), "{why}");

    // And nobody may join it.
    intruder
        .publish(
            EventBuilder::new(Kind::GroupJoinRequest, "")
                .tags([chat::h(&group)])
                .finalize(&stranger)
                .expect("signs"),
        )
        .await
        .expect("tries");
    let why = refused(&intruder).await;
    assert!(why.contains("always will"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_group_must_name_the_one_who_opens_it() {
    let (_relay, url) = colony(&[]).await;
    let (one, other, stranger) = (Keys::generate(), Keys::generate(), Keys::generate());
    let group = direct_id(&one.public_key(), &other.public_key());

    let client = Client::connect(&url).await.expect("connects");
    client
        .publish(
            EventBuilder::new(Kind::GroupCreateGroup, "")
                .tags([chat::h(&group)])
                .finalize(&stranger)
                .expect("signs"),
        )
        .await
        .expect("tries");

    let why = refused(&client).await;
    assert!(why.contains("must name you"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_relay_signs_the_membership_list() {
    let (relay, url) = colony(&["general"]).await;
    let general = id("general");
    let keys = Keys::generate();

    let client = Client::connect(&url).await.expect("connects");
    client
        .subscribe("meta", vec![Filter::new().kind(Kind::GroupMembers)])
        .await
        .expect("subscribes");
    let before = until_eose(&client).await;
    assert_eq!(before.len(), 1, "the channel the colony was made with");

    // Joining moves the membership, so the relay re-signs it.
    let ask = EventBuilder::new(Kind::GroupJoinRequest, "")
        .tags([chat::h(&general)])
        .finalize(&keys)
        .expect("signs");
    client.publish(ask).await.expect("asks to join");

    let members = next_event(&client).await;
    assert_eq!(members.kind, Kind::GroupMembers);
    let (named, everybody) = group::read_members(&members).expect("a 39002 reads back");
    assert_eq!(named, general);
    assert!(everybody.contains(&keys.public_key()));
    assert_eq!(relay.groups(), vec!["general".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_stops_the_events() {
    let (_relay, url) = colony(&["general"]).await;
    let general = id("general");
    let (one, other) = (Keys::generate(), Keys::generate());

    let speaker = join(&url, &one, &general).await;
    let listener = join(&url, &other, &general).await;

    listener
        .subscribe("chat", vec![Filter::new().kind(chat::CHAT)])
        .await
        .expect("subscribes");
    until_eose(&listener).await;
    listener.unsubscribe("chat").await.expect("closes it");

    // Something said now must not arrive. Instead of sleeping and asserting an
    // absence, ask a question whose answer can only come after it.
    speaker
        .publish(said(&one, &general, "unheard"))
        .await
        .expect("says something");
    accepted(&speaker).await;

    listener
        .subscribe("again", vec![Filter::new().kind(chat::CHAT)])
        .await
        .expect("subscribes again");
    let stored = until_eose(&listener).await;
    assert_eq!(stored.len(), 1, "it was stored, it just was not sent live");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_count_counts() {
    let (_relay, url) = colony(&["general"]).await;
    let general = id("general");
    let keys = Keys::generate();
    let client = join(&url, &keys, &general).await;

    for text in ["one", "two", "three"] {
        client
            .publish(said(&keys, &general, text))
            .await
            .expect("says something");
        accepted(&client).await;
    }

    client
        .subscribe("chat", vec![Filter::new().kind(chat::CHAT).limit(1)])
        .await
        .expect("subscribes");
    let page = until_eose(&client).await;
    assert_eq!(page.len(), 1, "the limit is a page");

    let counted = serde_json::to_string(&aphid_nostr::nostr::message::ClientMessage::count(
        aphid_nostr::nostr::message::SubscriptionId::new("how-many"),
        Filter::new().kind(chat::CHAT).limit(1),
    ))
    .expect("encodes");
    // The client has no `count`, so this one goes by hand.
    raw(&url, &counted).await;
}

/// Send one line on a connection of its own and read the first answer.
async fn raw(url: &str, line: &str) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connects");
    socket
        .send(Message::text(line.to_owned()))
        .await
        .expect("sends");

    let answer = tokio::time::timeout(PATIENCE, socket.next())
        .await
        .expect("answers")
        .expect("is still there")
        .expect("is a message");
    let Message::Text(text) = answer else {
        panic!("expected text, got {answer:?}");
    };
    assert!(text.contains("\"COUNT\""), "{text}");
    assert!(text.contains("\"count\":3"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_search_filter_closes_the_subscription() {
    let (_relay, url) = colony(&["general"]).await;
    let client = Client::connect(&url).await.expect("connects");

    client
        .subscribe("looking", vec![Filter::new().search("anything")])
        .await
        .expect("asks");

    match next(&client).await {
        RelayMessage::Closed { message, .. } => {
            assert!(message.contains("does not search"), "{message}");
        }
        other => panic!("expected a CLOSED, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_accepted_during_the_stored_phase_is_not_lost() {
    // The hazard every home-made relay ships. A REQ is answered in two phases,
    // and an event accepted between them is gone for ever — which shows up
    // months later as "messages sometimes just do not arrive".
    //
    // The race is deliberate: one client publishes as fast as it can while the
    // other subscribes, and every message must arrive exactly once.
    let (_relay, url) = colony(&["general"]).await;
    let general = id("general");
    let (one, other) = (Keys::generate(), Keys::generate());

    let speaker = join(&url, &one, &general).await;
    let listener = join(&url, &other, &general).await;

    // Something already stored, so the subscription has a page to fill.
    const BEFORE: usize = 20;
    const DURING: usize = 20;
    for index in 0..BEFORE {
        speaker
            .publish(said(&one, &general, &format!("before {index}")))
            .await
            .expect("says something");
        accepted(&speaker).await;
    }

    let racing = tokio::spawn(async move {
        for index in 0..DURING {
            speaker
                .publish(said(&one, &general, &format!("during {index}")))
                .await
                .expect("says something");
            accepted(&speaker).await;
        }
    });

    listener
        .subscribe("chat", vec![Filter::new().kind(chat::CHAT)])
        .await
        .expect("subscribes");

    let mut seen: Vec<String> = Vec::new();
    let mut eose = false;
    while seen.len() < BEFORE + DURING {
        match next(&listener).await {
            RelayMessage::Event { event, .. } => seen.push(event.content.clone()),
            RelayMessage::EndOfStoredEvents(_) => eose = true,
            RelayMessage::Closed { message, .. } => panic!("the subscription closed: {message}"),
            _ => {}
        }
    }
    racing.await.expect("the speaker finishes");

    assert!(eose, "the stored phase ended");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        seen.len(),
        "every message arrived exactly once; these came twice: {seen:?}"
    );
    for index in 0..DURING {
        let wanted = format!("during {index}");
        assert!(seen.contains(&wanted), "{wanted} never arrived: {seen:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_groups_survive_a_restart() {
    let temp = Temp::new("relay");
    let database = temp.path("colony.db");
    let address = "127.0.0.1:0".parse().expect("an address");
    let keys = Keys::generate();
    let general = id("general");
    let joiner = Keys::generate();

    let start = |keys: Keys| {
        let database = database.clone();
        async move {
            Relay::bind(Options {
                address,
                store: Store::open(&database).expect("a store"),
                keys,
                channels: vec!["general".to_owned()],
                history: 1_000,
            })
            .await
            .expect("starts")
        }
    };

    // A colony, somebody joining it, and then the colony stops.
    let (members, groups) = {
        let relay = start(keys.clone()).await;
        let url = format!("ws://{}", relay.address());
        let client = join(&url, &joiner, &general).await;

        client
            .subscribe("meta", vec![Filter::new().kind(Kind::GroupMembers)])
            .await
            .expect("subscribes");
        let stored = until_eose(&client).await;
        (stored, relay.groups())
    };
    assert_eq!(groups, vec!["general".to_owned()]);

    // The same log and the same key, in a new relay.
    let again = start(keys).await;
    let url = format!("ws://{}", again.address());
    assert_eq!(again.groups(), groups);

    let client = Client::connect(&url).await.expect("connects");
    client
        .subscribe("meta", vec![Filter::new().kind(Kind::GroupMembers)])
        .await
        .expect("subscribes");
    let after = until_eose(&client).await;

    // The very same events, not fresh ones: an unchanged group re-signs to the
    // id that is already stored, so a quiet restart writes nothing at all.
    assert_eq!(
        after.iter().map(|event| event.id).collect::<Vec<_>>(),
        members.iter().map(|event| event.id).collect::<Vec<_>>()
    );
    let (_, everybody) = group::read_members(&after[0]).expect("a 39002 reads back");
    assert!(everybody.contains(&joiner.public_key()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_line_that_is_not_a_message_is_a_notice() {
    let (_relay, url) = colony(&[]).await;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connects");
    socket
        .send(Message::text("hello there".to_owned()))
        .await
        .expect("sends");

    let answer = tokio::time::timeout(PATIENCE, socket.next())
        .await
        .expect("answers")
        .expect("is still there")
        .expect("is a message");
    let Message::Text(text) = answer else {
        panic!("expected text, got {answer:?}");
    };
    assert!(text.starts_with("[\"NOTICE\""), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_may_invite_and_a_member_may_not() {
    let (_relay, url) = colony(&["general"]).await;
    let general = id("general");
    let member = Keys::generate();

    let client = join(&url, &member, &general).await;
    let mint = EventBuilder::new(Kind::GroupCreateInvite, "")
        .tags([
            chat::h(&general),
            Tag::custom("code", ["let-me-in-please".to_owned()]),
        ])
        .finalize(&member)
        .expect("signs");
    client.publish(mint).await.expect("tries");

    let why = refused(&client).await;
    assert!(why.contains("only an admin"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_may_still_talk_after_a_restart() {
    let temp = Temp::new("rejoin");
    let database = temp.path("colony.db");
    let address = "127.0.0.1:0".parse().expect("an address");
    let keys = Keys::generate();
    let general = id("general");
    let joiner = Keys::generate();

    let start = |keys: Keys| {
        let database = database.clone();
        async move {
            Relay::bind(Options {
                address,
                store: Store::open(&database).expect("a store"),
                keys,
                channels: vec!["general".to_owned()],
                history: 1_000,
            })
            .await
            .expect("starts")
        }
    };

    {
        let relay = start(keys.clone()).await;
        let url = format!("ws://{}", relay.address());
        let client = join(&url, &joiner, &general).await;
        client
            .publish(said(&joiner, &general, "morning"))
            .await
            .expect("says something");
        accepted(&client).await;
    }

    let again = start(keys).await;
    let url = format!("ws://{}", again.address());
    let client = Client::connect(&url).await.expect("connects");
    client
        .publish(said(&joiner, &general, "still here"))
        .await
        .expect("tries");
    accepted(&client).await;
}
