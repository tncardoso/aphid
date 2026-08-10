//! `bash` — run a shell command, streaming its output.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use aphid_agent::{ToolCx, ToolHandler, ToolOutcome, tool_fn};
use aphid_core::Json;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::paths::Workspace;
use super::truncate;

/// How often the tool notices that the run was cancelled.
const CANCEL_POLL: Duration = Duration::from_millis(50);

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
pub fn tool(workspace: &Workspace) -> impl ToolHandler {
    let workspace = workspace.clone();
    tool_fn(NAME, description(), schema(), move |params: Params, cx| {
        let workspace = workspace.clone();
        async move { execute(&workspace, params, cx).await }
    })
}

async fn execute(workspace: &Workspace, params: Params, cx: ToolCx) -> ToolOutcome {
    if let Some(timeout) = params.timeout
        && (!timeout.is_finite() || timeout <= 0.0)
    {
        return ToolOutcome::error("timeout must be a positive number of seconds");
    }

    let mut child = match Command::new("bash")
        .arg("-c")
        .arg(&params.command)
        .current_dir(workspace.root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // So an abandoned future does not leave a process behind.
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return ToolOutcome::error(format!("could not start bash: {error}")),
    };

    // stdout and stderr are pumped concurrently and appended to one buffer, so
    // the output reads the way it would in a terminal. Interleaving between the
    // two pipes is inherently approximate.
    let collected = Arc::new(Mutex::new(String::new()));
    let mut pumps = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        pumps.push(tokio::spawn(pump(
            stdout,
            Arc::clone(&collected),
            cx.clone(),
        )));
    }
    if let Some(stderr) = child.stderr.take() {
        pumps.push(tokio::spawn(pump(
            stderr,
            Arc::clone(&collected),
            cx.clone(),
        )));
    }

    let outcome = wait(&mut child, &params, &cx).await;

    for pump in pumps {
        let _ = pump.await;
    }

    let full = collected.lock().expect("output lock").clone();
    let capped = truncate::tail(&full);
    let full_output_path = capped
        .full_output_path
        .as_ref()
        .map(|path| path.display().to_string());
    let truncated = capped.truncated;
    let mut text = capped.into_text();

    match outcome {
        Wait::Exited(0) => {}
        // A non-zero exit is not a tool failure — plenty of commands report
        // through their status. The model is told, and decides.
        Wait::Exited(code) => text.push_str(&format!("\n[exit code {code}]")),
        Wait::Signalled => text.push_str("\n[terminated by signal]"),
        Wait::TimedOut(seconds) => {
            return finish(
                text + &format!("\n[timed out after {seconds}s]"),
                true,
                truncated,
                full_output_path,
            );
        }
        Wait::Cancelled => {
            return finish(text + "\n[cancelled]", true, truncated, full_output_path);
        }
        Wait::Failed(error) => {
            return finish(
                text + &format!("\n[could not wait for the command: {error}]"),
                true,
                truncated,
                full_output_path,
            );
        }
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

enum Wait {
    Exited(i32),
    Signalled,
    TimedOut(f64),
    Cancelled,
    Failed(std::io::Error),
}

async fn wait(child: &mut tokio::process::Child, params: &Params, cx: &ToolCx) -> Wait {
    let outcome = match params.timeout {
        Some(seconds) => {
            let limit = Duration::from_secs_f64(seconds);
            tokio::select! {
                status = child.wait() => status_of(status),
                () = cancelled(cx) => Wait::Cancelled,
                () = tokio::time::sleep(limit) => Wait::TimedOut(seconds),
            }
        }
        None => {
            tokio::select! {
                status = child.wait() => status_of(status),
                () = cancelled(cx) => Wait::Cancelled,
            }
        }
    };

    if matches!(outcome, Wait::TimedOut(_) | Wait::Cancelled) {
        let _ = child.kill().await;
    }
    outcome
}

fn status_of(status: std::io::Result<std::process::ExitStatus>) -> Wait {
    match status {
        Ok(status) => match status.code() {
            Some(code) => Wait::Exited(code),
            None => Wait::Signalled,
        },
        Err(error) => Wait::Failed(error),
    }
}

/// `ToolCx::cancelled` is a flag, not a future, so it has to be polled.
async fn cancelled(cx: &ToolCx) {
    loop {
        if cx.cancelled() {
            return;
        }
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

/// Forward one pipe into the shared buffer, publishing each line as it lands.
async fn pump<R>(reader: R, collected: Arc<Mutex<String>>, cx: ToolCx)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(mut buffer) = collected.lock() {
            buffer.push_str(&line);
            buffer.push('\n');
        }
        if cx.is_observed() {
            cx.progress(&line);
        }
    }
}
