//! The terminal UI.
//!
//! Three regions in an alternate screen: a scrollable transcript, an input line
//! and a status line.
//!
//! # How it fits together
//!
//! [`runtime`] holds the loop all three aphid terminals share: a model, one
//! message type, one place that changes the model, and one place that does
//! anything the model asked for. It knows nothing about agents or plugins,
//! which is what makes it shareable.
//!
//! The agent runs on **its own task**, not inside the loop. That is what lets a
//! permission prompt block the agent while the UI keeps drawing —
//! [`UiConfirmer`](event::UiConfirmer) blocks on a channel until the app answers,
//! and if both lived on one task that would deadlock. It also means the UI never
//! touches the agent while a run is in flight: everything it draws comes from
//! [`UiEvent`](event::UiEvent)s, and the agent comes back when the run ends.

pub mod app;
pub mod event;
pub mod input;
pub mod logo;
pub mod modal;
pub mod runtime;
pub mod scrollback;
pub mod status;
pub mod surface;

pub use app::{App, run};
pub use event::{UiConfirmer, UiEvent, UiPlugin, UiSink};
pub use scrollback::Scrollback;
pub use status::Status;
