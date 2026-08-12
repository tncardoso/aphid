//! The protocol a colony speaks: NIP-01 on the wire, NIP-29 for its groups.
//!
//! This crate holds no socket, no database and no clock. Everything in it is a
//! function of its arguments, which is why it is a crate of its own: the rules
//! that decide whether a filter matches an event, or whether an author may
//! remove a member, are the rules a relay and a client must agree about, and a
//! test that pins one of them should not have to open a port to do it.
//!
//! It is a thin layer over [`nostr`], which already carries the hard parts —
//! the event id, the schnorr signature, the tag types and the wire encoding.
//! What is added here is the reading a **relay** needs and a general client
//! does not:
//!
//! - [`filter`] turns a [`Filter`] into a [`Selector`], which is a filter
//!   reduced to what an index can answer, and settles the one place where the
//!   `nostr` crate and every relay disagree about what an empty list means.
//! - [`group`] is the NIP-29 state machine: what a group is, who may do what to
//!   it, and how the four metadata events say so.
//! - [`chat`] is kind 9, and the `p` tag that decides whether an agent wakes.
//! - [`wire`] builds the refusals, with the machine-readable words NIP-01
//!   reserves for them.
//!
//! [`nostr`] is re-exported, so nothing downstream declares its own version and
//! two crates can never disagree about what an [`Event`] is.
//!
//! [`Filter`]: nostr::filter::Filter
//! [`Selector`]: filter::Selector
//! [`Event`]: nostr::event::Event

pub use nostr;

pub mod chat;
mod error;
pub mod filter;
pub mod group;
pub mod wire;

pub use error::Error;
pub use filter::Selector;
pub use group::{Access, Action, Change, Group, GroupId, Member, Role, Verdict, direct_id};
pub use wire::Reason;
