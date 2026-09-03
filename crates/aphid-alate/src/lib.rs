//! A resident aphid agent.
//!
//! An alate is the winged aphid, the morph that leaves the plant and lives away
//! from it. Where [`aphid_code`] builds an agent around a git workspace and lets
//! it end with the terminal, this one lives on its own: it has a home directory
//! it owns, a memory that outlasts any session, a clock that wakes it, and a
//! socket that anything can attach to.
//!
//! # The pieces
//!
//! - [`home`] — one directory per named instance, which is also the agent's
//!   workspace.
//! - [`config`] — `alate.json` inside it.
//! - [`memory`] — what the agent knows, as markdown files it owns.
//! - [`heartbeat`] — when the agent wakes itself.
//! - [`gateway`] — the socket the daemon listens on and clients attach to.
//! - [`daemon`] — the loop that ties them together.
//! - [`tui`] — the first client: a terminal attached to a running instance.
//! - `telegram` — a second client, behind the `telegram` feature: a bot that
//!   puts a chat on the same socket.
//! - `voice` — behind the `voice` feature: speech to text, so a client that
//!   carries audio can hand the agent words.
//!
//! The agent itself is still assembled by [`aphid_code::harness::build`]. This
//! crate adds what a resident agent needs around it and changes nothing about
//! how an agent is built.

#[cfg(feature = "colony")]
pub mod colony;
pub mod config;
pub mod cron;
pub mod daemon;
pub mod gateway;
#[cfg(feature = "gui")]
pub mod gui;
pub mod heartbeat;
pub mod home;
pub mod memory;
pub mod prompts;
pub mod sandbox;
pub mod sessions;
#[cfg(feature = "telegram")]
pub mod telegram;
pub mod tui;
#[cfg(feature = "voice")]
pub mod voice;

pub use config::Config;
pub use home::Home;
pub use memory::Memory;
