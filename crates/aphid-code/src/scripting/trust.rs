//! Whether a repository's plugins may run.
//!
//! Cloning a repository should not hand it a shell. Plugins under `~/.aphid` are
//! the user's own and always load; plugins inside a workspace are code that
//! arrived with the checkout, so the first time one is seen the user is asked,
//! and the answer is remembered in `~/.aphid/trust.json`.
//!
//! This gates **loading**, not what a loaded plugin may do. It stops a
//! repository quietly changing what the agent does before anybody looked at it.
//! It does not make a plugin somebody approved safe, and it is no defence at all
//! against what the model itself decides to run.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file, under the home directory.
pub const FILE_NAME: &str = "trust.json";
/// Bumped when the shape changes, so an older aphid says so rather than guessing.
pub const VERSION: u32 = 1;

/// What has been decided about a directory.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Trust {
    Trusted,
    Refused,
    /// Nobody has been asked yet.
    Unknown,
}

/// Every decision made so far.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decisions {
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

impl Default for Decisions {
    fn default() -> Self {
        Self {
            version: VERSION,
            entries: Vec::new(),
        }
    }
}

/// One directory, and what was said about it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub trusted: bool,
}

/// Where decisions are kept.
#[must_use]
pub fn path(home: &Path) -> PathBuf {
    home.join(".aphid").join(FILE_NAME)
}

/// Read the decisions file, or start empty.
///
/// A file that cannot be read or parsed yields no decisions, which means the
/// user is asked again — the safe direction, and better than refusing to start.
#[must_use]
pub fn load(store: &Path) -> Decisions {
    std::fs::read_to_string(store)
        .ok()
        .and_then(|text| serde_json::from_str::<Decisions>(&text).ok())
        .filter(|decisions| decisions.version <= VERSION)
        .unwrap_or_default()
}

/// What was decided about a project directory.
#[must_use]
pub fn decision(store: &Path, project: &Path) -> Trust {
    let project = canonical(project);
    load(store)
        .entries
        .iter()
        .find(|entry| entry.path == project)
        .map_or(Trust::Unknown, |entry| {
            if entry.trusted {
                Trust::Trusted
            } else {
                Trust::Refused
            }
        })
}

/// Record a decision, replacing any earlier one for the same directory.
///
/// # Errors
///
/// Fails when the file cannot be written.
pub fn record(store: &Path, project: &Path, trusted: bool) -> std::io::Result<()> {
    let project = canonical(project);
    let mut decisions = load(store);

    decisions.version = VERSION;
    decisions.entries.retain(|entry| entry.path != project);
    decisions.entries.push(Entry {
        path: project,
        trusted,
    });

    let text = serde_json::to_string_pretty(&decisions)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    aphid_core::catalog::write_atomically(store, &text)
}

/// The path as it will be compared, resolved where the filesystem allows.
///
/// Symlinks and `..` would otherwise let the same directory be trusted twice
/// under two names, which is how a stale decision ends up applying to something
/// the user never saw.
fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);

            let path = std::env::temp_dir().join(format!(
                "aphid-trust-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_unseen_directory_is_unknown() {
        let scratch = Scratch::new();
        let store = scratch.0.join("trust.json");
        assert_eq!(decision(&store, &scratch.0), Trust::Unknown);
    }

    #[test]
    fn a_decision_is_remembered_and_can_be_changed() {
        let scratch = Scratch::new();
        let store = scratch.0.join("trust.json");

        record(&store, &scratch.0, true).expect("record");
        assert_eq!(decision(&store, &scratch.0), Trust::Trusted);

        record(&store, &scratch.0, false).expect("record again");
        assert_eq!(decision(&store, &scratch.0), Trust::Refused);
        assert_eq!(load(&store).entries.len(), 1, "not appended twice");
    }

    #[test]
    fn an_unreadable_file_means_asking_again() {
        let scratch = Scratch::new();
        let store = scratch.0.join("trust.json");
        std::fs::write(&store, "{ this is not json").expect("write");

        assert_eq!(decision(&store, &scratch.0), Trust::Unknown);
    }

    #[test]
    fn a_file_from_a_newer_aphid_is_not_guessed_at() {
        let scratch = Scratch::new();
        let store = scratch.0.join("trust.json");
        std::fs::write(&store, r#"{"version":99,"entries":[]}"#).expect("write");

        assert_eq!(decision(&store, &scratch.0), Trust::Unknown);
    }
}
