//! The pieces aphid builds its system prompt from, for a plugin that builds its
//! own.
//!
//! `agent/request` can replace the system prompt whole. A plugin that does so
//! still wants the project's instructions, the skills and the tools in it, and
//! should not have to find `AGENTS.md` again or guess which tools a session
//! has. The harness fills this in once it has built the agent; until then every
//! function answers empty.

use std::sync::{Arc, RwLock};

use aphid_agent::Toolbox;
use rhai::{Array, Dynamic, Engine, Map};

use crate::context::ContextFile;
use crate::skills::Skill;

/// What the harness knew when it built the system prompt.
#[derive(Default)]
pub struct Parts {
    /// The system prompt as sent, after every `code/system-prompt` listener.
    pub system: String,
    /// The `AGENTS.md` files, global first.
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    /// The one-line guideline of each built-in tool, by tool name.
    pub snippets: Vec<(String, String)>,
    /// The agent's tools. Read when asked, so a tool a plugin added later is
    /// there too.
    pub tools: Option<Arc<Toolbox>>,
    /// The session the tools are read for, in a host with several.
    pub scope: Option<String>,
}

/// Shared by every plugin of a host, filled in by the harness.
#[derive(Default)]
pub struct PromptParts {
    parts: RwLock<Parts>,
}

impl PromptParts {
    /// Replace what is known.
    pub fn set(&self, parts: Parts) {
        match self.parts.write() {
            Ok(mut slot) => *slot = parts,
            Err(poisoned) => *poisoned.into_inner() = parts,
        }
    }

    fn read<T>(&self, read: impl FnOnce(&Parts) -> T) -> T {
        match self.parts.read() {
            Ok(parts) => read(&parts),
            Err(poisoned) => read(&poisoned.into_inner()),
        }
    }

    fn system(&self) -> String {
        self.read(|parts| parts.system.clone())
    }

    fn agents_md(&self) -> Array {
        self.read(|parts| {
            parts
                .context_files
                .iter()
                .map(|file| {
                    let mut entry = Map::new();
                    entry.insert("path".into(), file.path.display().to_string().into());
                    entry.insert("text".into(), file.content.clone().into());
                    Dynamic::from_map(entry)
                })
                .collect()
        })
    }

    fn skills(&self) -> Array {
        self.read(|parts| {
            parts
                .skills
                .iter()
                .map(|skill| {
                    let mut entry = Map::new();
                    entry.insert("name".into(), skill.name.clone().into());
                    entry.insert("description".into(), skill.description.clone().into());
                    entry.insert("path".into(), skill.path.display().to_string().into());
                    entry.insert("project".into(), skill.project.into());
                    Dynamic::from_map(entry)
                })
                .collect()
        })
    }

    fn tools(&self) -> Array {
        self.read(|parts| {
            let Some(toolbox) = &parts.tools else {
                return Array::new();
            };
            toolbox
                .declarations_for(parts.scope.as_deref())
                .into_iter()
                .map(|tool| {
                    let snippet = parts
                        .snippets
                        .iter()
                        .find(|(name, _)| *name == tool.name)
                        .map(|(_, snippet)| snippet.clone())
                        .unwrap_or_default();
                    let mut entry = Map::new();
                    entry.insert("name".into(), tool.name.to_string().into());
                    entry.insert("description".into(), tool.description.into());
                    entry.insert("snippet".into(), snippet.into());
                    Dynamic::from_map(entry)
                })
                .collect()
        })
    }
}

/// `system_prompt()`, `agents_md()`, `skills()` and `tool_list()`.
pub(crate) fn register(engine: &mut Engine, parts: &Arc<PromptParts>) {
    let read = Arc::clone(parts);
    engine.register_fn("system_prompt", move || read.system());
    let read = Arc::clone(parts);
    engine.register_fn("agents_md", move || read.agents_md());
    let read = Arc::clone(parts);
    engine.register_fn("skills", move || read.skills());
    let read = Arc::clone(parts);
    engine.register_fn("tool_list", move || read.tools());
}
