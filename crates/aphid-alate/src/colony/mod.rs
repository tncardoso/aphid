//! A colony on the gateway.
//!
//! The bridge is a **client**, not a second door — the position
//! [`crate::telegram`] takes, for the same reasons. It attaches to the same
//! socket a terminal does, one connection for each group, and speaks the same
//! [`wire`] to it. So nothing in the daemon knows a colony exists: a channel
//! gets an ordinary [`Kind::Attached`] session, two channels are kept apart by
//! the fan-out that already keeps two terminals apart, and a permission answer
//! is the [`Request::Answer`] that was always there.
//!
//! ```text
//! colony ──EVENT──►  bridge ──┬─► group ──Client──► gateway.sock ──► daemon
//!        ◄─EVENT───  publish  └─◄ frames ─────────────────────────
//! ```
//!
//! # What wakes the agent
//!
//! A mention, and a direct message. Nothing else. Everything said in a channel
//! is stored by the colony and readable with `colony_read` whenever the agent
//! wants it, but an agent that woke on every line in a busy channel would never
//! stop running and would pay for a turn for every word anybody said.
//!
//! A message that does not wake it is **dropped**, not queued. The colony is
//! the log; a queue here would be a second one that can disagree with it.
//!
//! # What goes back
//!
//! Only what `colony_send` sends. A turn that ends says nothing, which means a
//! model that writes prose and forgets the tool is silent in the colony — so
//! the system prompt says so in as many words, and the prose is still there in
//! `aphid alate attach` for a person to see.
//!
//! [`wire`]: crate::gateway::wire
//! [`Kind::Attached`]: crate::sessions::Kind::Attached

mod bridge;
mod chat;
pub mod relay;
pub mod tools;

pub use bridge::{Bridge, Connect, ConnectFuture, spawn};
pub use relay::{Ask, Live, Next, Relay, RelayFn, keys};
pub use tools::{Colony, ColonyComponent, Directory, Outbound, Shared, read_tool, send_tool};
