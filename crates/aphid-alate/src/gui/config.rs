//! `gui.json`: what the window remembers between runs.
//!
//! One file for the machine and not one for each alate, because there is one
//! window: it lives beside `alate/` rather than inside any instance's home.
//! The tolerance is the same as [`crate::config`] — a missing file is the
//! defaults, an empty file is the defaults too, and a key this build has no
//! name for is ignored rather than fatal. Nothing here is worth refusing to
//! open a window over.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The format version written by this build.
pub const VERSION: u32 = 1;

/// Why the file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Which window the companion is.
///
/// The names are the two habits this borrows: a console that drops from the
/// top of the screen, and a panel that stands against its edge.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A bar at the top of the screen that grows downwards.
    #[default]
    Quake,
    /// A column of full height against the right edge.
    Companion,
}

impl Mode {
    /// The other one.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Mode::Quake => Mode::Companion,
            Mode::Companion => Mode::Quake,
        }
    }

    /// The word this is written as, for a status line to show.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Mode::Quake => "quake",
            Mode::Companion => "companion",
        }
    }
}

/// Which drawing of the alate is on screen.
///
/// An alate is one creature with more than one likeness, so the roster is a
/// name and not a flag: `sap` is the hand-drawn one and `drift` the turning
/// one, and a third is a new entry rather than a new field.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Familiar {
    /// The winged aphid drawn by hand, in two dimensions.
    #[default]
    Sap,
    /// The same creature as a body that turns and ripples.
    Drift,
}

impl Familiar {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Familiar::Sap => "sap",
            Familiar::Drift => "drift",
        }
    }
}

/// The whole file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub mode: Mode,
    pub familiar: Familiar,
    /// The alate the window was last pointed at. `--name` wins over it; this is
    /// what a bare `aphid alate gui` opens.
    pub instance: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: VERSION,
            mode: Mode::default(),
            familiar: Familiar::default(),
            instance: None,
        }
    }
}

impl Config {
    /// Read the file, or take the defaults.
    ///
    /// A version from a newer aphid is not refused the way `alate.json` refuses
    /// one. That file decides how an agent runs; this one decides where a
    /// window sits, and the worst a misread key can do is open it in the wrong
    /// corner.
    ///
    /// # Errors
    ///
    /// Fails when the file exists and cannot be read, or when it is not JSON.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(Error::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text).map_err(|source| Error::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Write the file, making the directory above it if it is not there.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be made or the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut text = serde_json::to_string_pretty(self).map_err(|source| Error::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        text.push('\n');
        std::fs::write(path, text).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "aphid-gui-config-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        path
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let path = temp().join("nowhere").join("gui.json");
        assert_eq!(Config::load(&path).expect("defaults"), Config::default());
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        let dir = temp();
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("empty.json");
        std::fs::write(&path, "   \n").expect("write");
        assert_eq!(Config::load(&path).expect("defaults"), Config::default());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_key_this_build_does_not_know_is_ignored() {
        let dir = temp();
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("unknown.json");
        std::fs::write(&path, r#"{"mode":"companion","wings":true}"#).expect("write");
        let config = Config::load(&path).expect("loads");
        assert_eq!(config.mode, Mode::Companion);
        assert_eq!(config.familiar, Familiar::Sap);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn what_was_written_is_what_comes_back() {
        let dir = temp();
        let path = dir.join("round-trip.json");
        let config = Config {
            version: VERSION,
            mode: Mode::Companion,
            familiar: Familiar::Drift,
            instance: Some("work".to_owned()),
        };
        config.save(&path).expect("save");
        assert_eq!(Config::load(&path).expect("load"), config);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_mode_toggles_and_comes_back() {
        assert_eq!(Mode::Quake.toggled(), Mode::Companion);
        assert_eq!(Mode::Quake.toggled().toggled(), Mode::Quake);
    }
}
