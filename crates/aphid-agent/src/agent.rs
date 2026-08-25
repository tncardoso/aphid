//! The agent itself: configuration, construction and state.
//!
//! The loop that drives it lives in [`crate::run`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aphid_core::{
    MessageId, Model, Role, SimpleStreamOptions, StopReason, ThinkingLevel, Transcript, Usage,
};

use crate::events::{StreamListeners, ToolProgress, TranscriptListeners};

use crate::registry::Tools;
use crate::rt::{Bus, Composition, Runtime};
use crate::stream::{StreamFn, live_stream_fn};
use crate::tool::{ProgressSink, ToolCx, ToolHandler};
use crate::toolbox::Toolbox;

/// How many turns a run takes before the loop gives up, unless configured
/// otherwise. High enough not to interrupt real work, low enough that a model
/// stuck in a tool loop stops costing money.
pub const DEFAULT_MAX_TURNS: u32 = 64;

/// What a run produced.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    /// Why the last turn stopped.
    pub stop: StopReason,
    /// How many provider requests the run made.
    pub turns: u32,
    /// Tokens and cost for this run alone.
    pub usage: Usage,
    /// The last assistant message committed, if any.
    pub last: Option<MessageId>,
    /// The failure reported by the last turn, if it failed.
    pub error: Option<String>,
}

impl RunOutcome {
    /// The outcome of a run a [`Plugin::on_prompt`](crate::Plugin::on_prompt)
    /// hook turned away: nothing was appended and no request was sent.
    #[must_use]
    pub(crate) fn rejected(reason: String) -> Self {
        Self {
            stop: StopReason::Aborted,
            turns: 0,
            usage: Usage::default(),
            last: None,
            error: Some(reason),
        }
    }

    /// Whether the run ended in a provider or transport failure.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        self.stop.is_failure()
    }
}

/// A cancellation handle, cloneable and cheap to hold elsewhere.
#[derive(Clone, Debug, Default)]
pub struct AgentHandle {
    cancel: Arc<AtomicBool>,
}

impl AgentHandle {
    /// Ask the run to stop at its next checkpoint: between protocol events, or
    /// between turns. Tools observe it through [`ToolCx::cancelled`].
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }
}

/// A configured agent: a transcript, a model, its tools, and the components that
/// extend the loop.
pub struct Agent {
    pub(crate) transcript: Transcript,
    pub(crate) model: Model,
    pub(crate) tools: Arc<Toolbox>,
    /// Shared, because the progress sink handed to running tools holds it too.
    /// Where components listen. Shared, because the progress sink handed to
    /// running tools holds it too, and because every payload a listener may
    /// answer from another task carries a handle.
    pub(crate) bus: Arc<Bus>,
    /// The per-token subscribers. Not on the bus, and the type says why.
    pub(crate) stream_listeners: Arc<StreamListeners>,
    /// Subscribers that read the transcript where it grew. Also not on the bus,
    /// for the same reason.
    pub(crate) transcript_listeners: Arc<TranscriptListeners>,
    /// The composition this loop was built into, if it was built into one.
    /// Settled at the start of every run, so a component mounted from
    /// synchronous assembly code is loaded before anything is announced.
    pub(crate) composition: Option<Runtime>,
    pub(crate) options: SimpleStreamOptions,
    pub(crate) stream_fn: StreamFn,
    pub(crate) max_turns: u32,
    pub(crate) usage: Usage,
    pub(crate) cancel: Arc<AtomicBool>,
}

impl Agent {
    /// Start configuring an agent. The model is required by the type, so
    /// [`AgentBuilder::build`] cannot fail.
    #[must_use]
    pub fn builder() -> AgentBuilder<NoModel> {
        AgentBuilder {
            model: NoModel,
            system: None,
            tools: Tools::new(),
            bus: Arc::new(Bus::new()),
            stream_listeners: Arc::new(StreamListeners::new()),
            transcript_listeners: Arc::new(TranscriptListeners::new()),
            composition: None,
            toolbox: None,
            options: SimpleStreamOptions::default(),
            stream_fn: None,
            max_turns: DEFAULT_MAX_TURNS,
        }
    }

    #[must_use]
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// Mutable access, for callers that manage history themselves — pruning,
    /// compaction, replaying a saved session.
    pub fn transcript_mut(&mut self) -> &mut Transcript {
        &mut self.transcript
    }

    #[must_use]
    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn set_model(&mut self, model: Model) {
        self.model = model;
    }

    /// Swap the credential mid-session.
    ///
    /// Needed alongside [`set_model`](Self::set_model): a switch to a model from
    /// another provider would otherwise keep sending the previous provider's
    /// key. `None` clears it.
    pub fn set_api_key(&mut self, key: Option<compact_str::CompactString>) {
        self.options.stream.request.api_key = key;
    }

    #[must_use]
    pub fn tools(&self) -> &Arc<Toolbox> {
        &self.tools
    }

    /// Where components subscribe to what the loop announces.
    #[must_use]
    pub fn bus(&self) -> &Arc<Bus> {
        &self.bus
    }

    /// The per-token subscribers, which are deliberately not on the bus.
    #[must_use]
    pub fn stream_listeners(&self) -> &Arc<StreamListeners> {
        &self.stream_listeners
    }

    /// The subscribers that read the transcript where it grew.
    #[must_use]
    pub fn transcript_listeners(&self) -> &Arc<TranscriptListeners> {
        &self.transcript_listeners
    }

    pub fn set_thinking(&mut self, level: Option<ThinkingLevel>) {
        self.options.reasoning = level;
    }

    #[must_use]
    pub fn options(&self) -> &SimpleStreamOptions {
        &self.options
    }

    pub fn options_mut(&mut self) -> &mut SimpleStreamOptions {
        &mut self.options
    }

    /// Tokens and cost across every run this agent has made.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    #[must_use]
    pub fn handle(&self) -> AgentHandle {
        AgentHandle {
            cancel: Arc::clone(&self.cancel),
        }
    }

    /// The context handed to running tools, before it is scoped to a call.
    #[must_use]
    pub fn tool_cx(&self) -> ToolCx {
        ToolCx::new(
            Arc::clone(&self.cancel),
            Arc::new(BusProgress {
                bus: Arc::clone(&self.bus),
            }),
        )
    }

    /// Register a tool after construction. Replaces any tool of the same name.
    ///
    /// The set is read afresh each turn, so a tool added mid-session is offered
    /// on the next request rather than the next process.
    pub fn register_tool(&mut self, tool: impl ToolHandler) {
        self.tools.push(Arc::new(tool));
    }

    /// Replace the system prompt.
    ///
    /// The arenas are append-only, so this rebuilds the transcript into a fresh
    /// one — which also drops whatever garbage had accumulated. Call it between
    /// runs; a component that wants to add instructions for a single turn
    /// should listen for [`TurnStart`](crate::TurnStart) and call `note` on
    /// its payload instead.
    pub fn set_system(&mut self, text: &str) {
        let keep: Vec<MessageId> = (0..self.transcript.len())
            .filter(|&index| {
                // Drop only a leading system prompt. A system note appended
                // mid-conversation is part of the record and stays.
                !(index == 0
                    && self
                        .transcript
                        .get(index)
                        .is_some_and(|message| message.role() == Role::System))
            })
            .filter_map(|index| self.transcript.id_at(index))
            .collect();

        let mut rebuilt = Transcript::new();
        rebuilt.push_system(text);
        self.transcript.compact_into(&keep, &mut rebuilt);
        self.transcript = rebuilt;
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("model", &self.model.id)
            .field("messages", &self.transcript.len())
            .field("tools", &self.tools)
            .field("max_turns", &self.max_turns)
            .finish()
    }
}

/// Routes partial tool output to whoever subscribed to it.
struct BusProgress {
    bus: Arc<Bus>,
}

impl ProgressSink for BusProgress {
    fn progress(&self, call_id: &str, tool: &str, chunk: &str) {
        if self.bus.has_listeners::<ToolProgress>() {
            self.bus.emit(&mut ToolProgress {
                call_id: call_id.to_owned(),
                tool: tool.to_owned(),
                chunk: chunk.to_owned(),
            });
        }
    }

    fn is_observed(&self) -> bool {
        self.bus.has_listeners::<ToolProgress>()
    }
}

/// The builder's initial state: no model chosen yet, so no `build`.
#[derive(Debug)]
pub struct NoModel;

/// Configures an [`Agent`].
///
/// ```
/// # use aphid_agent::Agent;
/// # use aphid_core::{ThinkingLevel, providers::deepseek};
/// let agent = Agent::builder()
///     .model(deepseek::flash())
///     .system("You are terse.")
///     .thinking(ThinkingLevel::Low)
///     .max_turns(8)
///     .build();
///
/// assert_eq!(agent.transcript().len(), 1);
/// ```
pub struct AgentBuilder<M> {
    model: M,
    system: Option<String>,
    tools: Tools,
    bus: Arc<Bus>,
    stream_listeners: Arc<StreamListeners>,
    transcript_listeners: Arc<TranscriptListeners>,
    composition: Option<Runtime>,
    toolbox: Option<Arc<Toolbox>>,
    options: SimpleStreamOptions,
    stream_fn: Option<StreamFn>,
    max_turns: u32,
}

impl AgentBuilder<NoModel> {
    /// Choose the model. This is what unlocks [`AgentBuilder::build`].
    #[must_use]
    pub fn model(self, model: Model) -> AgentBuilder<Model> {
        AgentBuilder {
            model,
            system: self.system,
            tools: self.tools,
            bus: self.bus,
            stream_listeners: self.stream_listeners,
            transcript_listeners: self.transcript_listeners,
            composition: self.composition,
            toolbox: self.toolbox,
            options: self.options,
            stream_fn: self.stream_fn,
            max_turns: self.max_turns,
        }
    }
}

impl<M> AgentBuilder<M> {
    /// The system prompt, appended as the transcript's first message.
    #[must_use]
    pub fn system(mut self, text: impl Into<String>) -> Self {
        self.system = Some(text.into());
        self
    }

    /// Register a tool. A later registration under the same name wins.
    #[must_use]
    pub fn tool(mut self, tool: impl ToolHandler) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// Register several already-boxed tools, for toolsets assembled elsewhere.
    #[must_use]
    pub fn tools<I>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = Arc<dyn ToolHandler>>,
    {
        for tool in tools {
            self.tools.push(tool);
        }
        self
    }

    #[must_use]
    pub fn thinking(mut self, level: ThinkingLevel) -> Self {
        self.options.reasoning = Some(level);
        self
    }

    #[must_use]
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.options.stream.max_tokens = Some(max_tokens);
        self
    }

    #[must_use]
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.options.stream.temperature = Some(temperature);
        self
    }

    #[must_use]
    pub fn api_key(mut self, key: impl Into<compact_str::CompactString>) -> Self {
        self.options.stream.request.api_key = Some(key.into());
        self
    }

    /// Replace the request options wholesale.
    #[must_use]
    pub fn options(mut self, options: SimpleStreamOptions) -> Self {
        self.options = options;
        self
    }

    /// Cap how many provider requests one run may make.
    #[must_use]
    pub fn max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Replace the provider backend. Tests pass a scripted stream here.
    #[must_use]
    pub fn stream_fn(mut self, stream_fn: StreamFn) -> Self {
        self.stream_fn = Some(stream_fn);
        self
    }

    /// Build into a composition the caller already assembled.
    ///
    /// Everything mounted on it is already subscribed, so the loop starts
    /// announcing to listeners that are in place rather than to an empty bus
    /// that components join afterwards.
    #[must_use]
    pub fn compose(mut self, composition: &Composition) -> Self {
        self.bus = Arc::clone(&composition.bus);
        self.stream_listeners = Arc::clone(&composition.stream);
        self.transcript_listeners = Arc::clone(&composition.transcript);
        self.composition = Some(composition.runtime.clone());
        self.toolbox = Some(Arc::clone(&composition.tools));
        self
    }

    /// Use a bus the caller already owns.
    ///
    /// The point of supplying one is composition order: components subscribe
    /// when they load, which is generally before an agent exists to subscribe
    /// to. Handing the agent a bus that already has listeners on it is what
    /// lets the front end assemble the system first and build the loop into it.
    #[must_use]
    pub fn bus(mut self, bus: Arc<Bus>) -> Self {
        self.bus = bus;
        self
    }

    /// Use per-token subscribers the caller already owns. See
    /// [`AgentBuilder::bus`] for why.
    #[must_use]
    pub fn stream_listeners(mut self, listeners: Arc<StreamListeners>) -> Self {
        self.stream_listeners = listeners;
        self
    }

    /// Use transcript subscribers the caller already owns. See
    /// [`AgentBuilder::bus`] for why.
    #[must_use]
    pub fn transcript_listeners(mut self, listeners: Arc<TranscriptListeners>) -> Self {
        self.transcript_listeners = listeners;
        self
    }
}

impl AgentBuilder<Model> {
    /// Build the agent. Infallible: the type already proved a model was set.
    #[must_use]
    pub fn build(self) -> Agent {
        // The composition's box when there is one, so components that already
        // registered tools keep them and the agent's own join the same set.
        let tools = self.toolbox.unwrap_or_else(|| Arc::new(Toolbox::new()));
        for handler in self.tools.into_handlers() {
            tools.push(handler);
        }

        let mut transcript = Transcript::new();
        if let Some(system) = &self.system {
            transcript.push_system(system);
        }

        Agent {
            transcript,
            model: self.model,
            tools,
            bus: self.bus,
            stream_listeners: self.stream_listeners,
            transcript_listeners: self.transcript_listeners,
            composition: self.composition,
            options: self.options,
            stream_fn: self.stream_fn.unwrap_or_else(live_stream_fn),
            max_turns: self.max_turns,
            usage: Usage::default(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Everything [`create_agent`] needs.
///
/// Start from [`AgentConfig::new`] and override with struct-update syntax. There
/// is no `Default`, because there is no sensible default model.
pub struct AgentConfig {
    pub model: Model,
    pub system: Option<String>,
    pub tools: Vec<Arc<dyn ToolHandler>>,
    pub options: SimpleStreamOptions,
    pub max_turns: u32,
    /// `None` uses the real provider backend.
    pub stream_fn: Option<StreamFn>,
}

impl AgentConfig {
    #[must_use]
    pub fn new(model: Model) -> Self {
        Self {
            model,
            system: None,
            tools: Vec::new(),
            options: SimpleStreamOptions::default(),
            max_turns: DEFAULT_MAX_TURNS,
            stream_fn: None,
        }
    }
}

/// Build an agent from a config struct.
///
/// The same thing [`Agent::builder`] does, shaped for people arriving from
/// `create_agent()` in langgraph, agno or pydantic-ai.
///
/// ```
/// # use aphid_agent::{AgentConfig, create_agent};
/// # use aphid_core::providers::deepseek;
/// let agent = create_agent(AgentConfig {
///     system: Some("You are terse.".into()),
///     ..AgentConfig::new(deepseek::flash())
/// });
///
/// assert_eq!(agent.transcript().len(), 1);
/// ```
#[must_use]
pub fn create_agent(config: AgentConfig) -> Agent {
    let mut builder = Agent::builder()
        .model(config.model)
        .options(config.options)
        .max_turns(config.max_turns)
        .tools(config.tools);

    if let Some(system) = config.system {
        builder = builder.system(system);
    }
    if let Some(stream_fn) = config.stream_fn {
        builder = builder.stream_fn(stream_fn);
    }

    builder.build()
}
