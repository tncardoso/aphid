//! The wire, both ways.
//!
//! One JSON array for each line, so the test that matters is the round trip:
//! everything this relay writes must read back as itself, and everything a
//! client may send must be readable.

use aphid_nostr::nostr::event::{Event, EventBuilder, EventId, FinalizeEvent, Kind};
use aphid_nostr::nostr::filter::Filter;
use aphid_nostr::nostr::key::Keys;
use aphid_nostr::nostr::message::{ClientMessage, RelayMessage, SubscriptionId};
use aphid_nostr::wire::{self, Reason};

fn event() -> Event {
    let keys = Keys::generate();
    EventBuilder::new(Kind::Custom(9), "morning")
        .finalize(&keys)
        .expect("a freshly built event signs")
}

fn sub() -> SubscriptionId {
    SubscriptionId::new("colony")
}

#[test]
fn every_message_round_trips() {
    let event = event();
    let id = event.id;

    let relay = vec![
        RelayMessage::event(sub(), event.clone()),
        wire::accepted(id),
        wire::accepted_with(id, Reason::Duplicate, "this relay has it"),
        wire::refused(id, Reason::Invalid, "the signature does not check"),
        RelayMessage::eose(sub()),
        wire::closed(&sub(), Reason::Restricted, "join #general first"),
        wire::notice(Reason::Error, "the relay fell behind"),
        RelayMessage::count(sub(), 12),
    ];
    for message in relay {
        let json = serde_json::to_string(&message).expect("a relay message encodes");
        let back: RelayMessage = serde_json::from_str(&json).expect("and reads back");
        assert_eq!(message, back, "{json}");
        assert!(!json.contains('\n'), "a frame is one line: {json}");
    }

    let client = vec![
        ClientMessage::event(event),
        ClientMessage::req(sub(), vec![Filter::new().kind(Kind::Custom(9))]),
        ClientMessage::count(sub(), Filter::new()),
        ClientMessage::close(sub()),
    ];
    for message in client {
        let json = serde_json::to_string(&message).expect("a client message encodes");
        let back = wire::parse(&json).expect("and parses back");
        assert_eq!(message, back, "{json}");
    }
}

#[test]
fn a_refusal_names_its_reason() {
    let id = EventId::from_byte_array([7; 32]);
    let json = serde_json::to_string(&wire::refused(id, Reason::Restricted, "join it first"))
        .expect("encodes");
    assert!(json.starts_with("[\"OK\""), "{json}");
    assert!(json.contains("false"), "{json}");
    assert!(json.contains("restricted: join it first"), "{json}");
}

#[test]
fn a_duplicate_is_accepted_and_explained() {
    let id = EventId::from_byte_array([7; 32]);
    let message = wire::accepted_with(id, Reason::Duplicate, "this relay has it");
    let RelayMessage::Ok {
        status, message, ..
    } = message
    else {
        panic!("a duplicate is answered with an OK");
    };
    assert!(status, "a duplicate did arrive, so it is a success");
    assert_eq!(message, "duplicate: this relay has it");
}

#[test]
fn a_line_that_is_not_a_client_message_is_refused_by_name() {
    for line in ["", "{}", "[\"WHAT\"]", "not json at all"] {
        let error = wire::parse(line).expect_err("this is not a client message");
        assert!(
            error.to_string().contains("one JSON array"),
            "{line} -> {error}"
        );
    }
}
