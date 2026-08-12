//! What somebody said in a group.
//!
//! Kind 9 and not kind 1. A group message is not a note: a client that follows
//! the author must not draw it on a timeline, and a relay that carries groups
//! should not have to guess which of an author's notes belong to which group.
//!
//! Everything else here is a tag. The `h` says which group, the `p`s say who is
//! being spoken to — and a `p` is the whole of a colony's wake policy, because
//! an agent runs when it is named and not when the room is busy.

use nostr::event::{Event, EventBuilder, EventId, Kind, Tag};
use nostr::key::PublicKey;

use crate::group::GroupId;

/// The kind a group message is.
pub const CHAT: Kind = Kind::Custom(9);

/// How many ids a `previous` tag carries, and how much of each.
///
/// NIP-29 says up to fifty, and the first eight characters of each.
const PREVIOUS: usize = 50;
const PREFIX: usize = 8;

/// The group an event belongs to: its `h` tag.
#[must_use]
pub fn group_of(event: &Event) -> Option<&str> {
    event
        .tags
        .iter()
        .find(|tag| tag.kind() == "h")
        .and_then(|tag| tag.content())
}

/// The `h` tag naming a group.
#[must_use]
pub fn h(id: &GroupId) -> Tag {
    Tag::custom("h", [id.as_str().to_owned()])
}

/// Everybody the message names.
pub fn mentions(event: &Event) -> impl Iterator<Item = PublicKey> + '_ {
    event.tags.public_keys()
}

/// Whether the message names this key.
#[must_use]
pub fn mentions_key(event: &Event, who: &PublicKey) -> bool {
    mentions(event).any(|mentioned| mentioned == *who)
}

/// The `previous` tag: the head of the ids this author last saw in this group.
///
/// Written, and never checked. See the [`group`] module documentation.
///
/// [`group`]: crate::group
#[must_use]
pub fn previous(ids: &[EventId]) -> Tag {
    let heads = ids
        .iter()
        .take(PREVIOUS)
        .map(|id| id.to_hex()[..PREFIX].to_owned());
    Tag::custom("previous", heads)
}

/// The heads a `previous` tag carries.
#[must_use]
pub fn read_previous(event: &Event) -> Vec<&str> {
    event
        .tags
        .iter()
        .find(|tag| tag.kind() == "previous")
        .map(|tag| tag.as_slice()[1..].iter().map(String::as_str).collect())
        .unwrap_or_default()
}

/// One chat message, ready to sign.
///
/// `previous` is left off when there is nothing to put in it, which is what the
/// first message in a group has.
#[must_use]
pub fn message(
    group: &GroupId,
    text: &str,
    mentions: &[PublicKey],
    seen: &[EventId],
) -> EventBuilder {
    let mut tags = vec![h(group)];
    tags.extend(mentions.iter().map(|who| Tag::public_key(*who)));
    if !seen.is_empty() {
        tags.push(previous(seen));
    }
    EventBuilder::new(CHAT, text).tags(tags)
}
