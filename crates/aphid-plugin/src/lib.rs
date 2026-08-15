//! Rhai plugins for the aphid harness.
//!
//! A plugin is one `.rhai` file that defines functions with known names. There
//! is no manifest and no registration call: the host compiles the file, reads
//! the hooks out of the AST, and subscribes to exactly those.
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
//! fn on_tool_call(tool) {
//!     if tool.name == "write" && tool.arguments.contains("CHANGELOG") {
//!         return block("the changelog is written by hand");
//!     }
//! }
//!
//! fn on_turn_end(cx, turn) {
//!     if turn.stop_reason == "length" {
//!         cx.note("the last response was cut short");
//!     }
//! }
//! ```
//!
//! `call` is a reserved word in Rhai, so a hook parameter cannot use that name.
//!
//! # The shape of a hook
//!
//! Rhai passes arguments by value, so a script cannot change a payload by
//! mutating it. Each hook is handed a map and steers the run by what it
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

mod backend;
mod caps;
mod code;
mod command;
mod convert;
mod cx;
mod discover;
mod host;
mod script;
mod store;
mod surface;
mod tool;
pub mod trust;
mod widget;
mod worker;

pub use backend::ScriptBackend;
pub use caps::{Capabilities, DEFAULT_MAX_OPERATIONS, DEFAULT_TIMEOUT, Silent, Sink, resolve};
pub use code::{Change, Permission, SessionInfo};
pub use command::{Action, CommandSpec};
pub use convert::{object_to_map, to_dynamic, to_json};
pub use cx::ScriptCx;
pub use discover::{DIR_NAME, Diagnostic, ENTRY_FILE, EXTENSION, PluginFile, discover, explicit};
pub use host::{PluginHost, silent_sink};
pub use script::ScriptPlugin;
pub use store::Store;
pub use surface::{
    Placement, RegisteredSurface, Side, SurfaceAction, SurfaceEvent, SurfaceRender, SurfaceSpec,
};
pub use tool::{ScriptTool, ToolSpec};
pub use widget::Widget;
pub use worker::Worker;
