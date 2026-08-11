//! The steady interval an alate wakes on.
//!
//! One clock, one interval from `alate.json`, and one place it wakes: the
//! resident session, which lives as long as the daemon does. That is what makes
//! an alate resident rather than new every quarter of an hour — the heartbeat
//! comes back to a conversation that remembers this morning.
//!
//! Anything the agent wants to happen at a *particular* time belongs in
//! [`cron`](crate::cron) instead, which runs it in a session of its own. This
//! is only the pulse.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config;

/// The format version written by this build.
pub const VERSION: u32 = 1;

/// What is said on a wake nobody wrote a line for.
pub const DEFAULT_PROMPT: &str = "You woke on your own; nobody is waiting on an answer. Look at \
                                  your memory and at anything you said you would come back to. \
                                  Do the thing that is due, or say in one line that nothing is, \
                                  and stop. Use `cron` if something should happen at a \
                                  particular time rather than now.";

/// What is kept between runs of the daemon.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct State {
    version: u32,
    /// When the last wake happened, which is what the interval counts from.
    last: Option<DateTime<Utc>>,
}

/// The alate's pulse.
pub struct Schedule {
    path: PathBuf,
    every: Option<Duration>,
    /// The line said on a wake.
    prompt: String,
    /// When this alate woke up, which is what the first interval is measured
    /// from. Fixed at open and never moved: reading the clock each time
    /// [`next`](Self::next) is asked would push the first heartbeat one
    /// interval into the future on every call, and it would never arrive.
    started: DateTime<Utc>,
    state: State,
}

impl Schedule {
    /// Read the pulse for a home.
    ///
    /// A missing or unreadable `state.json` is a fresh clock. The file is the
    /// alate's own note to itself, and a corrupt one must not stop it starting.
    #[must_use]
    pub fn open(path: &Path, heartbeat: &config::Heartbeat, fallback: Option<String>) -> Self {
        let state = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<State>(&text).ok())
            .unwrap_or_default();

        Self {
            path: path.to_path_buf(),
            every: heartbeat.interval().unwrap_or_default(),
            prompt: heartbeat
                .prompt
                .clone()
                .or(fallback)
                .unwrap_or_else(|| DEFAULT_PROMPT.to_owned()),
            started: Utc::now(),
            state,
        }
    }

    /// When the next wake is due, or `None` when there is no interval.
    #[must_use]
    pub fn next(&self) -> Option<DateTime<Utc>> {
        let every = self.every?;
        let from = self.state.last.unwrap_or(self.started);
        Some(from + chrono::Duration::from_std(every).unwrap_or(chrono::Duration::zero()))
    }

    /// The prompt for a wake that is due now, or nothing.
    ///
    /// Taking it *is* the wake: the clock starts again from `now`, so a wake
    /// that came late does not immediately come again.
    pub fn due(&mut self, now: DateTime<Utc>) -> Option<String> {
        let next = self.next()?;
        if next > now {
            return None;
        }

        self.state.last = Some(now);
        self.save();
        Some(self.prompt.clone())
    }

    /// Write the pulse back.
    ///
    /// Failure is swallowed on purpose, and only here: a full disk should cost
    /// an alate the memory of when it last woke, not the session it is in.
    fn save(&mut self) {
        self.state.version = VERSION;
        if let Ok(text) = serde_json::to_string_pretty(&self.state) {
            let _ = aphid_core::catalog::write_atomically(&self.path, &format!("{text}\n"));
        }
    }
}
