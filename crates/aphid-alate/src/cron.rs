//! What the alate has told itself to do, and when.
//!
//! A crontab entry is a name, a five-field schedule and a prompt. When an entry
//! comes due the daemon opens a **session of its own** for it and runs the
//! prompt there, so a job never reads or disturbs the conversation somebody is
//! having. That is the whole reason this replaced the older `wake` tool, which
//! could only nudge one shared appointment.
//!
//! The file is `cron.json` in the home. The `cron` tool writes it and a person
//! can edit it; there is one of it, so there is never a question of which
//! crontab won.
//!
//! Times are **local**. `0 9 * * *` is nine in the morning where the machine
//! is, which is what cron has always meant.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aphid_agent::rt::{Component, Composition, Context};
use aphid_agent::{ToolHandler, ToolOutcome, Toolbox, tool_fn};
use chrono::{DateTime, Local};
use croner::Cron;
use croner::parser::{CronParser, Seconds};
use serde::{Deserialize, Serialize};

/// The format version written by this build.
pub const VERSION: u32 = 1;

/// The most entries one alate may hold.
///
/// A bound so a model in a loop cannot fill the disk, and high enough that
/// nobody legitimate will meet it.
pub const MAX_ENTRIES: usize = 64;

/// One scheduled job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    /// A five-field cron expression: minute, hour, day of month, month, day of
    /// week.
    pub schedule: String,
    pub prompt: String,
    /// When this last ran. Absent until it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<DateTime<Local>>,
}

/// The whole file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct File {
    version: u32,
    entries: Vec<Entry>,
}

impl Default for File {
    fn default() -> Self {
        Self {
            version: VERSION,
            entries: Vec::new(),
        }
    }
}

/// The jobs, and when each is next due.
pub struct Crontab {
    path: PathBuf,
    entries: Vec<Entry>,
    /// When this was opened. An entry that has never run measures from here, so
    /// a daemon that has just started does not fire every job it has.
    opened: DateTime<Local>,
}

impl Crontab {
    /// Read the crontab. A missing file is an empty one.
    ///
    /// Returns what could not be read as well. A crontab somebody edited by
    /// hand into nonsense must not stop the alate from starting, but it must
    /// not be silently ignored either: the diagnostics reach the gateway as
    /// notices.
    #[must_use]
    pub fn open(path: &Path) -> (Self, Vec<String>) {
        let mut problems = Vec::new();
        let entries = match std::fs::read_to_string(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                problems.push(format!("{}: {error}", path.display()));
                Vec::new()
            }
            Ok(text) if text.trim().is_empty() => Vec::new(),
            Ok(text) => match serde_json::from_str::<File>(&text) {
                Ok(file) if file.version > VERSION => {
                    problems.push(format!(
                        "{}: version {} was written by a newer aphid; this one understands \
                         {VERSION}",
                        path.display(),
                        file.version
                    ));
                    Vec::new()
                }
                Ok(file) => file.entries,
                Err(error) => {
                    problems.push(format!("{}: {error}", path.display()));
                    Vec::new()
                }
            },
        };

        // An entry whose schedule no longer parses is dropped, and said out
        // loud. Keeping it would mean checking it against the clock for ever
        // and failing every time.
        let mut kept = Vec::new();
        for entry in entries {
            match parse(&entry.schedule) {
                Ok(_) => kept.push(entry),
                Err(reason) => problems.push(format!(
                    "cron {}: {reason}; the entry was dropped",
                    entry.name
                )),
            }
        }

        (
            Self {
                path: path.to_path_buf(),
                entries: kept,
                opened: Local::now(),
            },
            problems,
        )
    }

    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// Add a job, replacing any of the same name.
    ///
    /// # Errors
    ///
    /// Fails when the name, the schedule or the prompt is not one a crontab may
    /// hold.
    pub fn set(&mut self, name: &str, schedule: &str, prompt: &str) -> Result<Entry, String> {
        let name = name.trim();
        crate::home::check_name(name).map_err(|error| error.to_string())?;

        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("a job needs a prompt to run".to_owned());
        }
        parse(schedule)?;

        if self.find(name).is_none() && self.entries.len() >= MAX_ENTRIES {
            return Err(format!("an alate cannot hold more than {MAX_ENTRIES} jobs"));
        }

        let entry = Entry {
            name: name.to_owned(),
            schedule: schedule.trim().to_owned(),
            prompt: prompt.to_owned(),
            // A rewritten job starts again: its old `last` belongs to a
            // schedule that no longer exists.
            last: None,
        };
        self.entries.retain(|held| held.name != name);
        self.entries.push(entry.clone());
        self.entries
            .sort_by(|left, right| left.name.cmp(&right.name));
        self.save();
        Ok(entry)
    }

    /// Drop a job. `true` when there was one.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.name != name.trim());
        let removed = self.entries.len() != before;
        if removed {
            self.save();
        }
        removed
    }

    /// When a job next runs.
    #[must_use]
    pub fn next_for(&self, entry: &Entry, after: DateTime<Local>) -> Option<DateTime<Local>> {
        parse(&entry.schedule)
            .ok()?
            .find_next_occurrence(&after, false)
            .ok()
    }

    /// The jobs due now, marked as run.
    ///
    /// A job that was missed while the alate was stopped runs **once** when it
    /// comes back, not once for every occurrence that passed: `last` moves to
    /// now, not to the moment that was missed. A daily job and a week of
    /// downtime is one run, which is what anybody wants and what a naive
    /// catch-up gets wrong.
    pub fn due(&mut self, now: DateTime<Local>) -> Vec<Entry> {
        let opened = self.opened;
        let mut fired = Vec::new();

        for entry in &mut self.entries {
            let after = entry.last.unwrap_or(opened);
            let Some(next) = parse(&entry.schedule)
                .ok()
                .and_then(|cron| cron.find_next_occurrence(&after, false).ok())
            else {
                continue;
            };
            if next <= now {
                entry.last = Some(now);
                fired.push(entry.clone());
            }
        }

        if !fired.is_empty() {
            self.save();
        }
        fired
    }

    /// The jobs, for the system prompt.
    ///
    /// The schedule and what it means in words, so the model can check its own
    /// arithmetic, and the prompt, so it knows what it already asked itself to
    /// do and does not schedule it twice.
    #[must_use]
    pub fn prompt_section(&self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let mut text = String::from(
            "\n\n<scheduled_jobs>\nJobs you have scheduled. Each runs in a session of its own, \
             which will not remember this conversation. Use `cron` to add one, change one, or \
             remove one.\n",
        );
        for entry in &self.entries {
            let meaning = parse(&entry.schedule)
                .map(|cron| cron.describe())
                .unwrap_or_default();
            text.push_str(&format!(
                "{} — {} ({}) — {}\n",
                entry.name,
                entry.schedule,
                meaning.trim().trim_end_matches('.'),
                entry.prompt
            ));
        }
        text.push_str("</scheduled_jobs>");
        Some(text)
    }

    /// Write the crontab back.
    ///
    /// Through a temporary sibling and a rename, so a daemon killed mid-write
    /// cannot leave a crontab that then fails to load.
    fn save(&self) {
        let file = File {
            version: VERSION,
            entries: self.entries.clone(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&file) {
            let _ = aphid_core::catalog::write_atomically(&self.path, &format!("{text}\n"));
        }
    }
}

/// Read a schedule, in the one dialect this accepts.
///
/// Seconds are refused on purpose. croner takes them optionally, which would
/// make `0 0 9 * * *` parse as "at nine, on the second" rather than as the
/// six-field pattern somebody meant — a job an hour early every day, and no
/// error to explain it. Five fields, or a message saying so.
///
/// # Errors
///
/// Fails with the sentence to show whoever wrote the schedule.
pub fn parse(schedule: &str) -> Result<Cron, String> {
    let schedule = schedule.trim();
    if schedule.is_empty() {
        return Err("a job needs a schedule, such as `0 9 * * *`".to_owned());
    }
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .build()
        .parse(schedule)
        .map_err(|error| {
            format!(
                "{schedule:?} is not a schedule: {error}. Use five fields — minute, hour, day of \
                 month, month, day of week — as in `0 9 * * *` or `*/15 * * * MON-FRI`"
            )
        })
}

/// A crontab shared by the daemon loop and the `cron` tool.
pub type Shared = Arc<Mutex<Crontab>>;

/// Take the lock, and take it even when another thread panicked holding it.
///
/// The file on disk is whole either way, and refusing every later job would
/// turn one failed tool call into an alate that never wakes again.
pub(crate) fn lock(crontab: &Shared) -> std::sync::MutexGuard<'_, Crontab> {
    match crontab.lock() {
        Ok(crontab) => crontab,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Debug, Deserialize)]
pub struct CronParams {
    pub name: String,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// `cron` — schedule a prompt, change one, or remove one.
#[must_use]
pub fn cron_tool(crontab: Shared) -> impl ToolHandler {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "What to call this job. Scheduling under a name that exists \
                                replaces it."
            },
            "schedule": {
                "type": "string",
                "description": "Five fields — minute, hour, day of month, month, day of week — \
                                in local time, as in `0 9 * * *`. Use `off` to remove the job."
            },
            "prompt": {
                "type": "string",
                "description": "What to do when it runs. Write it for someone who will not \
                                remember this conversation."
            }
        },
        "required": ["name"],
        "additionalProperties": false
    });
    let description = "Schedule a prompt to run later, again and again. It runs in a session of \
                       its own, which starts empty and will not remember what you are doing now \
                       — so put everything it needs in the prompt. Your memory is shared with \
                       it, so a job can write facts you will recall afterwards. Use `off` as the \
                       schedule to remove a job."
        .to_owned();

    tool_fn(
        "cron",
        description,
        schema,
        move |params: CronParams, _cx| {
            let crontab = crontab.clone();
            async move {
                let schedule = params.schedule.unwrap_or_default();
                let mut crontab = lock(&crontab);

                // `off` removes, the way `heartbeat.every` turns the heartbeat off.
                // One spelling for "not any more", across the whole configuration.
                if matches!(schedule.trim(), "off" | "never" | "none" | "") {
                    return if crontab.remove(&params.name) {
                        ToolOutcome::text(format!("{} will not run again", params.name))
                    } else {
                        ToolOutcome::error(format!(
                            "there is no job called {}; a schedule is needed to make one",
                            params.name
                        ))
                    };
                }

                let entry = match crontab.set(
                    &params.name,
                    &schedule,
                    params.prompt.as_deref().unwrap_or_default(),
                ) {
                    Ok(entry) => entry,
                    Err(error) => return ToolOutcome::error(error),
                };

                // The next run, said back, so the model's reading of its own
                // expression is checked against what will actually happen.
                match crontab.next_for(&entry, Local::now()) {
                    Some(next) => ToolOutcome::text(format!(
                        "{} is scheduled: {}. It runs next at {}",
                        entry.name,
                        entry.schedule,
                        next.format("%Y-%m-%d %H:%M %Z")
                    )),
                    None => ToolOutcome::text(format!(
                        "{} is scheduled: {}. Nothing matches it in the next hundred years, so it \
                     may never run",
                        entry.name, entry.schedule
                    )),
                }
            }
        },
    )
}

/// Ships the `cron` tool, and nothing else.
///
/// It subscribes to no hooks: the crontab is read by the daemon loop, not by
/// anything inside a run.
pub struct CronComponent {
    crontab: Shared,
    tools: Arc<Toolbox>,
}

impl CronComponent {
    #[must_use]
    pub fn new(crontab: Shared, composition: &Composition) -> Self {
        Self {
            crontab,
            tools: Arc::clone(&composition.tools),
        }
    }
}

impl Component for CronComponent {
    fn name(&self) -> &str {
        "cron"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        self.tools
            .contribute(ctx, Arc::new(cron_tool(self.crontab.clone())));
        Ok(())
    }
}
