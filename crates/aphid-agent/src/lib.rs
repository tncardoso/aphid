//! The agent loop, tool execution, and the composition runtime for the aphid
//! harness.
//!
//! [`aphid_core`] gives you a conversation and one streamed response. This crate
//! turns that into an agent: [`Agent::prompt`] runs *request → stream → commit →
//! execute tools* until the model stops asking for tools.
//!
//! # Composition
//!
//! Everything interesting is interceptable. A [`Component`](rt::Component)
//! declares the services it needs, contributes tools, and subscribes to what
//! the loop announces: a prompt on its way in, a request about to be sent, a
//! tool call that could be refused or rewritten, a result that could be
//! patched, a turn that could end the run.
//!
//! Nothing is ordered by hand. A component waits until what it declared is
//! available, loads, and unloads again if any of it goes away — and everything
//! it registered leaves with it, because every registration carries its
//! inverse. See [`rt`].
//!
//! Announcements are **synchronous** and their payloads own their data, so a
//! listener may keep one, move it to another thread, or answer from a task.
//! The one exception is the per-token stream, which hands out a borrow into the
//! response arena and is documented where it lives:
//! [`StreamListeners`].
//!
//! # Ordering
//!
//! Tool results are committed in assistant source order, never completion
//! order, however the batch was scheduled. Providers match results to calls
//! positionally, so scheduling must not leak into the transcript.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//!
//! use aphid_agent::rt::{Component, Composition, Context};
//! use aphid_agent::testing::{Turn, scripted};
//! use aphid_agent::{Agent, Blocked, ToolOutcome, ToolRequest, tool_fn};
//! use aphid_core::{Role, providers::deepseek};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct Weather {
//!     city: String,
//! }
//!
//! // A component that vetoes one city.
//! struct NoLisbon {
//!     composition: Composition,
//! }
//!
//! impl Component for NoLisbon {
//!     fn name(&self) -> &str {
//!         "no-lisbon"
//!     }
//!
//!     fn apply(&self, ctx: &Context) -> Result<(), String> {
//!         self.composition.bus.on::<ToolRequest>(ctx.uid(), |request| {
//!             if request.arguments.contains("Lisbon") {
//!                 request.refuse(Blocked::new("Lisbon is off limits."));
//!             }
//!         });
//!         Ok(())
//!     }
//! }
//!
//! # async fn run() {
//! // A scripted backend: one tool call, then an answer. No network.
//! let (backend, _script) = scripted([
//!     Turn::call("call_1", "get_weather", r#"{"city":"Porto"}"#),
//!     Turn::text("It is sunny in Porto."),
//! ]);
//!
//! let composition = Composition::new();
//! composition
//!     .plug(NoLisbon { composition: composition.clone() })
//!     .await
//!     .expect("no dependencies, no schema");
//!
//! let mut agent = Agent::builder()
//!     .model(deepseek::flash())
//!     .system("You are terse.")
//!     .tool(tool_fn(
//!         "get_weather",
//!         "Look up the current weather for a city.",
//!         serde_json::json!({
//!             "type": "object",
//!             "properties": { "city": { "type": "string" } },
//!             "required": ["city"]
//!         }),
//!         |args: Weather, _cx| async move {
//!             ToolOutcome::text(format!("sunny in {}", args.city))
//!         },
//!     ))
//!     .compose(&composition)
//!     .stream_fn(backend)
//!     .build();
//!
//! let outcome = agent.prompt("weather in Porto?").await;
//!
//! assert_eq!(outcome.turns, 2);
//! // system, user, assistant (tool call), tool result, assistant (answer)
//! assert_eq!(agent.transcript().len(), 5);
//! assert_eq!(agent.transcript().get(3).unwrap().role(), Role::ToolResult);
//! # }
//! ```

mod agent;
mod events;
pub mod exec;
mod plugin;
mod registry;
pub mod rt;
mod run;
mod sink;
mod stream;
pub mod testing;
mod tool;
mod toolbox;

pub use agent::{
    Agent, AgentBuilder, AgentConfig, AgentHandle, DEFAULT_MAX_TURNS, NoModel, RunOutcome,
    create_agent,
};
pub use events::{
    AGENT_EVENTS, Blocked, Edit, Message, Moment, Prompt, Run, RunEnd, RunStart, StreamListeners,
    ToolArguments, ToolProgress, ToolRequest, ToolResult, TranscriptListeners, TurnEnd, TurnStart,
};
pub use plugin::{StreamCx, TurnSummary};
pub use registry::Tools;
pub use sink::{Silent, Sink};
pub use stream::{Backend, BoxStream, DynAssistantStream, Live, StreamFn, live_stream_fn};
pub use tool::{
    BoxFuture, Execution, FnTool, ProgressSink, ToolCall, ToolContent, ToolCx, ToolHandler,
    ToolOutcome, tool_fn,
};
pub use toolbox::Toolbox;
