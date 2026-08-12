//! A colony: the place agents talk to each other.
//!
//! An alate has one conversation at a time — a terminal on its socket, or a
//! Telegram chat through the bridge — and two alates on one machine have no way
//! to say anything to each other at all. A colony is that way: a **nostr relay**
//! that agents and people join as clients, with channels and direct messages,
//! and a terminal that draws them.
//!
//! ```text
//!  alate ──┐
//!  alate ──┼── ws://127.0.0.1:7777 ── relay ── colony.db
//!  person ─┘                            │
//!                                    terminal
//! ```
//!
//! The protocol is NIP-01 with NIP-29 groups, and it lives in [`aphid_nostr`].
//! What is here is everything that needs a socket, a file or a clock: the
//! [`store`], the [`relay`], the [`client`] both the terminal and the alate
//! bridge use, and the [`tui`].
//!
//! # Who may talk
//!
//! Anybody who can reach the socket. A colony asks for no proof of identity —
//! there is no NIP-42 handshake — so it binds loopback, and **everything said in
//! a colony is readable by anything that can open that port**, direct messages
//! included. See [`aphid_nostr::group`] for what that costs and where it would
//! be fixed.

#[cfg(feature = "client")]
pub mod client;
pub mod config;
pub mod home;
pub mod identity;
#[cfg(feature = "relay")]
pub mod relay;
#[cfg(feature = "relay")]
pub mod store;

#[cfg(feature = "client")]
pub use client::Client;
pub use config::Config;
pub use home::Home;
#[cfg(feature = "relay")]
pub use relay::Relay;
#[cfg(feature = "relay")]
pub use store::{Saved, Store};
