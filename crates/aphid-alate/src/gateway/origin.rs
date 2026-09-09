//! Where a scheduled job writes back to.
//!
//! A job runs in a session of its own that nobody is watching, so what it says
//! reaches nobody. An [`Origin`] is the conversation it may speak into: the one
//! that scheduled it.
//!
//! A conversation is the thing addressed, and not a channel. A channel is a
//! property of a session — [`Kind::Attached`]'s `channel` — so addressing the
//! session puts a Telegram chat, a colony channel, a terminal and the resident
//! conversation under one rule instead of four.
//!
//! [`Kind::Attached`]: crate::sessions::Kind::Attached

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// The conversation a job may write back to.
///
/// Two fields for one thing: a session, and the name by which it comes back. A
/// session id does not survive a restart of the daemon — an attached session
/// dies with its connection, and the resident one is opened again with a new id
/// — so the id alone would name a conversation that is only sometimes there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// The session that scheduled the job.
    pub session: String,
    /// What it was called, from `Kind::label`. This is what finds it again
    /// after a restart, when the id is gone.
    pub label: String,
}

impl Origin {
    #[must_use]
    pub fn new(session: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            label: label.into(),
        }
    }
}

/// A label that names one conversation and not a kind of conversation.
///
/// A terminal that says nothing when it attaches is listed as `attached`, and
/// several of them carry that same word. Resolving by label would then be a
/// guess between them, so the generic ones are refused and only the id is left
/// to go on.
fn nameable(label: &str) -> bool {
    !matches!(label, "attached" | "")
}

/// The conversations that are open now: id to label.
///
/// Kept up to date by the daemon at the two points that already announce a
/// session opening and closing, so what it holds is what the clients were told.
#[derive(Debug, Default)]
pub struct Directory {
    open: HashMap<String, String>,
}

/// The directory, shared by the daemon loop and the sessions that deliver.
pub type Shared = Arc<RwLock<Directory>>;

impl Directory {
    pub fn opened(&mut self, id: &str, label: &str) {
        self.open.insert(id.to_owned(), label.to_owned());
    }

    pub fn closed(&mut self, id: &str) {
        self.open.remove(id);
    }

    /// The live session this origin names.
    ///
    /// The id first, while the session that scheduled the job is still open.
    /// Then the one open session carrying that label, which is what makes a
    /// Telegram chat survive a reconnection — it comes back with the same name
    /// and a new id — and the resident conversation survive a restart. With
    /// more than one candidate there is no answer: see [`nameable`].
    #[must_use]
    pub fn resolve(&self, origin: &Origin) -> Option<String> {
        if self.open.contains_key(&origin.session) {
            return Some(origin.session.clone());
        }
        if !nameable(&origin.label) {
            return None;
        }
        let mut named = self
            .open
            .iter()
            .filter(|(_, label)| label.as_str() == origin.label);
        let (id, _) = named.next()?;
        if named.next().is_some() {
            return None;
        }
        Some(id.clone())
    }
}

/// Take the read lock, and take it even when another thread panicked holding it.
pub fn read(directory: &Shared) -> std::sync::RwLockReadGuard<'_, Directory> {
    match directory.read() {
        Ok(directory) => directory,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Take the write lock, panic or no panic.
pub fn write(directory: &Shared) -> std::sync::RwLockWriteGuard<'_, Directory> {
    match directory.write() {
        Ok(directory) => directory,
        Err(poisoned) => poisoned.into_inner(),
    }
}
