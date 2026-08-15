//! A plugin's settings and its memory.
//!
//! Two maps, deliberately asymmetric. `config` is read-only and comes from a
//! file the user writes; `state` is what the plugin itself remembers, and the
//! host writes it back. Keeping them apart means a plugin can never corrupt its
//! own settings, and a user editing settings can never be surprised by the
//! plugin having rewritten them.
//!
//! ```text
//! .aphid/plugins/<name>.json          # config, project then home
//! .aphid/plugins/state/<name>.json    # state, written by the host
//! ```

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rhai::Map;

use crate::convert;

/// A plugin's persisted state.
///
/// Written back only when a script actually called `save_state`, so a session
/// full of read-only plugins touches the disk not at all.
pub struct Store {
    inner: Mutex<Inner>,
    path: Option<PathBuf>,
}

struct Inner {
    state: Map,
    dirty: bool,
    /// Increased on every write. Callers cache a rendered projection by this.
    version: u64,
}

impl Store {
    /// Load a plugin's state, if it has any on disk.
    ///
    /// Unreadable or malformed state starts empty rather than failing: state is
    /// the plugin's own scratch space, and refusing to start a session over it
    /// would be out of all proportion.
    #[must_use]
    pub fn load(dir: Option<&Path>, name: &str) -> Self {
        let path = dir.map(|dir| dir.join(format!("{name}.json")));
        let state = path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .map(|value| convert::object_to_map(&value))
            .unwrap_or_default();

        Self {
            inner: Mutex::new(Inner {
                state,
                dirty: false,
                version: 0,
            }),
            path,
        }
    }

    /// The state as a script sees it: a copy, so a script holding one past the
    /// hook cannot mutate the host's copy behind its back.
    #[must_use]
    pub fn get(&self) -> Map {
        self.inner
            .lock()
            .map(|inner| inner.state.clone())
            .unwrap_or_default()
    }

    /// Replace the state and mark it for persistence.
    pub fn set(&self, state: Map) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.state = state;
            inner.dirty = true;
            inner.version = inner.version.wrapping_add(1);
        }
    }

    /// Replace the in-memory state without marking it for persistence.
    pub fn set_memory(&self, state: Map) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.state = state;
            inner.version = inner.version.wrapping_add(1);
        }
    }

    /// The current state version.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.inner
            .lock()
            .map(|inner| inner.version)
            .unwrap_or_default()
    }

    /// Write the state back, if it changed and there is anywhere to write it.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be written.
    pub fn flush(&self) -> std::io::Result<()> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        let Ok(mut inner) = self.inner.lock() else {
            return Ok(());
        };
        if !inner.dirty {
            return Ok(());
        }

        let document = convert::to_json(&rhai::Dynamic::from_map(inner.state.clone()));
        let text = serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_owned());

        // The same atomic write the model catalog uses: a session killed
        // mid-write leaves the old state, never half of the new one.
        aphid_core::catalog::write_atomically(path, &text)?;
        inner.dirty = false;
        Ok(())
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("path", &self.path).finish()
    }
}

/// Read a plugin's settings file, searching each directory in turn.
///
/// The first hit wins, so a project setting shadows a personal one exactly as a
/// project plugin shadows a personal plugin.
#[must_use]
pub fn config(dirs: &[PathBuf], name: &str) -> Map {
    for dir in dirs {
        let path = dir.join(format!("{name}.json"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            return convert::object_to_map(&value);
        }
    }
    Map::new()
}
