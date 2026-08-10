//! The agent loop, tool execution, and the plugin API for the aphid harness.
//!
//! [`aphid_core`] gives you a conversation and one streamed response. This crate
//! turns that into an agent: [`Agent::prompt`] runs *request → stream → commit →
//! execute tools* until the model stops asking for tools.
//!
//! # The plugin API
//!
//! Everything interesting is interceptable. A [`Plugin`] can contribute tools,
//! add context before a request, watch every protocol event, block or rewrite a
//! tool call, patch a tool result, and stop the run.
//!
//! Hooks are **synchronous**. The only per-token hook is [`Plugin::on_event`],
//! and boxing a future for each of those would undo the point of the core's
//! arena layout. Anything that must await belongs in a [`ToolHandler`], the one
//! async point in the surface. Plugins declare an [`Interest`] set, and the
//! registry turns those declarations into one subscriber list per hook, so a
//! hook nobody wants costs an empty-slice check.
//!
//! # Ordering
//!
//! Tool results are committed in assistant source order, never completion order,
//! however the batch was scheduled. Providers match results to calls
//! positionally, so scheduling must not leak into the transcript.
//!
//! # Example
//!
//! ```
//! use aphid_agent::{Agent, Guard, PendingCall, Plugin, ToolOutcome, tool_fn};
//! use aphid_agent::testing::{Turn, scripted};
//! use aphid_core::{Role, providers::deepseek};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct Weather {
//!     city: String,
//! }
//!
//! // A plugin that vetoes one city.
//! struct NoLisbon;
//!
//! impl Plugin for NoLisbon {
//!     fn name(&self) -> &str {
//!         "no-lisbon"
//!     }
//!
//!     fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
//!         if call.arguments().contains("Lisbon") {
//!             return Guard::block("Lisbon is off limits.");
//!         }
//!         Guard::Allow
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
//!     .plugin(NoLisbon)
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
mod plugin;
mod registry;
mod run;
mod stream;
pub mod testing;
mod tool;

pub use agent::{
    Agent, AgentBuilder, AgentConfig, AgentHandle, DEFAULT_MAX_TURNS, NoModel, RunOutcome,
    create_agent,
};
pub use plugin::{
    Cx, EventListener, Flow, Guard, Interest, PendingCall, Plugin, ResultCx, RunCx, StreamCx,
    TurnCx, TurnSummary,
};
pub use registry::{Plugins, Tools};
pub use stream::{Backend, BoxStream, DynAssistantStream, Live, StreamFn, live_stream_fn};
pub use tool::{
    BoxFuture, Execution, FnTool, ProgressSink, ToolCall, ToolContent, ToolCx, ToolHandler,
    ToolOutcome, tool_fn,
};
