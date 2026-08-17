//! The loop every aphid terminal runs on.
//!
//! One model, one message type, one place that changes the model. A program
//! answers three questions and nothing else:
//!
//! - [`Program::update`] — what this message does to the model. It is pure: no
//!   IO, no script, no task, no terminal. Whatever it wants done comes back as
//!   a [`Cmd`], which is plain data a test can read.
//! - [`Draw::draw`] — what the model looks like. It takes `&self` and a
//!   scratchpad the runtime owns, so drawing cannot change what is drawn.
//! - [`Effects::perform`] — how one thing gets done. This is where the agent,
//!   the plugin host, the processes and the terminal live, because none of
//!   them belong in a model.
//!
//! Nothing here knows about agents, plugins or sessions. That is what lets a
//! headless driver implement [`Program`] and not [`Draw`], and what lets three
//! different terminals share one loop.
//!
//! # Why the effects are data
//!
//! A boxed future would be more flexible and much less testable: a test could
//! not ask what an update decided without running the decision, and running it
//! means bringing back the agent and the terminal that the split was there to
//! remove. An effect enum lets a test say
//!
//! ```ignore
//! assert_eq!(model.update(msg).effects(), [Effect::StartRun("hello".into())]);
//! ```
//!
//! which is the whole point.

mod answers;
mod cmd;
mod driver;
mod hub;
mod keys;
mod program;
mod terminal;

pub use answers::{ANSWER_TIMEOUT, Answers, RequestId};
pub use cmd::Cmd;
pub use driver::run;
pub use hub::{Hub, channel};
pub use keys::spawn_input_thread;
pub use program::{Draw, Effects, Program, Subs, Timer};
pub use terminal::{Tty, restore, setup};
