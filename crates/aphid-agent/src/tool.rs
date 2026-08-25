//! Tools the model may call, and the handlers that run them.
//!
//! [`aphid_core::Tool`] is only the *declaration* — the name, description and
//! JSON Schema sent to the provider. A [`ToolHandler`] pairs that declaration
//! with the code that executes a call.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aphid_core::{ContentInput, Json, Tool, ToolResultMeta, Usage};
use compact_str::CompactString;
use serde::de::DeserializeOwned;

/// A boxed future, the one allocation the async parts of this crate allow
/// themselves.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Whether a tool may run alongside its siblings from the same assistant turn.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Execution {
    /// May run concurrently with the other calls in the batch.
    #[default]
    Parallel,
    /// Must have the batch to itself. One sequential tool serializes the whole
    /// batch, which is the conservative reading and keeps ordering obvious.
    Sequential,
}

/// One tool call handed to a handler.
///
/// `arguments` is the raw JSON the model produced. It is borrowed straight out
/// of the transcript arena on the sequential path, so a handler that does not
/// need a typed struct pays nothing to read it.
#[derive(Copy, Clone, Debug)]
pub struct ToolCall<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub arguments: &'a str,
}

impl ToolCall<'_> {
    /// Deserialize the arguments into a typed struct.
    ///
    /// # Errors
    ///
    /// Fails when the model produced JSON that does not fit `T`.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_str(self.arguments)
    }

    /// The arguments as a JSON value.
    ///
    /// # Errors
    ///
    /// Fails when the model produced something that is not valid JSON.
    pub fn json(&self) -> Result<Json, serde_json::Error> {
        serde_json::from_str(self.arguments)
    }
}

/// Where a tool's partial output goes.
///
/// The agent installs one that fans out to [`ToolProgress`]; a tool
/// built and called on its own gets the no-op.
///
/// [`ToolProgress`]: crate::ToolProgress
pub trait ProgressSink: Send + Sync + 'static {
    fn progress(&self, call_id: &str, tool: &str, chunk: &str);

    /// Whether anything is listening. A tool can skip assembling progress text
    /// when nothing will read it.
    fn is_observed(&self) -> bool {
        true
    }
}

/// The default sink, for a `ToolCx` built outside a run.
struct Discard;

impl ProgressSink for Discard {
    fn progress(&self, _call_id: &str, _tool: &str, _chunk: &str) {}

    fn is_observed(&self) -> bool {
        false
    }
}

/// What a running tool can see of the agent around it.
///
/// Cheap to clone — a handle, not a snapshot — because the concurrent execution
/// path hands one to every spawned task. Each call gets its own, carrying the
/// identity the progress sink needs.
#[derive(Clone)]
pub struct ToolCx {
    cancel: Arc<AtomicBool>,
    sink: Arc<dyn ProgressSink>,
    call_id: CompactString,
    tool: CompactString,
}

impl ToolCx {
    /// A context bound to an agent's cancellation handle.
    ///
    /// For driving a tool outside a run — tests, a one-off invocation, a tool
    /// called by another tool. Inside a run the agent builds these itself.
    #[must_use]
    pub fn for_handle(handle: &crate::AgentHandle) -> Self {
        Self::new(handle.flag(), Arc::new(Discard))
    }

    /// Send this context's progress somewhere.
    #[must_use]
    pub fn with_sink(mut self, sink: Arc<dyn ProgressSink>) -> Self {
        self.sink = sink;
        self
    }

    #[must_use]
    pub(crate) fn new(cancel: Arc<AtomicBool>, sink: Arc<dyn ProgressSink>) -> Self {
        Self {
            cancel,
            sink,
            call_id: CompactString::default(),
            tool: CompactString::default(),
        }
    }

    /// Scope this context to one call.
    #[must_use]
    pub(crate) fn for_call(&self, call_id: &str, tool: &str) -> Self {
        Self {
            cancel: Arc::clone(&self.cancel),
            sink: Arc::clone(&self.sink),
            call_id: CompactString::new(call_id),
            tool: CompactString::new(tool),
        }
    }

    /// Whether the run has been cancelled. Long-running tools should poll this
    /// and return early.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The id of the call being executed.
    #[must_use]
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// The name of the tool being executed.
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// Publish output produced so far.
    ///
    /// A tool that takes a while — a build, a test run — calls this as lines
    /// arrive so a UI can show them live instead of a spinner. Chunks are
    /// advisory: the authoritative output is still the [`ToolOutcome`] the tool
    /// returns.
    pub fn progress(&self, chunk: &str) {
        self.sink.progress(&self.call_id, &self.tool, chunk);
    }

    /// Whether any plugin subscribed to progress. Checking this lets a tool
    /// avoid formatting output nobody will see.
    #[must_use]
    pub fn is_observed(&self) -> bool {
        self.sink.is_observed()
    }
}

impl Default for ToolCx {
    fn default() -> Self {
        Self::new(Arc::new(AtomicBool::new(false)), Arc::new(Discard))
    }
}

impl std::fmt::Debug for ToolCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCx")
            .field("call_id", &self.call_id)
            .field("tool", &self.tool)
            .field("cancelled", &self.cancelled())
            .finish()
    }
}

/// A piece of a tool's output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolContent {
    Text(String),
    Image { data: Vec<u8>, mime: CompactString },
}

impl ToolContent {
    pub(crate) fn as_input(&self) -> ContentInput<'_> {
        match self {
            ToolContent::Text(text) => ContentInput::Text(text),
            ToolContent::Image { data, mime } => ContentInput::Image { data, mime },
        }
    }
}

/// What a tool produced.
///
/// The fields line up one-for-one with [`ToolResultMeta`], so committing an
/// outcome to the transcript is a single `push_tool_result`.
#[derive(Clone, Debug, Default)]
pub struct ToolOutcome {
    pub content: Vec<ToolContent>,
    /// Structured payload for consumers that understand this tool.
    pub details: Option<Json>,
    pub is_error: bool,
    /// Cost of running the tool itself, where it is known.
    pub usage: Option<Usage>,
    /// Tools that became callable because this one ran.
    pub added_tool_names: Vec<CompactString>,
    /// Ask the run to stop after this batch. Honoured only when *every* result
    /// in the batch asks for it.
    pub terminate: bool,
}

impl ToolOutcome {
    /// A successful text result.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text(text.into())],
            ..Self::default()
        }
    }

    /// A failure the model is expected to read and recover from.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text(message.into())],
            is_error: true,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: Json) -> Self {
        self.details = Some(details);
        self
    }

    #[must_use]
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Ask the run to stop after this batch.
    #[must_use]
    pub fn terminating(mut self) -> Self {
        self.terminate = true;
        self
    }

    /// The concatenated text of this outcome, for renderers and assertions.
    #[must_use]
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                ToolContent::Text(text) => Some(text.as_str()),
                ToolContent::Image { .. } => None,
            })
            .collect()
    }

    pub(crate) fn into_meta(self, id: &str, name: &str) -> (ToolResultMeta, Vec<ToolContent>) {
        let mut meta = ToolResultMeta::new(id, name);
        meta.is_error = self.is_error;
        meta.usage = self.usage;
        meta.details = self.details;
        meta.added_tool_names = self.added_tool_names;
        (meta, self.content)
    }
}

/// A tool the agent can execute.
///
/// `execute` is the only async point in the plugin surface: hooks are
/// synchronous, so the streaming path never allocates a future per event.
pub trait ToolHandler: Send + Sync + 'static {
    /// The declaration sent to the provider. Built once, at registration.
    fn declaration(&self) -> &Tool;

    /// Run the call. Encode failures as `ToolOutcome::error` rather than
    /// panicking — the model is usually able to recover from them.
    fn execute<'a>(&'a self, call: ToolCall<'a>, cx: &'a ToolCx) -> BoxFuture<'a, ToolOutcome>;

    fn execution(&self) -> Execution {
        Execution::Parallel
    }
}

/// A [`ToolHandler`] built from an async closure over a deserialized argument
/// struct.
pub struct FnTool<F, P> {
    declaration: Tool,
    call: F,
    execution: Execution,
    _params: PhantomData<fn() -> P>,
}

impl<F, P> FnTool<F, P> {
    /// Force this tool to have its batch to itself.
    #[must_use]
    pub fn sequential(mut self) -> Self {
        self.execution = Execution::Sequential;
        self
    }

    /// Constrain the provider's sampling to this tool's schema.
    #[must_use]
    pub fn constrained(mut self, sampling: aphid_core::ConstrainedSampling) -> Self {
        self.declaration = self.declaration.constrained(sampling);
        self
    }
}

/// Build a tool from a schema and an async closure.
///
/// The JSON Schema is supplied by the caller rather than derived, matching
/// [`Tool::new`] and keeping the dependency list empty.
///
/// ```
/// # use aphid_agent::{ToolOutcome, tool_fn};
/// # use serde::Deserialize;
/// #[derive(Deserialize)]
/// struct Weather {
///     city: String,
/// }
///
/// let weather = tool_fn(
///     "get_weather",
///     "Look up the current weather for a city.",
///     serde_json::json!({
///         "type": "object",
///         "properties": { "city": { "type": "string" } },
///         "required": ["city"]
///     }),
///     |args: Weather, _cx| async move { ToolOutcome::text(format!("sunny in {}", args.city)) },
/// );
/// ```
pub fn tool_fn<P, F, Fut>(
    name: impl Into<CompactString>,
    description: impl Into<String>,
    parameters: Json,
    call: F,
) -> FnTool<F, P>
where
    P: DeserializeOwned + Send + 'static,
    F: Fn(P, ToolCx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ToolOutcome> + Send + 'static,
{
    FnTool {
        declaration: Tool::new(name, description, parameters),
        call,
        execution: Execution::Parallel,
        _params: PhantomData,
    }
}

impl<P, F, Fut> ToolHandler for FnTool<F, P>
where
    P: DeserializeOwned + Send + 'static,
    F: Fn(P, ToolCx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ToolOutcome> + Send + 'static,
{
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn execute<'a>(&'a self, call: ToolCall<'a>, cx: &'a ToolCx) -> BoxFuture<'a, ToolOutcome> {
        match serde_json::from_str::<P>(call.arguments) {
            Ok(params) => Box::pin((self.call)(params, cx.clone())),
            // The model produced arguments that do not fit the schema. That is
            // a recoverable mistake, so it goes back as an error result.
            Err(error) => {
                let name = call.name.to_owned();
                Box::pin(std::future::ready(ToolOutcome::error(format!(
                    "invalid arguments for `{name}`: {error}"
                ))))
            }
        }
    }

    fn execution(&self) -> Execution {
        self.execution
    }
}
