//! The NIP-29 state machine.
//!
//! Every rule the relay enforces is decided here, so this is where each one is
//! pinned. A rule that only lives in the relay is one that has to be tested
//! over a socket, and a rule that is tested over a socket is one nobody
//! rewrites with confidence.

use aphid_nostr::group::{self, Access, Action, Group, GroupId, Member, Role, Verdict};
use aphid_nostr::nostr::event::{Event, EventBuilder, FinalizeEvent, Kind, Tag};
use aphid_nostr::nostr::key::{Keys, PublicKey};
use aphid_nostr::nostr::types::Timestamp;
use aphid_nostr::{chat, direct_id};

fn at(secs: u64) -> Timestamp {
    Timestamp::from_secs(secs)
}

fn id(name: &str) -> GroupId {
    GroupId::parse(name).expect("a group id")
}

/// One signed moderation event.
fn moderation(keys: &Keys, kind: Kind, group: &GroupId, tags: Vec<Tag>) -> Event {
    let mut all = vec![chat::h(group)];
    all.extend(tags);
    EventBuilder::new(kind, "")
        .tags(all)
        .custom_created_at(at(1_000))
        .finalize(keys)
        .expect("a freshly built event signs")
}

fn action(event: &Event) -> (GroupId, Action) {
    Action::read(event)
        .expect("this event is readable")
        .expect("this kind is a moderation kind")
}

/// Apply one event the way the relay does, and say what happened.
fn apply(group: &mut Group, event: &Event) -> Result<group::Change, aphid_nostr::Error> {
    let (named, action) = action(event);
    assert_eq!(
        named, group.id,
        "the h tag names the group it is applied to"
    );
    group.apply(&action, &event.pubkey, event.created_at)
}

fn p(who: &PublicKey) -> Tag {
    Tag::public_key(*who)
}

fn p_as(who: &PublicKey, role: Role) -> Tag {
    Tag::custom("p", [who.to_hex(), role.name().to_owned()])
}

#[test]
fn a_created_group_has_one_admin() {
    let owner = Keys::generate();
    let group = Group::create(id("general"), owner.public_key(), at(1));

    assert_eq!(group.role_of(&owner.public_key()), Some(Role::Admin));
    assert_eq!(group.members.len(), 1);
    assert_eq!(group.access, Access::channel());
    assert!(!group.is_direct());
}

#[test]
fn an_open_group_still_needs_membership_to_post() {
    // NIP-29's `open` governs joining, not posting. A colony refuses a
    // non-member's message in an open group as firmly as in a closed one, so
    // that an agent which mistypes a group name is told rather than quietly
    // enrolled. Do not "fix" this.
    let owner = Keys::generate();
    let stranger = Keys::generate();
    let group = Group::create(id("general"), owner.public_key(), at(1));
    assert!(!group.access.closed, "this group is open");

    let Verdict::Refuse(reason, why) = group.may_publish(&stranger.public_key(), chat::CHAT) else {
        panic!("a stranger may not talk in a group it has not joined");
    };
    assert_eq!(reason, aphid_nostr::Reason::Restricted);
    assert!(why.contains("join general"), "{why}");

    // The one thing a stranger may say to a group is "let me in".
    assert_eq!(
        group.may_publish(&stranger.public_key(), Kind::GroupJoinRequest),
        Verdict::Allow
    );
}

#[test]
fn only_a_member_may_post_to_a_closed_group() {
    let owner = Keys::generate();
    let guest = Keys::generate();
    let mut group = Group::create(id("build"), owner.public_key(), at(1));
    group.access.closed = true;

    assert!(matches!(
        group.may_publish(&guest.public_key(), chat::CHAT),
        Verdict::Refuse(..)
    ));

    group.members.insert(guest.public_key(), Role::Member);
    assert_eq!(
        group.may_publish(&guest.public_key(), chat::CHAT),
        Verdict::Allow
    );
}

#[test]
fn a_join_request_needs_a_code_when_the_group_is_closed() {
    let owner = Keys::generate();
    let guest = Keys::generate();
    let mut group = Group::create(id("build"), owner.public_key(), at(1));
    group.access.closed = true;

    let bare = moderation(&guest, Kind::GroupJoinRequest, &group.id, Vec::new());
    let error = apply(&mut group, &bare).expect_err("a closed group is closed");
    assert!(error.to_string().contains("ask an admin"), "{error}");
    assert!(!group.is_member(&guest.public_key()));

    // An open group takes anybody who asks.
    group.access.closed = false;
    let change = apply(&mut group, &bare).expect("an open group takes anybody");
    assert!(change.members);
    assert_eq!(group.role_of(&guest.public_key()), Some(Role::Member));
}

#[test]
fn an_invite_is_spent_once() {
    let owner = Keys::generate();
    let first = Keys::generate();
    let second = Keys::generate();
    let mut group = Group::create(id("build"), owner.public_key(), at(1));
    group.access.closed = true;

    let code = Tag::custom("code", ["let-me-in-please".to_owned()]);
    let mint = moderation(
        &owner,
        Kind::GroupCreateInvite,
        &group.id,
        vec![code.clone()],
    );
    apply(&mut group, &mint).expect("an admin may mint a code");
    assert_eq!(group.invites.len(), 1);

    let build = group.id.clone();
    let join = |keys: &Keys| moderation(keys, Kind::GroupJoinRequest, &build, vec![code.clone()]);

    apply(&mut group, &join(&first)).expect("the first to use the code gets in");
    assert!(group.is_member(&first.public_key()));
    assert!(group.invites.is_empty(), "the code is spent");

    let error = apply(&mut group, &join(&second)).expect_err("a spent code opens nothing");
    assert!(error.to_string().contains("ask an admin"), "{error}");
    assert!(!group.is_member(&second.public_key()));
}

#[test]
fn only_an_admin_mints_an_invite() {
    let owner = Keys::generate();
    let member = Keys::generate();
    let mut group = Group::create(id("build"), owner.public_key(), at(1));
    group.members.insert(member.public_key(), Role::Member);

    let mint = moderation(
        &member,
        Kind::GroupCreateInvite,
        &group.id,
        vec![Tag::custom("code", ["let-me-in-please".to_owned()])],
    );
    let error = apply(&mut group, &mint).expect_err("a member is not an admin");
    assert!(error.to_string().contains("only an admin"), "{error}");
}

#[test]
fn the_last_admin_cannot_be_removed() {
    let owner = Keys::generate();
    let member = Keys::generate();
    let mut group = Group::create(id("general"), owner.public_key(), at(1));
    group.members.insert(member.public_key(), Role::Member);

    let kick = moderation(
        &owner,
        Kind::GroupRemoveUser,
        &group.id,
        vec![p(&owner.public_key())],
    );
    let error = apply(&mut group, &kick).expect_err("a group needs one admin");
    assert!(error.to_string().contains("needs one admin"), "{error}");
    assert!(group.is_member(&owner.public_key()));

    // And the same rule stops the last admin walking out.
    let leave = moderation(&owner, Kind::GroupLeaveRequest, &group.id, Vec::new());
    assert!(apply(&mut group, &leave).is_err());

    // With a second admin, either may go.
    let promote = moderation(
        &owner,
        Kind::GroupPutUser,
        &group.id,
        vec![p_as(&member.public_key(), Role::Admin)],
    );
    apply(&mut group, &promote).expect("an admin may promote");
    apply(&mut group, &kick).expect("now there is another admin");
    assert!(!group.is_member(&owner.public_key()));
}

#[test]
fn a_member_may_always_leave() {
    let owner = Keys::generate();
    let member = Keys::generate();
    let mut group = Group::create(id("general"), owner.public_key(), at(1));
    group.members.insert(member.public_key(), Role::Member);

    let leave = moderation(&member, Kind::GroupLeaveRequest, &group.id, Vec::new());
    let change = apply(&mut group, &leave).expect("leaving needs no role");
    assert!(change.members);
    assert!(!group.is_member(&member.public_key()));

    // Leaving twice is not an error, and moves nothing.
    let change = apply(&mut group, &leave).expect("leaving twice is quiet");
    assert!(!change.any());
}

#[test]
fn two_changes_in_one_second_are_still_two_changes() {
    // The metadata events carry `changed_at` as their `created_at`, and NIP-01
    // breaks a tie between two versions of one addressable event on the lower
    // id. So a second change within the same second must still say a later
    // time, or the relay re-signs a membership list the store then refuses in
    // favour of the one before it.
    let owner = Keys::generate();
    let first = Keys::generate();
    let second = Keys::generate();
    let mut group = Group::create(id("general"), owner.public_key(), at(1_000));

    let join = |keys: &Keys| {
        EventBuilder::new(Kind::GroupJoinRequest, "")
            .tags([chat::h(&GroupId::parse("general").expect("an id"))])
            .custom_created_at(at(1_000))
            .finalize(keys)
            .expect("signs")
    };

    let was = group.changed_at;
    apply(&mut group, &join(&first)).expect("joins");
    assert!(
        group.changed_at > was,
        "the first change moved the clock on"
    );

    let was = group.changed_at;
    apply(&mut group, &join(&second)).expect("joins");
    assert!(group.changed_at > was, "and so did the second");

    // A change that moves nothing leaves it alone.
    let was = group.changed_at;
    apply(&mut group, &join(&first)).expect("joining twice is quiet");
    assert_eq!(group.changed_at, was);
}

#[test]
fn a_direct_id_is_the_same_from_both_sides() {
    let a = Keys::generate().public_key();
    let b = Keys::generate().public_key();
    assert_eq!(direct_id(&a, &b), direct_id(&b, &a));
    assert!(direct_id(&a, &b).is_direct());
}

#[test]
fn a_direct_id_names_both_members() {
    let a = Keys::generate().public_key();
    let b = Keys::generate().public_key();
    let (first, second) = direct_id(&a, &b)
        .direct_members()
        .expect("a direct id names its two members");
    assert_ne!(first, second);
    assert!([first, second].contains(&a));
    assert!([first, second].contains(&b));
}

#[test]
fn a_direct_group_takes_no_new_members() {
    let a = Keys::generate();
    let b = Keys::generate();
    let intruder = Keys::generate();
    let id = direct_id(&a.public_key(), &b.public_key());
    let mut group = Group::direct(&id, at(1)).expect("a direct id makes a direct group");

    assert_eq!(group.members.len(), 2);
    assert_eq!(group.role_of(&a.public_key()), Some(Role::Admin));
    assert_eq!(group.role_of(&b.public_key()), Some(Role::Admin));
    assert_eq!(group.access, Access::direct());

    for event in [
        moderation(&intruder, Kind::GroupJoinRequest, &id, Vec::new()),
        moderation(&a, Kind::GroupPutUser, &id, vec![p(&intruder.public_key())]),
        moderation(&a, Kind::GroupRemoveUser, &id, vec![p(&b.public_key())]),
        moderation(&a, Kind::GroupLeaveRequest, &id, Vec::new()),
    ] {
        let error = apply(&mut group, &event).expect_err("a direct group has two members");
        assert!(error.to_string().contains("always will"), "{error}");
    }
    assert_eq!(group.members.len(), 2);
}

#[test]
fn a_direct_group_is_readable_by_anybody() {
    // The one that a person has to know about: with nothing proving who is
    // asking, a direct message is a grouping and not a privacy boundary.
    let a = Keys::generate();
    let b = Keys::generate();
    let stranger = Keys::generate();
    let id = direct_id(&a.public_key(), &b.public_key());
    let group = Group::direct(&id, at(1)).expect("a direct group");

    assert!(group.may_read(Some(&stranger.public_key())));
    assert!(group.may_read(None));
    assert!(matches!(
        group.may_publish(&stranger.public_key(), chat::CHAT),
        Verdict::Refuse(..)
    ));
}

#[test]
fn the_metadata_events_say_what_the_group_is() {
    let relay = Keys::generate();
    let owner = Keys::generate();
    let member = Keys::generate();
    let mut group = Group::create(id("general"), owner.public_key(), at(1));
    group.members.insert(member.public_key(), Role::Member);
    group.name = Some("General".to_owned());
    group.about = Some("Everything else.".to_owned());

    let signed = |builder: EventBuilder| builder.finalize(&relay).expect("the relay signs");

    let metadata = signed(group::metadata(&group));
    let read = group::read_metadata(&metadata).expect("a 39000 reads back");
    assert_eq!(read.id, group.id);
    assert_eq!(read.name.as_deref(), Some("General"));
    assert_eq!(read.about.as_deref(), Some("Everything else."));
    assert_eq!(read.access, group.access);

    let members = signed(group::members(&group));
    let (named, everybody) = group::read_members(&members).expect("a 39002 reads back");
    assert_eq!(named, group.id);
    assert_eq!(everybody.len(), 2);
    assert!(everybody.contains(&owner.public_key()));
    assert!(everybody.contains(&member.public_key()));

    let admins = signed(group::admins(&group));
    let (named, admins) = group::read_admins(&admins).expect("a 39001 reads back");
    assert_eq!(named, group.id);
    assert_eq!(
        admins,
        vec![Member {
            pubkey: owner.public_key(),
            role: Role::Admin
        }]
    );

    let roles = signed(group::roles(&group));
    assert_eq!(roles.kind, Kind::GroupRoles);
    assert_eq!(roles.tags.identifier().as_deref(), Some(group.id.as_str()));
}

#[test]
fn the_metadata_is_a_function_of_the_group_alone() {
    // A relay that restarts rebuilds its groups and re-signs their metadata.
    // If the events were not deterministic it would write four new ones for
    // every group at every start-up, and every client would redraw.
    let relay = Keys::generate();
    let owner = Keys::generate();
    let group = Group::create(id("general"), owner.public_key(), at(1));

    let once = group::metadata(&group).finalize(&relay).expect("signs");
    let twice = group::metadata(&group).finalize(&relay).expect("signs");
    assert_eq!(once.id, twice.id, "the same group signs the same metadata");
}

#[test]
fn a_log_replayed_twice_gives_the_same_group() {
    let owner = Keys::generate();
    let member = Keys::generate();
    let guest = Keys::generate();
    let group = id("general");

    let log = vec![
        moderation(
            &owner,
            Kind::GroupPutUser,
            &group,
            vec![p_as(&member.public_key(), Role::Admin)],
        ),
        moderation(
            &owner,
            Kind::GroupCreateInvite,
            &group,
            vec![Tag::custom("code", ["let-me-in-please".to_owned()])],
        ),
        moderation(
            &guest,
            Kind::GroupJoinRequest,
            &group,
            vec![Tag::custom("code", ["let-me-in-please".to_owned()])],
        ),
        moderation(
            &member,
            Kind::GroupRemoveUser,
            &group,
            vec![p(&guest.public_key())],
        ),
        moderation(
            &owner,
            Kind::GroupEditMetadata,
            &group,
            vec![Tag::custom("name", ["General".to_owned()])],
        ),
    ];

    let replay = || {
        let mut state = Group::create(group.clone(), owner.public_key(), at(1));
        for event in &log {
            // Some of these are refused on purpose in a later replay; the point
            // is that the same log gives the same answer every time.
            let _ = apply(&mut state, event);
        }
        state
    };

    assert_eq!(replay(), replay());
    let state = replay();
    assert_eq!(state.name.as_deref(), Some("General"));
    assert!(state.is_member(&member.public_key()));
    assert!(!state.is_member(&guest.public_key()));
    // The group is open, so the guest was let in without the code and the code
    // is still there to spend. A join only spends one when it needed one.
    assert_eq!(state.invites.len(), 1);
}

#[test]
fn a_moderation_event_with_no_group_is_refused() {
    let owner = Keys::generate();
    let orphan = EventBuilder::new(Kind::GroupJoinRequest, "")
        .finalize(&owner)
        .expect("signs");
    let error = Action::read(&orphan).expect_err("a group event names its group");
    assert!(error.to_string().contains("an h tag"), "{error}");
}

#[test]
fn something_said_is_not_a_moderation_event() {
    let keys = Keys::generate();
    let said = chat::message(&id("general"), "morning", &[], &[])
        .finalize(&keys)
        .expect("signs");
    assert!(Action::read(&said).expect("readable").is_none());
    assert_eq!(chat::group_of(&said), Some("general"));
    assert!(group::is_carried(said.kind));
    assert!(!group::is_moderation(said.kind));
}

#[test]
fn a_mention_is_what_names_somebody() {
    let keys = Keys::generate();
    let other = Keys::generate();
    let said = chat::message(&id("general"), "morning", &[other.public_key()], &[])
        .finalize(&keys)
        .expect("signs");

    assert!(chat::mentions_key(&said, &other.public_key()));
    assert!(!chat::mentions_key(&said, &keys.public_key()));
}

#[test]
fn previous_carries_the_head_of_each_id() {
    let keys = Keys::generate();
    let earlier = chat::message(&id("general"), "first", &[], &[])
        .finalize(&keys)
        .expect("signs");
    let later = chat::message(&id("general"), "second", &[], &[earlier.id])
        .finalize(&keys)
        .expect("signs");

    let heads = chat::read_previous(&later);
    assert_eq!(heads, vec![&earlier.id.to_hex()[..8]]);
    assert!(
        chat::read_previous(&earlier).is_empty(),
        "the first message in a group has nothing before it"
    );
}
