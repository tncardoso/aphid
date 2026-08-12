//! The four events the relay signs about a group.
//!
//! NIP-29 makes the relay the authority: a group is what the relay says it is,
//! and it says so in kinds 39000 to 39003, addressable on the group id. Nobody
//! else may sign one, and a client that wants to change a group asks with a
//! moderation event and waits to see the metadata change.
//!
//! These are builders and not events, because signing needs a key and this
//! crate holds none. They carry the group's `changed_at` as their `created_at`,
//! which makes each one a function of the group alone — so a relay that
//! restarts and rebuilds its state signs exactly what is already stored, and
//! therefore signs nothing.

use nostr::event::{Event, EventBuilder, Kind, Tag};
use nostr::key::PublicKey;

use super::id::GroupId;
use super::state::{Access, Group, Member, Role};
use crate::{Error, chat};

/// What a 39000 says about a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub id: GroupId,
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub access: Access,
}

/// Kind 39000: what the group is called and who may get in.
#[must_use]
pub fn metadata(group: &Group) -> EventBuilder {
    let mut tags = vec![Tag::identifier(group.id.as_str())];
    for (word, value) in [
        ("name", &group.name),
        ("about", &group.about),
        ("picture", &group.picture),
    ] {
        if let Some(value) = value {
            tags.push(Tag::custom(word, [value.clone()]));
        }
    }
    tags.extend(group.access.tags());
    tags.push(Tag::custom(
        "supported_kinds",
        [chat::CHAT.as_u16().to_string()],
    ));

    EventBuilder::new(Kind::GroupMetadata, "")
        .tags(tags)
        .custom_created_at(group.changed_at)
}

/// Kind 39001: the admins, with their roles.
#[must_use]
pub fn admins(group: &Group) -> EventBuilder {
    let tags = std::iter::once(Tag::identifier(group.id.as_str()))
        .chain(
            group
                .members
                .iter()
                .filter(|(_, role)| role.moderates())
                .map(|(pubkey, role)| Tag::custom("p", [pubkey.to_hex(), role.name().to_owned()])),
        )
        .collect::<Vec<_>>();

    EventBuilder::new(Kind::GroupAdmins, "")
        .tags(tags)
        .custom_created_at(group.changed_at)
}

/// Kind 39002: everybody in the group, admins included.
#[must_use]
pub fn members(group: &Group) -> EventBuilder {
    let tags = std::iter::once(Tag::identifier(group.id.as_str()))
        .chain(group.members.keys().map(|pubkey| Tag::public_key(*pubkey)))
        .collect::<Vec<_>>();

    EventBuilder::new(Kind::GroupMembers, "")
        .tags(tags)
        .custom_created_at(group.changed_at)
}

/// Kind 39003: the roles this group knows.
///
/// A colony declares the same two everywhere, so this never changes after the
/// group is made and carries the group's `created_at`.
#[must_use]
pub fn roles(group: &Group) -> EventBuilder {
    let tags = std::iter::once(Tag::identifier(group.id.as_str()))
        .chain([Role::Member, Role::Admin].into_iter().map(|role| {
            Tag::custom(
                "role",
                [role.name().to_owned(), role.description().to_owned()],
            )
        }))
        .collect::<Vec<_>>();

    EventBuilder::new(Kind::GroupRoles, "")
        .tags(tags)
        .custom_created_at(group.created_at)
}

/// Read a 39000 back.
///
/// The relay never needs this — it holds the group it signed about. A client
/// does, and the colony's terminal is a client.
///
/// # Errors
///
/// Fails when the event carries no `d` tag, or one that is not a group id.
pub fn read_metadata(event: &Event) -> Result<Metadata, Error> {
    let id = identifier(event)?;
    let word = |name: &str| {
        event
            .tags
            .iter()
            .find(|tag| tag.kind() == name)
            .and_then(|tag| tag.content())
            .map(str::to_owned)
    };

    Ok(Metadata {
        id,
        name: word("name"),
        about: word("about"),
        picture: word("picture"),
        access: Access::read(&event.tags),
    })
}

/// Read a 39002 back: the group, and everybody in it.
///
/// # Errors
///
/// Fails when the event carries no `d` tag, or one that is not a group id.
pub fn read_members(event: &Event) -> Result<(GroupId, Vec<PublicKey>), Error> {
    let id = identifier(event)?;
    Ok((id, event.tags.public_keys().collect()))
}

/// Read a 39001 back: the group, and its admins with their roles.
///
/// # Errors
///
/// Fails when the event carries no `d` tag, or one that is not a group id.
pub fn read_admins(event: &Event) -> Result<(GroupId, Vec<Member>), Error> {
    let id = identifier(event)?;
    let admins = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == "p")
        .filter_map(|tag| {
            let mut values = tag.as_slice().iter().skip(1);
            let pubkey = PublicKey::from_hex(values.next()?).ok()?;
            let role = values
                .filter_map(|name| Role::parse(name))
                .max()
                .unwrap_or(Role::Admin);
            Some(Member { pubkey, role })
        })
        .collect();

    Ok((id, admins))
}

/// The group an addressable metadata event is about.
fn identifier(event: &Event) -> Result<GroupId, Error> {
    let id = event.tags.identifier().ok_or(Error::Missing {
        kind: "group metadata event",
        want: "a d tag naming its group",
    })?;
    GroupId::parse(&id)
}
