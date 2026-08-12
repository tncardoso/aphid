//! `colony.json`: what one hub is.
//!
//! The shape follows [`aphid_alate::config`], because that file taught the
//! lesson already: a missing file is the defaults, an empty file is the defaults
//! too — that is what a truncated write leaves behind — and a file written by a
//! newer aphid is refused by name rather than half-read.
//!
//! [`aphid_alate::config`]: https://docs.rs/aphid-alate

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The format version written by this build.
pub const VERSION: u32 = 1;

/// Where a colony listens when its file says nothing.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7777";

/// The channel a fresh colony is made with, so there is somewhere to talk.
pub const DEFAULT_CHANNEL: &str = "general";

/// How many messages one group keeps when nothing says otherwise.
pub const DEFAULT_HISTORY: usize = 5_000;

/// Why a configuration could not be read.
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
    #[error("{path}: version {found} was written by a newer aphid; this one understands {VERSION}")]
    Version { path: PathBuf, found: u32 },
}

/// The whole file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// Where the relay listens.
    ///
    /// Loopback, and it matters: **everything that reaches this socket may
    /// publish and read**. A colony asks nobody who they are, so the network
    /// interface is the whole of the access control and it has to be right.
    pub listen: String,
    /// What the person at this terminal is called, published as a kind 0.
    /// Absent shows an npub, which is nobody's idea of a name.
    pub name: Option<String>,
    /// The channels made at start-up if they are not there.
    pub channels: Vec<String>,
    /// The most messages kept for one group. Older ones are trimmed at
    /// start-up, never on the hot path.
    pub history: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: VERSION,
            listen: DEFAULT_LISTEN.to_owned(),
            name: None,
            channels: vec![DEFAULT_CHANNEL.to_owned()],
            history: DEFAULT_HISTORY,
        }
    }
}

impl Config {
    /// Read the file. A missing or empty file is the defaults, not a failure.
    ///
    /// # Errors
    ///
    /// Fails when the file exists but cannot be read or parsed, or when it was
    /// written by a newer aphid.
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

        let config: Self = serde_json::from_str(&text).map_err(|source| Error::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if config.version > VERSION {
            return Err(Error::Version {
                path: path.to_path_buf(),
                found: config.version,
            });
        }
        Ok(config)
    }

    /// Write the file, through a temporary sibling and a rename.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created or the file cannot be written.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        aphid_core::catalog::write_atomically(path, &text)
    }

    /// Where to listen.
    ///
    /// `listen` is a string in the file and not a [`SocketAddr`], so a typo is
    /// this sentence rather than a serde failure two layers away from the
    /// person who typed it.
    ///
    /// # Errors
    ///
    /// Fails when `listen` is not an address and a port.
    pub fn address(&self) -> Result<SocketAddr, String> {
        self.listen.parse().map_err(|_| {
            format!(
                "{:?} is not an address and a port, such as {DEFAULT_LISTEN}",
                self.listen
            )
        })
    }
}
