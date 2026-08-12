//! The terminal, without a terminal.
//!
//! [`App::apply`] and [`App::typed`] are pure functions of what arrived and
//! what was typed, so the whole of the interesting behaviour is testable the
//! way `aphid-code`'s renderer is tested — by asking what it decided, not by
//! drawing it.

#![cfg(feature = "tui")]

use aphid_colony::tui::app::{App, Send};
use aphid_colony::tui::chats::Kind as ChatKind;
use aphid_nostr::nostr::event::{Event, EventBuilder, FinalizeEvent, Kind, Tag};
use aphid_nostr::nostr::key::Keys;
use aphid_nostr::nostr::message::{RelayMessage, SubscriptionId};
use aphid_nostr::nostr::types::Timestamp;
use aphid_nostr::{GroupId, chat, direct_id, group};

fn id(name: &str) -> GroupId {
    GroupId::parse(name).expect("a group id")
}

fn app(me: &Keys) -> App {
    App::new(me.clone(), "ws://127.0.0.1:7777".to_owned())
}

/// What the relay would send: an EVENT on the terminal's subscription.
fn arriving(event: Event) -> RelayMessage<'static> {
    RelayMessage::event(SubscriptionId::new("colony"), event)
}

/// A relay-signed 39002 saying who is in a group.
fn members(relay: &Keys, group: &GroupId, everybody: &[&Keys]) -> Event {
    let mut tags = vec![Tag::identifier(group.as_str())];
    tags.extend(
        everybody
            .iter()
            .map(|keys| Tag::public_key(keys.public_key())),
    );
    EventBuilder::new(Kind::GroupMembers, "")
        .tags(tags)
        .finalize(relay)
        .expect("signs")
}

fn said(keys: &Keys, group: &GroupId, text: &str, at: u64) -> Event {
    chat::message(group, text, &[], &[])
        .custom_created_at(Timestamp::from_secs(at))
        .finalize(keys)
        .expect("signs")
}

fn named(keys: &Keys, name: &str) -> Event {
    EventBuilder::new(Kind::Metadata, format!(r#"{{"name":"{name}"}}"#))
        .finalize(keys)
        .expect("signs")
}

/// The single event a batch of sends published, insisting there was one.
fn published(sending: Vec<Send>) -> Event {
    let mut events: Vec<Event> = sending
        .into_iter()
        .filter_map(|send| match send {
            Send::Publish(event) => Some(*event),
            _ => None,
        })
        .collect();
    assert_eq!(events.len(), 1, "expected exactly one event");
    events.remove(0)
}

#[test]
fn the_opening_request_asks_for_everything_it_draws() {
    let me = Keys::generate();
    let Send::Subscribe(id, filters) = app(&me).opening() else {
        panic!("the terminal opens with a REQ");
    };
    assert_eq!(id, "colony");
    assert_eq!(filters.len(), 3, "one round trip, one EOSE");
}

#[test]
fn a_group_the_colony_knows_becomes_a_row() {
    let me = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);

    app.apply(&arriving(members(&relay, &id("general"), &[])));

    assert_eq!(app.chats.rows().len(), 1);
    assert_eq!(app.chats.rows()[0].kind, ChatKind::Channel);
    assert!(!app.chats.rows()[0].joined, "nobody is in it yet");
    assert_eq!(app.chats.rows()[0].label(&app.names), "#general");
}

#[test]
fn a_membership_list_says_whether_this_terminal_is_in_it() {
    let me = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);

    app.apply(&arriving(members(&relay, &id("general"), &[&me])));
    assert!(app.chats.rows()[0].joined);
    assert_eq!(
        app.members.get(&id("general")).expect("a membership list"),
        &vec![me.public_key()]
    );
}

#[test]
fn a_two_member_group_is_a_direct_message_labelled_by_the_other_one() {
    let me = Keys::generate();
    let scout = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);
    let group = direct_id(&me.public_key(), &scout.public_key());

    app.apply(&arriving(members(&relay, &group, &[&me, &scout])));
    app.apply(&arriving(named(&scout, "scout")));

    let row = &app.chats.rows()[0];
    assert_eq!(row.kind, ChatKind::Direct);
    assert_eq!(row.label(&app.names), "@scout");
}

#[test]
fn a_name_renames_everything_already_drawn() {
    let me = Keys::generate();
    let scout = Keys::generate();
    let mut app = app(&me);

    app.apply(&arriving(said(&scout, &id("general"), "morning", 100)));
    // Before the kind 0, the author shows as the head of its key.
    assert!(!app.names.contains_key(&scout.public_key()));

    app.apply(&arriving(named(&scout, "scout")));
    assert_eq!(
        app.names.get(&scout.public_key()).map(String::as_str),
        Some("scout")
    );
}

#[test]
fn something_said_elsewhere_is_unread_and_here_is_not() {
    let me = Keys::generate();
    let other = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);

    app.apply(&arriving(members(&relay, &id("general"), &[&me])));
    app.apply(&arriving(members(&relay, &id("build"), &[&me])));
    app.chats.select(&id("general"));

    app.apply(&arriving(said(&other, &id("general"), "here", 100)));
    app.apply(&arriving(said(&other, &id("build"), "elsewhere", 101)));

    let unread = |name: &str| {
        app.chats
            .rows()
            .iter()
            .find(|chat| chat.id == id(name))
            .expect("a row")
            .unread
    };
    assert_eq!(
        unread("general"),
        0,
        "the chat on screen is read as it arrives"
    );
    assert_eq!(unread("build"), 1);
}

#[test]
fn its_own_words_are_never_unread() {
    let me = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);

    app.apply(&arriving(members(&relay, &id("general"), &[&me])));
    app.apply(&arriving(members(&relay, &id("build"), &[&me])));
    app.chats.select(&id("general"));

    // Something this terminal published into a chat it is not looking at.
    app.apply(&arriving(said(&me, &id("build"), "mine", 100)));
    assert_eq!(
        app.chats
            .rows()
            .iter()
            .find(|chat| chat.id == id("build"))
            .expect("a row")
            .unread,
        0
    );
}

#[test]
fn an_event_that_arrives_twice_is_drawn_once() {
    // A subscription answers with what is stored and then with what is live,
    // and the two overlap at the edges.
    let me = Keys::generate();
    let other = Keys::generate();
    let mut app = app(&me);

    let event = said(&other, &id("general"), "morning", 100);
    app.apply(&arriving(event.clone()));
    app.apply(&arriving(event));

    assert_eq!(app.current().expect("a log").recent().len(), 1);
}

#[test]
fn typing_puts_a_message_in_the_chat_on_screen() {
    let me = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);
    app.apply(&arriving(members(&relay, &id("general"), &[&me])));
    app.chats.select(&id("general"));

    let event = published(app.typed("morning"));
    assert_eq!(event.kind, chat::CHAT);
    assert_eq!(event.content, "morning");
    assert_eq!(chat::group_of(&event), Some("general"));
    assert!(chat::mentions(&event).next().is_none());
}

#[test]
fn an_at_name_becomes_the_tag_that_wakes_somebody() {
    // A mention is the whole of a colony's wake policy: an agent runs when it
    // is named and not when the room is busy.
    let me = Keys::generate();
    let scout = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);

    app.apply(&arriving(members(&relay, &id("general"), &[&me, &scout])));
    app.apply(&arriving(named(&scout, "scout")));
    app.chats.select(&id("general"));

    let event = published(app.typed("@scout, is the build red?"));
    assert!(chat::mentions_key(&event, &scout.public_key()));
    assert!(!chat::mentions_key(&event, &me.public_key()));
}

#[test]
fn a_name_nobody_has_is_not_a_mention() {
    let me = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);
    app.apply(&arriving(members(&relay, &id("general"), &[&me])));
    app.chats.select(&id("general"));

    let event = published(app.typed("@nobody are you there"));
    assert!(chat::mentions(&event).next().is_none());
    assert_eq!(event.content, "@nobody are you there");
}

#[test]
fn join_makes_a_group_that_is_not_there_and_joins_one_that_is() {
    let me = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);

    let made = published(app.typed("/join design"));
    assert_eq!(made.kind, Kind::GroupCreateGroup);
    assert_eq!(chat::group_of(&made), Some("design"));

    // Once the colony has answered, the same command asks to be let in.
    app.apply(&arriving(members(&relay, &id("design"), &[&relay])));
    let asked = published(app.typed("/join design"));
    assert_eq!(asked.kind, Kind::GroupJoinRequest);
}

#[test]
fn a_leading_hash_is_allowed_because_people_type_it() {
    let me = Keys::generate();
    let mut app = app(&me);
    let made = published(app.typed("/join #design"));
    assert_eq!(chat::group_of(&made), Some("design"));
}

#[test]
fn dm_opens_the_group_the_two_of_them_share() {
    let me = Keys::generate();
    let scout = Keys::generate();
    let mut app = app(&me);
    app.apply(&arriving(named(&scout, "scout")));

    let made = published(app.typed("/dm scout"));
    assert_eq!(made.kind, Kind::GroupCreateGroup);
    assert_eq!(
        chat::group_of(&made),
        Some(direct_id(&me.public_key(), &scout.public_key()).as_str())
    );
    // And it is what is on screen now.
    assert_eq!(
        app.chats.current().expect("one is chosen").kind,
        ChatKind::Direct
    );
}

#[test]
fn a_direct_message_may_be_opened_with_a_key() {
    let me = Keys::generate();
    let scout = Keys::generate();
    let mut app = app(&me);

    let made = published(app.typed(&format!("/dm {}", scout.public_key().to_hex())));
    assert_eq!(
        chat::group_of(&made),
        Some(direct_id(&me.public_key(), &scout.public_key()).as_str())
    );
}

#[test]
fn invite_and_kick_name_somebody_in_the_chat_on_screen() {
    let me = Keys::generate();
    let scout = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);

    app.apply(&arriving(members(&relay, &id("general"), &[&me])));
    app.apply(&arriving(named(&scout, "scout")));
    app.chats.select(&id("general"));

    for (line, kind) in [
        ("/invite scout", Kind::GroupPutUser),
        ("/kick scout", Kind::GroupRemoveUser),
    ] {
        let event = published(app.typed(line));
        assert_eq!(event.kind, kind);
        assert_eq!(chat::group_of(&event), Some("general"));
        assert!(chat::mentions_key(&event, &scout.public_key()));
    }
}

#[test]
fn leave_is_about_the_chat_on_screen() {
    let me = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);
    app.apply(&arriving(members(&relay, &id("general"), &[&me])));
    app.chats.select(&id("general"));

    let event = published(app.typed("/leave"));
    assert_eq!(event.kind, Kind::GroupLeaveRequest);
    assert_eq!(chat::group_of(&event), Some("general"));
}

#[test]
fn me_publishes_a_name() {
    let me = Keys::generate();
    let mut app = app(&me);

    let event = published(app.typed("/me thiago"));
    assert_eq!(event.kind, Kind::Metadata);
    assert!(event.content.contains("thiago"), "{}", event.content);
    assert_eq!(
        app.names.get(&me.public_key()).map(String::as_str),
        Some("thiago")
    );
}

#[test]
fn a_command_nobody_has_says_so() {
    let me = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);
    app.apply(&arriving(members(&relay, &id("general"), &[&me])));
    app.chats.select(&id("general"));

    assert!(app.typed("/wat").is_empty());
    let drawn = app
        .current()
        .expect("a log")
        .rows(80, 40, &app.names, false)
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<String>();
    assert!(drawn.contains("no command /wat"), "{drawn}");
}

#[test]
fn quit_quits() {
    let me = Keys::generate();
    let mut app = app(&me);
    app.typed("/quit");
    assert!(app.quit);
}

#[test]
fn a_closed_subscription_is_asked_for_again() {
    // NIP-01 leaves a client one recovery, and the terminal is the thing that
    // knows what it wanted.
    let me = Keys::generate();
    let mut app = app(&me);

    let again = app.apply(&RelayMessage::closed(
        SubscriptionId::new("colony"),
        "error: the colony fell behind by 4; ask again",
    ));
    assert!(matches!(again.as_slice(), [Send::Subscribe(id, _)] if id == "colony"));
}

#[test]
fn a_refusal_is_shown_and_not_swallowed() {
    let me = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);
    app.apply(&arriving(members(&relay, &id("general"), &[&me])));
    app.chats.select(&id("general"));

    app.apply(&RelayMessage::ok(
        said(&me, &id("general"), "x", 100).id,
        false,
        "restricted: join general before you talk in it",
    ));

    let drawn = app
        .current()
        .expect("a log")
        .rows(80, 40, &app.names, false)
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<String>();
    assert!(drawn.contains("restricted:"), "{drawn}");
}

#[test]
fn a_backfill_asks_for_what_came_before_the_top() {
    let me = Keys::generate();
    let other = Keys::generate();
    let mut app = app(&me);
    app.apply(&arriving(said(&other, &id("general"), "oldest", 100)));
    app.apply(&arriving(said(&other, &id("general"), "newest", 200)));

    let Some(Send::Subscribe(id, filters)) = app.backfill() else {
        panic!("there is more behind it");
    };
    assert_eq!(id, "backfill");
    assert_eq!(
        filters[0].until,
        Some(Timestamp::from_secs(99)),
        "up to just before the oldest one drawn"
    );
}

#[test]
fn a_group_metadata_event_is_enough_to_draw_a_row() {
    let me = Keys::generate();
    let relay = Keys::generate();
    let mut app = app(&me);

    let group = aphid_nostr::Group::create(id("design"), relay.public_key(), Timestamp::now());
    let metadata = group::metadata(&group).finalize(&relay).expect("signs");

    app.apply(&arriving(metadata));
    assert_eq!(app.chats.rows()[0].id, id("design"));
}
