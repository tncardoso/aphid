//! `alate.json`: what one instance is.
//!
//! The shape follows [`aphid_core::catalog`], because that file taught the
//! lesson already: a missing file is the defaults, an empty file is the defaults
//! too — that is what a truncated write leaves behind — and a file written by a
//! newer aphid is refused by name rather than half-read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aphid_core::ThinkingLevel;
use serde::{Deserialize, Serialize};

/// The format version written by this build.
pub const VERSION: u32 = 1;

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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// The model to run, by the name `aphid model` shows. The catalog's own
    /// default when this is absent.
    pub model: Option<String>,
    pub thinking: Option<Thinking>,
    /// Where the agent works. The home itself when this is absent, which keeps
    /// a fresh alate inside its own directory.
    pub workspace: Option<PathBuf>,
    pub permissions: Permissions,
    pub heartbeat: Heartbeat,
    pub memory: MemoryConfig,
    pub gateway: Gateway,
    /// Literal environment values passed to sandboxed commands. Host values are
    /// deliberately configured in the user-owned sandbox policy instead.
    pub environment: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: VERSION,
            model: None,
            thinking: Some(Thinking::Medium),
            workspace: None,
            permissions: Permissions::default(),
            heartbeat: Heartbeat::default(),
            memory: MemoryConfig::default(),
            gateway: Gateway::default(),
            environment: BTreeMap::new(),
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
}

/// How hard the model thinks.
///
/// A copy of the ladder rather than [`ThinkingLevel`] itself, for the reason
/// [`aphid_core::catalog::ModelEntry`] gives: the runtime type is a layout, and
/// this one is what somebody has to type into a file.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Thinking {
    /// The runtime level, or `None` for [`Thinking::Off`].
    #[must_use]
    pub fn level(self) -> Option<ThinkingLevel> {
        Some(match self {
            Thinking::Off => return None,
            Thinking::Minimal => ThinkingLevel::Minimal,
            Thinking::Low => ThinkingLevel::Low,
            Thinking::Medium => ThinkingLevel::Medium,
            Thinking::High => ThinkingLevel::High,
            Thinking::Xhigh => ThinkingLevel::XHigh,
            Thinking::Max => ThinkingLevel::Max,
        })
    }
}

/// What happens when a tool asks permission.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permissions {
    /// Ask whoever is attached. With nobody attached there is nobody to ask, so
    /// the call is refused — an unattended alate must not be able to talk
    /// itself into anything.
    #[default]
    Ask,
    Allow,
    Deny,
}

/// When the agent wakes itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Heartbeat {
    /// How long between wakes: `15m`, `2h`, `30s`, or `off` for none.
    pub every: String,
    /// What to say on a wake. `HEARTBEAT.md` in the home, then a built-in line,
    /// when this is absent.
    pub prompt: Option<String>,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self {
            every: "15m".to_owned(),
            prompt: None,
        }
    }
}

impl Heartbeat {
    /// The interval, or `None` when the alate only wakes when it asked to.
    ///
    /// # Errors
    ///
    /// Fails when `every` is not a duration.
    pub fn interval(&self) -> Result<Option<Duration>, String> {
        duration(&self.every)
    }
}

/// How much of the memory to offer unasked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// How many facts are recalled and offered for each prompt. Zero turns
    /// automatic recall off and leaves the `recall` tool.
    pub recall: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { recall: 5 }
    }
}

/// The socket terminals attach to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gateway {
    /// `gateway.sock` in the home when absent.
    pub socket: Option<PathBuf>,
    /// A Telegram bot on the same gateway. Absent means no bot.
    pub telegram: Option<Telegram>,
    /// A colony on the same gateway. Absent means this alate talks to no other
    /// agents.
    pub colony: Option<Colony>,
}

/// A Telegram bot that speaks to this alate.
///
/// Kept in every build, and not behind the `telegram` feature, so one
/// `alate.json` is the same file whichever build reads it. A build without the
/// feature says so and carries on rather than failing to parse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Telegram {
    /// The variable the bot token is read from, never the token itself. The
    /// same rule the model keys follow: a configuration file is copied and
    /// shared, and a token in it goes with it.
    pub token_env: String,
    /// The chats that may talk to this alate, by id. An empty list allows
    /// nobody: whoever can reach the bot can make the agent run commands, so
    /// the default has to be closed. A chat that is refused is told its id.
    pub chats: Vec<i64>,
    /// How long one `getUpdates` waits for something to happen.
    pub poll: String,
    /// Post a line for each tool call, so a long run is legible from a phone.
    pub tools: bool,
    /// The API root. `https://api.telegram.org` when absent; a test server
    /// otherwise.
    pub api: Option<String>,
}

/// The variable a bot token is read from when the configuration names none.
pub const TOKEN_ENV: &str = "TELEGRAM_BOT_TOKEN";

/// The variable a colony key is read from when the configuration names none.
pub const COLONY_KEY_ENV: &str = "APHID_COLONY_KEY";

/// The colony this alate talks in.
///
/// A second client on the same gateway, exactly as [`Telegram`] is. Kept in
/// every build and **not** behind the `colony` feature, so one `alate.json` is
/// the same file whichever build reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Colony {
    /// Where the relay is.
    pub relay: String,
    /// The variable this agent's secret key is read from, in hex or as an
    /// `nsec`. Never the key itself: the same rule the bot token follows.
    pub key_env: String,
    /// The channels to join at start-up. An empty list joins nothing and
    /// watches whatever this agent has already been added to, which is what an
    /// agent invited from the terminal wants.
    pub channels: Vec<String>,
    /// What this agent publishes as its name, so a chat shows a word and not
    /// the head of a key. The instance's own name when absent.
    pub name: Option<String>,
    /// Wake on a mention in a channel.
    ///
    /// A direct message always wakes it, whatever this says: a message to you
    /// and to nobody else is not something to read later.
    pub mentions: bool,
    /// How long to wait before dialling again after the colony goes.
    pub retry: String,
}

impl Default for Colony {
    fn default() -> Self {
        Self {
            relay: "ws://127.0.0.1:7777".to_owned(),
            key_env: COLONY_KEY_ENV.to_owned(),
            channels: Vec::new(),
            name: None,
            mentions: true,
            retry: "5s".to_owned(),
        }
    }
}

impl Colony {
    /// How long to wait before dialling again.
    ///
    /// # Errors
    ///
    /// Fails when `retry` is not a duration, or when it is `off` — a wait of no
    /// length is a loop that dials as fast as it can fail.
    pub fn interval(&self) -> Result<Duration, String> {
        match duration(&self.retry)? {
            Some(interval) => Ok(interval),
            None => Err(format!(
                "{:?} is not a length of time to wait for; gateway.colony.retry \
                 is how long to wait before dialling again, such as \"5s\"",
                self.retry
            )),
        }
    }
}

impl Default for Telegram {
    fn default() -> Self {
        Self {
            token_env: TOKEN_ENV.to_owned(),
            chats: Vec::new(),
            poll: "25s".to_owned(),
            tools: false,
            api: None,
        }
    }
}

impl Telegram {
    /// How long one `getUpdates` waits.
    ///
    /// # Errors
    ///
    /// Fails when `poll` is not a duration, or when it is `off` — a poll of no
    /// length is a loop that asks Telegram as fast as it can answer.
    pub fn interval(&self) -> Result<Duration, String> {
        match duration(&self.poll)? {
            Some(interval) => Ok(interval),
            None => Err(format!(
                "{:?} is not a length of time to wait for; gateway.telegram.poll \
                 is how long one request holds open, such as \"25s\"",
                self.poll
            )),
        }
    }
}

/// Read a duration such as `30s`, `15m`, `2h` or `1d`.
///
/// `off`, `never`, `none` and `0` all mean no duration at all. A bare number is
/// seconds, because the alternative is guessing.
///
/// # Errors
///
/// Fails with a sentence naming what could not be read.
pub fn duration(text: &str) -> Result<Option<Duration>, String> {
    let text = text.trim();
    if text.is_empty() || matches!(text, "off" | "never" | "none" | "0") {
        return Ok(None);
    }

    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, "s"),
    };

    let count: u64 = digits
        .parse()
        .map_err(|_| format!("{text:?} does not start with a number"))?;
    let seconds = match unit.trim() {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 60 * 60,
        "d" | "day" | "days" => 24 * 60 * 60,
        other => return Err(format!("{other:?} is not a unit of time; use s, m, h or d")),
    };

    match count.checked_mul(seconds) {
        // `15m` and `0m` differ: one is an interval, the other is off. Both are
        // written by hand, so both have to mean what they look like.
        Some(0) => Ok(None),
        Some(total) => Ok(Some(Duration::from_secs(total))),
        None => Err(format!("{text:?} is longer than any wait can be")),
    }
}
