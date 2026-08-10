//! `edit` — replace exact spans of text in a file.
//!
//! The uniqueness rule is what makes this safe: every `old_text` must match the
//! file exactly once. A model that supplies an ambiguous snippet gets an error
//! naming the ambiguity instead of a silently wrong edit, and one that supplies
//! a stale snippet learns the file changed under it.

use aphid_agent::{ToolHandler, ToolOutcome, tool_fn};
use aphid_core::Json;
use serde::Deserialize;

use super::paths::Workspace;

pub const NAME: &str = "edit";

pub const SNIPPET: &str = "replace exact text in a file";

#[derive(Debug, Deserialize)]
pub struct Replacement {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Deserialize)]
pub struct Params {
    pub path: String,
    pub edits: Vec<Replacement>,
}

#[must_use]
pub fn schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
            "edits": {
                "type": "array",
                "description": "Replacements applied in order. Each old_text must appear exactly once in the file.",
                "items": {
                    "type": "object",
                    "properties": {
                        "old_text": { "type": "string", "description": "Exact text to replace, including indentation." },
                        "new_text": { "type": "string", "description": "Replacement text for this targeted edit." }
                    },
                    "required": ["old_text", "new_text"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["path", "edits"],
        "additionalProperties": false
    })
}

pub const DESCRIPTION: &str = "Edit a file by replacing exact spans of text. Each edit's old_text \
     must appear exactly once in the file — include surrounding lines to make it unique. Edits are \
     applied in order and the file is written once. If any edit does not match exactly once, no \
     part of the file is changed.";

#[must_use]
pub fn tool(workspace: &Workspace) -> impl ToolHandler {
    let workspace = workspace.clone();
    tool_fn(NAME, DESCRIPTION, schema(), move |params: Params, _cx| {
        let workspace = workspace.clone();
        async move { execute(&workspace, &params).await }
    })
}

/// Where an edit landed, for the diff a renderer draws.
#[derive(Debug)]
struct Applied {
    line: usize,
    old: String,
    new: String,
}

async fn execute(workspace: &Workspace, params: &Params) -> ToolOutcome {
    let path = match workspace.resolve(&params.path) {
        Ok(path) => path,
        Err(error) => return ToolOutcome::error(error),
    };

    if params.edits.is_empty() {
        return ToolOutcome::error("no edits were given");
    }

    let original = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) => {
            return ToolOutcome::error(format!("could not read {}: {error}", params.path));
        }
    };

    // Every edit is validated and applied against a working copy first, so a
    // failure halfway through leaves the file on disk untouched.
    let mut working = original.clone();
    let mut applied = Vec::with_capacity(params.edits.len());

    for (index, edit) in params.edits.iter().enumerate() {
        let ordinal = index + 1;
        if edit.old_text.is_empty() {
            return ToolOutcome::error(format!("edit {ordinal}: old_text is empty"));
        }
        if edit.old_text == edit.new_text {
            return ToolOutcome::error(format!(
                "edit {ordinal}: old_text and new_text are identical, so it would change nothing"
            ));
        }

        let matches = working.matches(edit.old_text.as_str()).count();
        match matches {
            1 => {}
            0 => {
                return ToolOutcome::error(format!(
                    "edit {ordinal}: old_text does not appear in {}. It must match the file \
                     exactly, including indentation — read the file again if it has changed.",
                    params.path
                ));
            }
            n => {
                return ToolOutcome::error(format!(
                    "edit {ordinal}: old_text appears {n} times in {}. Include more surrounding \
                     lines so it identifies exactly one place.",
                    params.path
                ));
            }
        }

        let at = working
            .find(edit.old_text.as_str())
            .expect("a single match was just counted");
        applied.push(Applied {
            line: working[..at].lines().count().max(1),
            old: edit.old_text.clone(),
            new: edit.new_text.clone(),
        });
        working.replace_range(at..at + edit.old_text.len(), &edit.new_text);
    }

    if let Err(error) = tokio::fs::write(&path, &working).await {
        return ToolOutcome::error(format!("could not write {}: {error}", params.path));
    }

    let relative = workspace.display(&path);
    let count = applied.len();
    let plural = if count == 1 { "edit" } else { "edits" };
    let mut outcome = ToolOutcome::text(format!("Applied {count} {plural} to {relative}"));
    outcome.details = Some(serde_json::json!({
        "path": relative,
        "edits": applied
            .iter()
            .map(|edit| serde_json::json!({
                "line": edit.line,
                "old": edit.old,
                "new": edit.new,
            }))
            .collect::<Vec<_>>(),
    }));
    outcome
}
