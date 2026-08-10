//! `read` — read a file, or a slice of one.

use aphid_agent::{ToolHandler, ToolOutcome, tool_fn};
use aphid_core::Json;
use serde::Deserialize;

use super::paths::Workspace;
use super::truncate;

/// How much of a file to sniff before deciding it is not text.
const SNIFF_BYTES: usize = 8192;

pub const NAME: &str = "read";

pub const SNIPPET: &str = "read a file, optionally a line range";

#[derive(Debug, Deserialize)]
pub struct Params {
    pub path: String,
    /// 1-indexed line to start from.
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[must_use]
pub fn schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "number", "description": "Maximum number of lines to read" }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

#[must_use]
pub fn description() -> String {
    format!(
        "Read the contents of a text file. Output is line-numbered and capped at {} lines or \
         {} KiB, whichever comes first. Use offset and limit for large files; when you need the \
         whole file, continue with offset until it is complete. Binary files are not supported.",
        truncate::MAX_LINES,
        truncate::MAX_BYTES / 1024
    )
}

#[must_use]
pub fn tool(workspace: &Workspace) -> impl ToolHandler {
    let workspace = workspace.clone();
    tool_fn(NAME, description(), schema(), move |params: Params, _cx| {
        let workspace = workspace.clone();
        async move { execute(&workspace, &params).await }
    })
}

async fn execute(workspace: &Workspace, params: &Params) -> ToolOutcome {
    let path = match workspace.resolve(&params.path) {
        Ok(path) => path,
        Err(error) => return ToolOutcome::error(error),
    };

    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return ToolOutcome::error(format!("could not read {}: {error}", params.path));
        }
    };

    // A NUL byte early on is the cheap, reliable signal for "not text".
    if bytes.iter().take(SNIFF_BYTES).any(|byte| *byte == 0) {
        return ToolOutcome::error(format!(
            "{} looks like a binary file; this tool reads text only",
            params.path
        ));
    }

    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return ToolOutcome::error(format!("{} is not valid UTF-8", params.path));
        }
    };

    if text.is_empty() {
        return ToolOutcome::text(format!("{} is empty", params.path));
    }

    let all: Vec<&str> = text.lines().collect();
    let total_lines = all.len();
    let start = params.offset.unwrap_or(1).max(1) - 1;

    if start >= total_lines {
        return ToolOutcome::error(format!(
            "offset {} is past the end of {} ({total_lines} lines)",
            start + 1,
            params.path
        ));
    }

    let end = match params.limit {
        Some(limit) => (start + limit).min(total_lines),
        None => total_lines,
    };

    // The gutter width follows the largest number actually printed, so a short
    // file does not get a wide, empty margin.
    let width = end.to_string().len();
    let numbered: String = all[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("{:>width$}\t{line}\n", start + offset + 1))
        .collect();

    let capped = truncate::head(&numbered);
    let truncated = capped.truncated;
    let mut outcome = ToolOutcome::text(capped.into_text());
    outcome.details = Some(serde_json::json!({
        "path": workspace.display(&path),
        "total_lines": total_lines,
        "from_line": start + 1,
        "to_line": end,
        "truncated": truncated,
    }));
    outcome
}
