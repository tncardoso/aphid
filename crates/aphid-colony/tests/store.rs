//! Every event, in SQLite.
//!
//! The last test in this file is the important one. Everything else pins a rule
//! of NIP-01; `the_store_and_the_matcher_agree` pins the two halves of a
//! subscription against each other, which is the thing that goes quietly wrong.

#![cfg(feature = "relay")]

mod common;

use std::collections::BTreeSet;

use aphid_colony::store::{Saved, Store};
use aphid_nostr::filter::{self, MAX_LIMIT};
use aphid_nostr::nostr::event::{Event, EventBuilder, EventId, FinalizeEvent, Kind, Tag};
use aphid_nostr::nostr::filter::{Filter, SingleLetterTag};
use aphid_nostr::nostr::key::Keys;
use aphid_nostr::nostr::types::Timestamp;
use aphid_nostr::{Selector, chat};
use common::Temp;

fn store() -> Store {
    Store::open_in_memory().expect("a store opens")
}

/// A selector that keeps everything it is given, so a test compares sets and
/// not pages.
fn everything(filter: &Filter) -> Selector {
    let mut selector = Selector::from_filter(filter).expect("reduces");
    selector.limit = MAX_LIMIT;
    selector
}

fn said(keys: &Keys, group: &str, text: &str, at: u64) -> Event {
    EventBuilder::new(chat::CHAT, text)
        .tags([Tag::custom("h", [group.to_owned()])])
        .custom_created_at(Timestamp::from_secs(at))
        .finalize(keys)
        .expect("signs")
}

fn letter(character: char) -> SingleLetterTag {
    SingleLetterTag::from_char(character).expect("a letter")
}

fn ids(events: &[Event]) -> BTreeSet<EventId> {
    events.iter().map(|event| event.id).collect()
}

#[test]
fn an_event_comes_back_exactly_as_it_arrived() {
    let store = store();
    let keys = Keys::generate();
    let event = said(&keys, "general", "morning", 100);

    assert_eq!(store.save(&event).expect("saves"), Saved::Stored);

    let back = store
        .query(&everything(&Filter::new().id(event.id)))
        .expect("queries");
    assert_eq!(back, vec![event.clone()]);
    // The signature still checks, which is what storing the arrived bytes is
    // for: an event re-serialized by a different encoder may not.
    assert!(back[0].verify().is_ok());
}

#[test]
fn a_duplicate_is_not_stored_twice() {
    let store = store();
    let keys = Keys::generate();
    let event = said(&keys, "general", "morning", 100);

    assert_eq!(store.save(&event).expect("saves"), Saved::Stored);
    assert_eq!(store.save(&event).expect("saves"), Saved::Duplicate);
    assert_eq!(store.count(&everything(&Filter::new())).expect("counts"), 1);
}

#[test]
fn an_ephemeral_event_is_not_stored() {
    let store = store();
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::from_u16(20_001), "gone")
        .finalize(&keys)
        .expect("signs");

    assert_eq!(store.save(&event).expect("saves"), Saved::NotStored);
    assert!(
        Saved::NotStored.fans_out(),
        "it is still sent to whoever is listening"
    );
    assert_eq!(store.count(&everything(&Filter::new())).expect("counts"), 0);
}

/// An addressable event: same author, same kind, same `d`.
fn about(keys: &Keys, group: &str, text: &str, at: u64) -> Event {
    EventBuilder::new(Kind::GroupMetadata, text)
        .tags([Tag::identifier(group)])
        .custom_created_at(Timestamp::from_secs(at))
        .finalize(keys)
        .expect("signs")
}

#[test]
fn an_addressable_event_supersedes_the_older_one() {
    let store = store();
    let relay = Keys::generate();

    let older = about(&relay, "general", "first", 100);
    let newer = about(&relay, "general", "second", 200);

    assert_eq!(store.save(&older).expect("saves"), Saved::Stored);
    assert_eq!(store.save(&newer).expect("saves"), Saved::Stored);

    let held = store.query(&everything(&Filter::new())).expect("queries");
    assert_eq!(held, vec![newer], "only the newest version survives");
}

#[test]
fn an_older_addressable_event_does_not_supersede_the_newer_one() {
    let store = store();
    let relay = Keys::generate();

    let older = about(&relay, "general", "first", 100);
    let newer = about(&relay, "general", "second", 200);

    // The other way round: the newer one arrives first.
    assert_eq!(store.save(&newer).expect("saves"), Saved::Stored);
    assert_eq!(store.save(&older).expect("saves"), Saved::Superseded);

    let held = store.query(&everything(&Filter::new())).expect("queries");
    assert_eq!(held, vec![newer]);
}

#[test]
fn a_tie_on_created_at_keeps_the_lower_id() {
    // NIP-01's tie-break, which nobody implements right the first time. Two
    // versions of one addressable event at the same second: the lower id wins,
    // whichever arrives first.
    let relay = Keys::generate();
    let one = about(&relay, "general", "one", 100);
    let other = about(&relay, "general", "other", 100);
    let (low, high) = if one.id < other.id {
        (one, other)
    } else {
        (other, one)
    };

    for (first, second, expected) in [
        (&low, &high, Saved::Superseded),
        (&high, &low, Saved::Stored),
    ] {
        let store = store();
        assert_eq!(store.save(first).expect("saves"), Saved::Stored);
        assert_eq!(store.save(second).expect("saves"), expected);

        let held = store.query(&everything(&Filter::new())).expect("queries");
        assert_eq!(
            held,
            vec![low.clone()],
            "the lower id is the one that stays"
        );
    }
}

#[test]
fn a_tag_filter_finds_only_its_group() {
    let store = store();
    let keys = Keys::generate();
    for event in [
        said(&keys, "general", "one", 100),
        said(&keys, "general", "two", 101),
        said(&keys, "build", "three", 102),
    ] {
        store.save(&event).expect("saves");
    }

    let general = store
        .query(&everything(
            &Filter::new().custom_tags(letter('h'), ["general".to_owned()]),
        ))
        .expect("queries");
    assert_eq!(general.len(), 2);
    assert!(
        general
            .iter()
            .all(|event| chat::group_of(event) == Some("general"))
    );
}

#[test]
fn two_tag_letters_are_anded() {
    let store = store();
    let keys = Keys::generate();
    let other = Keys::generate();

    let named = EventBuilder::new(chat::CHAT, "hello you")
        .tags([
            Tag::custom("h", ["general".to_owned()]),
            Tag::public_key(other.public_key()),
        ])
        .custom_created_at(Timestamp::from_secs(100))
        .finalize(&keys)
        .expect("signs");
    let unnamed = said(&keys, "general", "hello nobody", 101);
    let elsewhere = EventBuilder::new(chat::CHAT, "hello you elsewhere")
        .tags([
            Tag::custom("h", ["build".to_owned()]),
            Tag::public_key(other.public_key()),
        ])
        .custom_created_at(Timestamp::from_secs(102))
        .finalize(&keys)
        .expect("signs");

    for event in [&named, &unnamed, &elsewhere] {
        store.save(event).expect("saves");
    }

    let both = Filter::new()
        .custom_tags(letter('h'), ["general".to_owned()])
        .custom_tags(letter('p'), [other.public_key().to_hex()]);
    assert_eq!(
        store.query(&everything(&both)).expect("queries"),
        vec![named]
    );
}

#[test]
fn a_limit_takes_the_newest() {
    let store = store();
    let keys = Keys::generate();
    for at in 100..110 {
        store
            .save(&said(&keys, "general", "line", at))
            .expect("saves");
    }

    let selector = Selector::from_filter(&Filter::new().limit(3)).expect("reduces");
    let newest = store.query(&selector).expect("queries");
    assert_eq!(newest.len(), 3);
    assert_eq!(
        newest
            .iter()
            .map(|e| e.created_at.as_secs())
            .collect::<Vec<_>>(),
        vec![109, 108, 107],
        "newest first"
    );

    // A count ignores the limit, because a count of the newest three is not a
    // count.
    assert_eq!(store.count(&selector).expect("counts"), 10);
}

#[test]
fn a_void_selector_touches_nothing() {
    let store = store();
    let keys = Keys::generate();
    store
        .save(&said(&keys, "general", "line", 100))
        .expect("saves");

    let selector = Selector::from_filter(&Filter::new().kinds(Vec::new())).expect("reduces");
    assert!(selector.void);
    assert!(store.query(&selector).expect("queries").is_empty());
    assert_eq!(store.count(&selector).expect("counts"), 0);
}

#[test]
fn pruning_keeps_the_newest_and_drops_its_tags() {
    let store = store();
    let keys = Keys::generate();
    for at in 100..110 {
        store
            .save(&said(&keys, "general", "line", at))
            .expect("saves");
    }
    for at in 100..103 {
        store
            .save(&said(&keys, "build", "line", at))
            .expect("saves");
    }

    let removed = store.prune(4).expect("prunes");
    assert_eq!(
        removed, 6,
        "six of general's ten went; build was under the mark"
    );

    let left = store.query(&everything(&Filter::new())).expect("queries");
    assert_eq!(left.len(), 7);

    // The tags went with them, by the cascade — so a tag query finds only what
    // is left and not a row pointing at nothing.
    let general = store
        .query(&everything(
            &Filter::new().custom_tags(letter('h'), ["general".to_owned()]),
        ))
        .expect("queries");
    assert_eq!(general.len(), 4);
}

#[test]
fn the_moderation_log_comes_back_oldest_first() {
    let store = store();
    let keys = Keys::generate();

    let moderation = |kind: Kind, at: u64| {
        EventBuilder::new(kind, "")
            .tags([Tag::custom("h", ["general".to_owned()])])
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(&keys)
            .expect("signs")
    };

    for event in [
        moderation(Kind::GroupCreateGroup, 100),
        moderation(Kind::GroupJoinRequest, 200),
        moderation(Kind::GroupLeaveRequest, 300),
    ] {
        store.save(&event).expect("saves");
    }
    // Chat is not moderation, and must not appear in the fold.
    store
        .save(&said(&keys, "general", "morning", 150))
        .expect("saves");

    let log = store.moderation_log().expect("reads the log");
    assert_eq!(
        log.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![
            Kind::GroupCreateGroup,
            Kind::GroupJoinRequest,
            Kind::GroupLeaveRequest
        ]
    );
}

#[test]
fn the_relay_reads_back_only_what_it_signed() {
    let store = store();
    let relay = Keys::generate();
    let somebody = Keys::generate();

    store
        .save(&about(&relay, "general", "mine", 100))
        .expect("saves");
    store
        .save(&about(&somebody, "build", "theirs", 100))
        .expect("saves");
    store
        .save(&said(&relay, "general", "not metadata", 100))
        .expect("saves");

    let mine = store.signed_by(&relay.public_key()).expect("reads");
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].kind, Kind::GroupMetadata);
}

#[test]
fn a_schema_from_a_newer_aphid_is_refused_by_name() {
    let temp = Temp::new("store");
    let path = temp.path("colony.db");
    {
        let store = Store::open(&path).expect("a new file opens");
        drop(store);
    }
    // Pretend a later aphid wrote it.
    let connection = rusqlite::Connection::open(&path).expect("opens");
    connection
        .execute("UPDATE meta SET value = '99' WHERE key = 'schema'", [])
        .expect("updates");
    drop(connection);

    let error = Store::open(&path).expect_err("a newer schema is refused");
    assert!(error.to_string().contains("schema 99"), "{error}");
    assert!(error.to_string().contains("newer aphid"), "{error}");
}

#[test]
fn the_store_and_the_matcher_agree() {
    // The one that keeps the SQL in `store::query` and the predicate in
    // `aphid_nostr::filter` from drifting apart. Every filter is asked of both,
    // and the answers must be the same set.
    let store = store();
    let authors: Vec<Keys> = (0..6).map(|_| Keys::generate()).collect();
    let groups = ["general", "build", "design", "ops", "random"];
    // Regular kinds only, so nothing is replaced and the store holds exactly
    // what was put in it.
    let kinds = [
        chat::CHAT,
        Kind::from_u16(1),
        Kind::from_u16(7),
        Kind::GroupJoinRequest,
    ];

    let mut written = Vec::new();
    for index in 0..200_u64 {
        let author = &authors[index as usize % authors.len()];
        let group = groups[index as usize % groups.len()];
        let kind = kinds[index as usize % kinds.len()];
        let mentioned = &authors[(index as usize + 3) % authors.len()];

        let event = EventBuilder::new(kind, format!("line {index}"))
            .tags([
                Tag::custom("h", [group.to_owned()]),
                Tag::public_key(mentioned.public_key()),
            ])
            .custom_created_at(Timestamp::from_secs(1_000 + index))
            .finalize(author)
            .expect("signs");

        assert_eq!(store.save(&event).expect("saves"), Saved::Stored);
        written.push(event);
    }

    let mut filters = vec![
        Filter::new(),
        Filter::new().kind(chat::CHAT),
        Filter::new().kinds([chat::CHAT, Kind::from_u16(7)]),
        Filter::new().since(Timestamp::from_secs(1_100)),
        Filter::new().until(Timestamp::from_secs(1_050)),
        Filter::new()
            .since(Timestamp::from_secs(1_050))
            .until(Timestamp::from_secs(1_060)),
        Filter::new().custom_tags(letter('h'), ["general".to_owned()]),
        Filter::new().custom_tags(letter('h'), ["general".to_owned(), "build".to_owned()]),
        Filter::new().custom_tags(letter('h'), ["nobody-is-here".to_owned()]),
        Filter::new().id(written[7].id),
        Filter::new().ids([written[7].id, written[9].id]),
        // Asked for nothing, and nothing is what it gets.
        Filter::new().kinds(Vec::new()),
        Filter::new().ids(Vec::new()),
    ];
    for author in &authors {
        filters.push(Filter::new().author(author.public_key()));
        filters.push(
            Filter::new()
                .author(author.public_key())
                .kind(chat::CHAT)
                .custom_tags(letter('h'), ["general".to_owned()]),
        );
        filters.push(Filter::new().custom_tags(letter('p'), [author.public_key().to_hex()]));
    }

    for filter in &filters {
        let stored = ids(&store.query(&everything(filter)).expect("queries"));
        let live: BTreeSet<EventId> = written
            .iter()
            .filter(|event| filter::matches_live(filter, event))
            .map(|event| event.id)
            .collect();

        assert_eq!(
            stored, live,
            "the store and the matcher disagree about {filter:?}"
        );
        assert_eq!(
            store.count(&everything(filter)).expect("counts"),
            live.len(),
            "and the count disagrees too: {filter:?}"
        );
    }
}
