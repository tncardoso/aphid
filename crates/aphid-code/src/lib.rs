//! A coding harness built on [`aphid_agent`].
//!
//! Where `aphid-agent` is deliberately unopinionated — a loop, a tool registry, a
//! plugin API — this crate is the specialization: the tools a coding agent needs,
//! a system prompt assembled from the project's own conventions, sessions that
//! survive a restart, and a terminal UI.
//!
//! ```no_run
//! use aphid_code::{Workspace, harness::{self, HarnessOptions}};
//!
//! # async fn run() {
//! let mut options = HarnessOptions::new(Workspace::discover());
//! options.api_key = std::env::var("DEEPSEEK_API_KEY").ok().map(Into::into);
//!
//! let mut harness = harness::build(options);
//! let outcome = harness.agent.prompt("what does this crate do?").await;
//! println!("{} turns, ${:.4}", outcome.turns, outcome.usage.cost.total);
//! # }
//! ```

pub mod context;
pub mod harness;
pub mod headless;
pub mod model;
pub mod plugins;
pub mod prompt;
pub mod session;
pub mod skills;
pub mod tools;
pub mod tui;

pub use context::home_dir;
pub use harness::{Harness, HarnessOptions};
pub use model::Catalog;
pub use skills::Skill;
pub use tools::Workspace;
