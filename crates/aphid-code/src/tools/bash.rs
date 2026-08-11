//! `bash` — run a shell command, streaming its output.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use aphid_agent::exec::{self, Registry, Spec, Status};
use aphid_agent::{ToolCx, ToolHandler, ToolOutcome, tool_fn};
use aphid_core::Json;
use serde::Deserialize;

use super::paths::Workspace;
use super::truncate;

pub const NAME: &str = "bash";

pub const SNIPPET: &str = "run a shell command in the workspace";

#[derive(Debug, Deserialize)]
pub struct Params {
    pub command: String,
    /// Seconds. No timeout when absent.
    #[serde(default)]
    pub timeout: Option<f64>,
}

#[must_use]
pub fn schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Bash command to execute" },
            "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

#[must_use]
pub fn description() -> String {
    format!(
        "Execute a bash command in the workspace root. Returns stdout and stderr interleaved. \
         Output is capped at the last {} lines or {} KiB, whichever comes first; when it is \
         capped the full output is written to a temp file whose path is included in the result. \
         Optionally provide a timeout in seconds.",
        truncate::MAX_LINES,
        truncate::MAX_BYTES / 1024
    )
}

#[must_use]
pub fn tool(workspace: &Workspace, processes: &Arc<Registry>) -> impl ToolHandler {
    let workspace = workspace.clone();
    let processes = Arc::clone(processes);
    tool_fn(NAME, description(), schema(), move |params: Params, cx| {
        let workspace = workspace.clone();
        let processes = Arc::clone(&processes);
        async move { execute(&workspace, &processes, params, cx).await }
    })
}

async fn execute(
    workspace: &Workspace,
    processes: &Arc<Registry>,
    params: Params,
    cx: ToolCx,
) -> ToolOutcome {
    if let Some(timeout) = params.timeout
        && (!timeout.is_finite() || timeout <= 0.0)
    {
        return ToolOutcome::error("timeout must be a positive number of seconds");
    }

    // stdout and stderr are pumped concurrently and appended to one buffer, so
    // the output reads the way it would in a terminal. Interleaving between the
    // two pipes is inherently approximate.
    let collected = Arc::new(Mutex::new(String::new()));
    let sink = {
        let collected = Arc::clone(&collected);
        let cx = cx.clone();
        Arc::new(move |_stream, line: &str| {
            if let Ok(mut buffer) = collected.lock() {
                buffer.push_str(line);
                buffer.push('\n');
            }
            if cx.is_observed() {
                cx.progress(line);
            }
        })
    };

    let spec = Spec::new(NAME, params.command)
        .cwd(Some(workspace.root().to_path_buf()))
        .timeout(params.timeout.map(Duration::from_secs_f64));
    let status = exec::run(processes, spec, Some(&cx), sink).await;

    let full = collected.lock().expect("output lock").clone();
    let capped = truncate::tail(&full);
    let full_output_path = capped
        .full_output_path
        .as_ref()
        .map(|path| path.display().to_string());
    let truncated = capped.truncated;
    let mut text = capped.into_text();

    match status {
        Status::Exited(0) => {}
        // A non-zero exit is not a tool failure — plenty of commands report
        // through their status. The model is told, and decides.
        Status::Exited(code) => text.push_str(&format!("\n[exit code {code}]")),
        Status::Signalled => text.push_str("\n[terminated by signal]"),
        Status::TimedOut => {
            let seconds = params.timeout.unwrap_or_default();
            return finish(
                format!("{text}\n[timed out after {seconds}s]"),
                true,
                truncated,
                full_output_path,
            );
        }
        Status::Cancelled => {
            return finish(text + "\n[cancelled]", true, truncated, full_output_path);
        }
        // Stopped from `/ps`, which is the user saying they have seen enough.
        Status::Killed | Status::Killing => {
            return finish(text + "\n[killed]", true, truncated, full_output_path);
        }
        Status::Failed(error) => {
            return finish(
                format!("{text}\n[{error}]"),
                true,
                truncated,
                full_output_path,
            );
        }
        Status::Running => {}
    }

    if text.is_empty() {
        text.push_str("[no output]");
    }
    finish(text, false, truncated, full_output_path)
}

fn finish(
    text: String,
    is_error: bool,
    truncated: bool,
    full_output_path: Option<String>,
) -> ToolOutcome {
    let mut outcome = if is_error {
        ToolOutcome::error(text)
    } else {
        ToolOutcome::text(text)
    };
    outcome.details = Some(serde_json::json!({
        "truncated": truncated,
        "full_output_path": full_output_path,
    }));
    outcome
}
