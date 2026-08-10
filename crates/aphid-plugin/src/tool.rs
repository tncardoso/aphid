//! Tools written in Rhai.
//!
//! A script registers one while its body runs at load time:
//!
//! ```rhai
//! register_tool(#{
//!     name: "wordcount",
//!     description: "Count the words in a file.",
//!     parameters: #{
//!         type: "object",
//!         properties: #{ path: #{ type: "string" } },
//!         required: ["path"]
//!     },
//!     execute: |args| { fs_read(args.path).split(' ').len() }
//! });
//! ```
//!
//! The schema is written by hand, as every built-in tool's is. Registering a
//! name a built-in already uses replaces it, because the tool registry keeps one
//! handler per name and takes the last one registered.
//!
//! The body runs on a blocking thread rather than the agent's task. A hook has
//! to be quick because the loop is waiting on it; a tool is a thing the model
//! chose to wait for, and `exec` and `http` are exactly what one is usually for.

use std::sync::{Arc, Mutex};

use aphid_agent::{BoxFuture, Execution, ToolCall, ToolCx, ToolHandler, ToolOutcome};
use aphid_core::Tool;
use rhai::{Dynamic, FnPtr, Map};

use crate::convert;
use crate::script::ScriptPlugin;

/// A tool declaration collected while a script loaded.
#[derive(Clone)]
pub struct ToolSpec {
    pub declaration: Tool,
    pub body: FnPtr,
    pub execution: Execution,
}

/// Where `register_tool` puts what it is given.
///
/// Shared with the engine's closure, because registration happens while the
/// script's body runs and the plugin does not exist yet at that point.
pub type Registry = Arc<Mutex<Vec<ToolSpec>>>;

/// Read a `register_tool` argument.
///
/// Returns the reason it was refused, which the script sees as a runtime error —
/// a malformed declaration is a mistake to fix, not something to discover later
/// as a tool that is quietly missing.
pub(crate) fn spec(declaration: &Map) -> Result<ToolSpec, String> {
    let name = declaration
        .get("name")
        .filter(|value| value.is_string())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| "a tool needs a `name`".to_owned())?;

    let description = declaration
        .get("description")
        .filter(|value| value.is_string())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| format!("tool `{name}` needs a `description`"))?;

    let body = declaration
        .get("execute")
        .and_then(|value| value.clone().try_cast::<FnPtr>())
        .ok_or_else(|| format!("tool `{name}` needs an `execute` function"))?;

    let parameters = declaration
        .get("parameters")
        .map_or_else(|| serde_json::json!({ "type": "object" }), convert::to_json);

    let execution = if declaration
        .get("sequential")
        .and_then(|value| value.as_bool().ok())
        .unwrap_or(false)
    {
        Execution::Sequential
    } else {
        Execution::Parallel
    };

    Ok(ToolSpec {
        declaration: Tool::new(name, description, parameters),
        body,
        execution,
    })
}

/// A [`ToolHandler`] that runs a Rhai function.
pub struct ScriptTool {
    plugin: Arc<ScriptPlugin>,
    spec: ToolSpec,
}

impl ScriptTool {
    #[must_use]
    pub fn new(plugin: Arc<ScriptPlugin>, spec: ToolSpec) -> Self {
        Self { plugin, spec }
    }
}

impl ToolHandler for ScriptTool {
    fn declaration(&self) -> &Tool {
        &self.spec.declaration
    }

    fn execute<'a>(&'a self, call: ToolCall<'a>, _cx: &'a ToolCx) -> BoxFuture<'a, ToolOutcome> {
        // Everything the body needs, owned, because it is about to move to
        // another thread.
        let plugin = Arc::clone(&self.plugin);
        let body = self.spec.body.clone();
        let name = self.spec.declaration.name.to_string();
        let arguments = call.arguments.to_owned();

        Box::pin(async move {
            let run = tokio::task::spawn_blocking(move || {
                let parsed: serde_json::Value = match serde_json::from_str(&arguments) {
                    Ok(value) => value,
                    // A tool called with unparseable arguments is a mistake the
                    // model can see and correct, exactly as `tool_fn` treats it.
                    Err(error) => {
                        return Err(format!("`{name}` was called with invalid JSON: {error}"));
                    }
                };

                plugin
                    .call_fn(&body, (convert::object_to_map(&parsed),))
                    .map_err(|error| format!("`{name}` failed: {error}"))
            })
            .await;

            match run {
                Ok(Ok(value)) => outcome(&value),
                Ok(Err(message)) => ToolOutcome::error(message),
                // A panic in a script is still just this tool's failure.
                Err(error) => ToolOutcome::error(format!("the tool did not finish: {error}")),
            }
        })
    }

    fn execution(&self) -> Execution {
        self.spec.execution
    }
}

/// Turn what a script returned into a result.
///
/// A string or number is the answer itself, which is what most tools return. A
/// map with `content` says more: an error flag, structured details a renderer
/// can use.
fn outcome(value: &Dynamic) -> ToolOutcome {
    if value.is_map() {
        let map: Map = value.clone().cast();
        if let Some(content) = map.get("content") {
            let mut result = ToolOutcome::text(content.to_string());
            result.is_error = map
                .get("is_error")
                .and_then(|flag| flag.as_bool().ok())
                .unwrap_or(false);
            if let Some(details) = map.get("details").filter(|value| !value.is_unit()) {
                result.details = Some(convert::to_json(details));
            }
            return result;
        }
    }

    if value.is_unit() {
        return ToolOutcome::text("done");
    }

    ToolOutcome::text(value.to_string())
}
