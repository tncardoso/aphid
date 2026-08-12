//! NIP-29 groups: what one is, who may change it, and how the relay says so.
//!
//! In NIP-29 the **relay is the authority**. A group is not a thing its members
//! agree about; it is a thing the relay asserts, in four addressable events it
//! signs with its own key. A participant asks for a change with a moderation
//! event and learns the answer by seeing the metadata change, or not.
//!
//! That is why this module has no signing in it. The rules are here, in one
//! fold over one log — [`Group::apply`] — and the key that turns a group into
//! events lives in the relay.
//!
//! # What a colony implements
//!
//! The `h` tag on every group event, and a refusal for one without it. Kinds
//! 39000, 39001, 39002 and 39003, signed by the relay and addressable on the
//! group id. Kind 9 chat. Moderation kinds 9000 put-user, 9001 remove-user,
//! 9002 edit-metadata, 9007 create-group, 9008 delete-group, 9009 create-invite,
//! 9021 join-request and 9022 leave-request. The `public`/`private` and
//! `open`/`closed` words. "Only members may post". The `previous` tag, written.
//!
//! # What it does not, and why
//!
//! **NIP-42 AUTH.** A colony asks nobody to prove who they are, so the relay
//! does not know who is asking on a `REQ`. That is one decision with one large
//! consequence: no group can honestly be `private`, so every group is public,
//! so **a direct message is world-readable**. [`Group::may_read`] is the one
//! function that would change if AUTH ever arrived.
//!
//! **Checking `previous`.** NIP-29 lets a relay refuse a message whose
//! `previous` does not line up, and does not say what lining up means or what to
//! do about a fork. With one relay there are no forks to heal, so a check buys
//! nothing and a wrong one drops messages in silence. It is written, so that a
//! future relay could start checking it.
//!
//! **9005 delete-event, and NIP-09.** Nothing in a colony deletes. A kind 5 is
//! stored like any other event and is not acted on: an agent able to erase what
//! it said is a debugging hazard.
//!
//! **Kinds 11 and 12, threads.** A Slack-like log is flat. Threads are a second
//! layout, a second unread count and a second read tool.
//!
//! **Relay hints in an `h` tag, and kind 39004.** One relay, and no audio.

mod action;
mod id;
mod meta;
mod state;

pub use action::Action;
pub use id::{GroupId, direct_id};
pub use meta::{
    Metadata, admins, members, metadata, read_admins, read_members, read_metadata, roles,
};
pub use state::{Access, Change, Group, Member, Role, Verdict, is_carried, is_moderation};
