//! What the agent knows between sessions.
//!
//! Three operations: write one fact, recall the facts a question needs, and
//! list the paths that hold any. A fact is one short line, and a fact belongs
//! to a path such as `/projects/aphid`.
//!
//! The facts are markdown files in the agent's own home — see [`store`] for the
//! shape and why there is no index. They sit inside the workspace, so the agent
//! reads and edits them with `read`, `write` and `edit` like anything else, and
//! so does the user with `grep`. A memory nobody can open is a memory nobody
//! can check.
//!
//! [`plugin`] is how the agent reaches it: two tools, and the recall that
//! happens whether or not it asks.

pub mod plugin;
pub mod store;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use plugin::MemoryPlugin;
pub use store::Memory;

/// The deepest a memory path may go. A tree deeper than this is a filing system
/// nobody navigates.
pub const MAX_DEPTH: usize = 8;

/// Why the memory could not answer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Path(String),
    #[error("{0}")]
    Fact(String),
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

/// One fact the memory gave back.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: String,
    pub fact: String,
    /// How well this fact answered, from 0 to 1: the share of the question's
    /// weight it carries. It orders one answer, and means nothing on its own.
    pub score: f64,
}

/// A memory shared by the tools and the plugin.
///
/// The store blocks, so this is a plain mutex and every tool takes it inside a
/// blocking task. It is never held across an await.
pub type Shared = Arc<Mutex<Memory>>;

/// Take the lock, and take it even when another thread panicked holding it.
///
/// A poisoned memory is still a readable one: the panic happened in a tool, the
/// files on disk are whole either way, and refusing every later recall would
/// turn one failed call into a dead agent.
pub(crate) fn lock(memory: &Shared) -> std::sync::MutexGuard<'_, Memory> {
    match memory.lock() {
        Ok(memory) => memory,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// A memory path, checked and in its written form.
///
/// The segments obey the rules an instance name obeys, and for the same reason:
/// nothing in the allowed set is a separator, and a leading dot is refused, so
/// no path can name a file outside the memory.
///
/// # Errors
///
/// Fails with the sentence to show whoever wrote the path.
pub fn normalise(path: &str) -> Result<String, Error> {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err(Error::Path(
            "a fact needs a path, such as /projects/aphid".to_owned(),
        ));
    }

    let segments: Vec<&str> = trimmed.split('/').filter(|part| !part.is_empty()).collect();
    if segments.len() > MAX_DEPTH {
        return Err(Error::Path(format!(
            "{path:?} goes more than {MAX_DEPTH} levels deep"
        )));
    }
    for segment in &segments {
        crate::home::check_name(segment)
            .map_err(|error| Error::Path(format!("{path:?} is not a memory path: {error}")))?;
    }
    Ok(format!("/{}", segments.join("/")))
}

/// The paths rendered for the system prompt.
///
/// The map and never the contents, the way the prompt lists a skill's name and
/// leaves its body on disk: the agent sees which topics exist and calls
/// `recall` for what is in them.
#[must_use]
pub fn prompt_section(paths: &[String]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    let mut text = String::from(
        "\n\n<memory_paths>\nYou have a memory of facts, filed under these paths. Call `recall` \
         to read what is in one, and `remember` to add to it.\n",
    );
    for path in paths {
        text.push_str(path);
        text.push('\n');
    }
    text.push_str("</memory_paths>");
    Some(text)
}
