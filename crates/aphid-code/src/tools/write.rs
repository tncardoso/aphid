//! `write` — create or overwrite a file.

use std::sync::Arc;

use aphid_agent::{ToolCx, ToolHandler, ToolOutcome, tool_fn};
use aphid_core::Json;
use aphid_plugin::{Change, PluginHost};
use serde::Deserialize;

use super::paths::Workspace;

pub const NAME: &str = "write";

pub const SNIPPET: &str = "create or overwrite a file";

#[derive(Debug, Deserialize)]
pub struct Params {
    pub path: String,
    pub content: String,
}

#[must_use]
pub fn schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
            "content": { "type": "string", "description": "Content to write to the file" }
        },
        "required": ["path", "content"],
        "additionalProperties": false
    })
}

pub const DESCRIPTION: &str = "Write content to a file, creating it and any missing parent \
     directories. Overwrites the file if it already exists — to change part of an existing file, \
     prefer the edit tool.";

#[must_use]
pub fn tool(workspace: &Workspace, host: Option<Arc<PluginHost>>) -> impl ToolHandler {
    let workspace = workspace.clone();
    tool_fn(NAME, DESCRIPTION, schema(), move |params: Params, cx| {
        let workspace = workspace.clone();
        let host = host.clone();
        async move { execute(&workspace, &params, host.as_deref(), &cx).await }
    })
}

async fn execute(
    workspace: &Workspace,
    params: &Params,
    host: Option<&PluginHost>,
    cx: &ToolCx,
) -> ToolOutcome {
    let path = match workspace.resolve(&params.path) {
        Ok(path) => path,
        Err(error) => return ToolOutcome::error(error),
    };

    let existed = path.exists();
    let relative = workspace.display(&path);
    let lines = params.content.lines().count();

    // A write to a big or networked file can take a moment, and a silent card
    // looks stuck. Say what is about to happen, so a live card has something on
    // it before the syscall returns. Progress is advisory: the outcome is the
    // authoritative report.
    if cx.is_observed() {
        cx.progress(&format!(
            "{verb} {relative} ({bytes} bytes, {lines} lines)",
            verb = if existed { "overwriting" } else { "writing" },
            bytes = params.content.len(),
        ));
    }
    // Read before writing, and only when a plugin is listening: the old text is
    // what makes a change hook useful, and reading every file for nobody is not.
    let before = if host.is_some() && existed {
        tokio::fs::read_to_string(&path).await.ok()
    } else {
        None
    };
    if let Some(parent) = path.parent()
        && let Err(error) = tokio::fs::create_dir_all(parent).await
    {
        return ToolOutcome::error(format!("could not create {}: {error}", parent.display()));
    }

    if let Err(error) = tokio::fs::write(&path, &params.content).await {
        return ToolOutcome::error(format!("could not write {}: {error}", params.path));
    }

    if cx.is_observed() {
        cx.progress(&format!(
            "wrote {} bytes to {relative}",
            params.content.len()
        ));
    }

    if let Some(host) = host {
        host.file_change(&path, Change::Write, before.as_deref(), &params.content);
    }

    let verb = if existed { "Overwrote" } else { "Created" };
    let mut outcome = ToolOutcome::text(format!(
        "{verb} {relative} ({} bytes, {lines} lines)",
        params.content.len()
    ));
    outcome.details = Some(serde_json::json!({
        "path": relative,
        "bytes": params.content.len(),
        "lines": lines,
        "created": !existed,
    }));
    outcome
}
