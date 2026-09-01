//! Rhai script hosting for the coding harness.
//!
//! A plugin is one `.rhai` file. It declares what it needs, and contributes
//! what it has from a single `apply` function:
//!
//! ```rhai
//! const inject = ["commands"];
//!
//! fn apply(ctx) {
//!     command(#{ name: "review", description: "…", run: |args| { … } });
//!     on("agent/tool-call", |tool| { … });
//! }
//! ```
//!
//! Everything registered there is taken back when the plugin unloads, and a
//! plugin whose `inject` is not satisfied never runs at all — so a command that
//! is listed is a command that works.
//!
//! ```text
//! .aphid/plugins/<name>.rhai
//! .aphid/plugins/<name>/main.rhai
//! ```
//!
//! Searched in the workspace and then under `~/.aphid`, project first.
//!
//! ```rhai
//! //! Keeps the model away from the release notes.
//!
//! fn apply(ctx) {
//!     on("agent/tool-call", |tool| {
//!         if tool.name == "write" && tool.arguments.contains("CHANGELOG") {
//!             return block("the changelog is written by hand");
//!         }
//!     });
//!
//!     on("agent/turn-end", |cx, turn| {
//!         if turn.stop_reason == "length" {
//!             cx.note("the last response was cut short");
//!         }
//!     });
//! }
//! ```
//!
//! `call` is a reserved word in Rhai, so a listener parameter cannot use that
//! name — and reaching a service is `invoke`, not `call`.
//!
//! # The shape of a listener
//!
//! Rhai passes arguments by value, so a script cannot change a payload by
//! mutating it. Each listener is handed a map and steers the run by what it
//! **returns**: unit changes nothing, `block("…")` and `stop()` and `reject("…")`
//! are verdicts, and a map patches named fields. `cx` is the exception — it
//! carries a handle, so `cx.note(…)` works whatever Rhai does with the value.
//!
//! # Capabilities
//!
//! A script can compute and nothing more until the host grants otherwise. See
//! [`Capabilities`]: `fs_read`, `fs_write`, `fs_exists`, `fs_list` (confined to
//! one directory), `exec`, `http_get`, `http_post`, plus `log` and `notify`,
//! which are always available.
//!
//! Output goes to the `sink` service, whose trait lives in [`aphid_agent`]
//! because a Rust component that never touches a script still wants one.

mod caps;
mod command;
mod component;
mod convert;
mod cx;
mod discover;
mod entries;
mod facade;
mod host;
pub mod hub;
mod script;
mod store;
mod subscribe;
mod surface;
mod tool;
pub mod trust;
mod widget;
mod wiring;
mod worker;

pub use caps::{Capabilities, DEFAULT_MAX_OPERATIONS, DEFAULT_TIMEOUT, resolve};
pub use command::{Action, CommandSpec, Registered, registered as registered_commands};
pub use component::ScriptComponent;
pub use convert::{object_to_map, to_dynamic, to_json};
pub use cx::ScriptCx;
pub use discover::{DIR_NAME, Diagnostic, ENTRY_FILE, EXTENSION, PluginFile, discover, explicit};
pub use entries::{IsolateSpec, Row, Scripts, compose, read};
pub use facade::{Facade, ScriptService};
pub use host::{PluginHost, ScriptHost, silent_sink};
pub use hub::{Job, Open, PluginHub, Report};
pub use script::{Declares, ScriptPlugin};
pub use store::Store;
pub use surface::{
    Host, Placement, RegisteredSurface, Side, SurfaceAction, SurfaceEvent, SurfaceRender,
    SurfaceSpec, registered as registered_surfaces, ticking as ticking_surfaces,
};
pub use tool::{ScriptTool, ToolSpec};
pub use widget::Widget;
pub use worker::Worker;
