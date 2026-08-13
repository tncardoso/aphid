//! The conversations an alate is having at once.
//!
//! A session is one agent context: one [`Agent`], one transcript, one file
//! under `.aphid/sessions`. An alate hosts several rather than being one, which
//! is what lets a scheduled job run without landing in the middle of what you
//! were saying.
//!
//! Three kinds, and each ends differently:
//!
//! - [`Kind::Resident`] is made at start-up and never closed. The heartbeat
//!   wakes here, so context builds up across the day.
//! - [`Kind::Attached`] is made by a client and dies with it, and is named
//!   after the channel that client said it was. Work that should outlive a
//!   terminal does not belong here.
//! - [`Kind::Cron`] is made by a job and ends when its run ends. It starts
//!   empty every time.
//!
//! What they share is everything that is *the alate* rather than a
//! conversation: the memory, the crontab, the plugin host, the model and the
//! permission gate. A cron job can write a fact the resident session recalls an
//! hour later, which is the point of sharing a memory and not a transcript.
//!
//! A session's id is its **session file's** id, so one name identifies a
//! conversation whether it is running or long finished on disk, and `/session
//! <id>` needs no second namespace.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use aphid_agent::{Agent, AgentHandle, RunOutcome, StreamFn};
use aphid_code::harness::{self, HarnessOptions};
use aphid_code::plugins::permissions::{AllowAll, Confirmer, DenyAll, Permissions};
use aphid_code::session::{self, SessionPlugin, sessions_dir};
use aphid_code::tools::Workspace;
use aphid_plugin::PluginHost;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::config::{Config, Permissions as Wanted};
use crate::cron::{self, CronPlugin};
use crate::gateway::{GatewayPlugin, Publisher};
use crate::home::Home;
use crate::memory::{self, MemoryPlugin};

/// Why a session exists, and therefore when it ends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    /// Made at start-up, never closed. Where the heartbeat wakes.
    Resident,
    /// Made by a client, and ended when that connection closes.
    ///
    /// `channel` is what that client said it was — `telegram: 42` — and is
    /// absent for a terminal, which says nothing. It exists so a listing says
    /// **where** a conversation is being had: an alate with a bot on it has
    /// sessions that nobody at this keyboard opened.
    Attached {
        connection: u64,
        channel: Option<String>,
    },
    /// Made by a job, and ended when its run ends.
    Cron { name: String },
}

impl Kind {
    /// A word for the status line and the session list.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Kind::Resident => "resident".to_owned(),
            Kind::Attached {
                channel: Some(channel),
                ..
            } => channel.clone(),
            Kind::Attached { .. } => "attached".to_owned(),
            Kind::Cron { name } => format!("cron: {name}"),
        }
    }
}

/// What a session looks like from outside.
///
/// The kind is a word and not a [`Kind`], because a session read off the disk
/// has no kind to report — the file records what was said, not why the session
/// existed — and a client only ever prints it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub id: String,
    pub kind: String,
    /// When it started, as a local time somebody can read.
    pub started: String,
    /// Whether a run is in flight. Always false for one read off the disk.
    pub running: bool,
}

/// One conversation.
pub struct Session {
    pub id: String,
    pub kind: Kind,
    pub created: DateTime<Local>,
    /// `Some` while idle. The run in flight takes it and hands it back.
    agent: Option<Agent>,
    handle: AgentHandle,
    /// Kept so its transcript can be flushed when the session closes.
    plugin: Arc<SessionPlugin>,
    queued: VecDeque<String>,
}

impl Session {
    #[must_use]
    pub fn info(&self) -> Info {
        Info {
            id: self.id.clone(),
            kind: self.kind.label(),
            started: self.created.format("%Y-%m-%d %H:%M").to_string(),
            running: self.agent.is_none(),
        }
    }

    /// Where this session's transcript is written.
    #[must_use]
    pub fn path(&self) -> Option<std::path::PathBuf> {
        self.plugin.path()
    }

    /// Ask the run in flight to stop at its next checkpoint.
    pub fn cancel(&self) {
        self.handle.cancel();
    }

    /// Put a prompt in this session's queue.
    pub fn enqueue(&mut self, text: String) {
        self.queued.push_back(text);
    }

    /// Whether this session could start a run right now.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.agent.is_some() && !self.queued.is_empty()
    }
}

/// Every session the daemon is holding, and every run in flight.
pub struct Sessions {
    open: HashMap<String, Session>,
    /// Runs in flight, each yielding its session's id, the agent it borrowed,
    /// and what the run produced.
    running: JoinSet<(String, Agent, RunOutcome)>,
    /// Which session each running task belongs to. A task that panics gives
    /// back its id and nothing else, so the mapping has to be kept out here.
    tasks: HashMap<tokio::task::Id, String>,
    resident: Option<String>,
}

impl Sessions {
    #[must_use]
    pub fn new() -> Self {
        Self {
            open: HashMap::new(),
            running: JoinSet::new(),
            tasks: HashMap::new(),
            resident: None,
        }
    }

    /// The resident session's id, once one has been opened.
    #[must_use]
    pub fn resident(&self) -> Option<&str> {
        self.resident.as_deref()
    }

    pub fn insert(&mut self, session: Session) {
        if session.kind == Kind::Resident {
            self.resident = Some(session.id.clone());
        }
        self.open.insert(session.id.clone(), session);
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Session> {
        self.open.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Session> {
        self.open.get_mut(id)
    }

    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.open.contains_key(id)
    }

    /// Every open session, newest first.
    #[must_use]
    pub fn list(&self) -> Vec<Info> {
        let mut infos: Vec<Info> = self.open.values().map(Session::info).collect();
        infos.sort_by(|left, right| right.started.cmp(&left.started));
        infos
    }

    /// The ids of the sessions a closing connection owned.
    #[must_use]
    pub fn owned_by(&self, connection: u64) -> Vec<String> {
        self.open
            .values()
            .filter(|session| {
                matches!(&session.kind, Kind::Attached { connection: owner, .. } if *owner == connection)
            })
            .map(|session| session.id.clone())
            .collect()
    }

    /// Close a session, cancelling anything it had in flight.
    ///
    /// The transcript is already on disk — the session plugin appends as
    /// messages are committed — so this loses nothing but the context.
    pub fn close(&mut self, id: &str) -> Option<Session> {
        let session = self.open.remove(id)?;
        session.cancel();
        Some(session)
    }

    /// Start a run for every session that has one waiting and an agent to run
    /// it with.
    ///
    /// This is what makes jobs concurrent: each ready session gets its own
    /// task, and a long conversation in one holds up nothing in another.
    ///
    /// Returns the ids that started. The daemon does not announce them — the
    /// run reports its own first turn — but a test can assert on them.
    pub fn start_ready(&mut self) -> Vec<String> {
        let ready: Vec<String> = self
            .open
            .values()
            .filter(|session| session.ready())
            .map(|session| session.id.clone())
            .collect();

        for id in &ready {
            let Some(session) = self.open.get_mut(id) else {
                continue;
            };
            let (Some(agent), Some(prompt)) = (session.agent.take(), session.queued.pop_front())
            else {
                continue;
            };
            let spawned = id.clone();
            let handle = self.running.spawn(async move {
                let mut agent = agent;
                let outcome = agent.prompt(&prompt).await;
                (spawned, agent, outcome)
            });
            self.tasks.insert(handle.id(), id.clone());
        }
        ready
    }

    /// Wait for the next run to finish.
    ///
    /// `None` for the outcome means the run panicked. Its session is closed
    /// rather than left behind: the agent went with the task, so the session
    /// could never run anything again. The transcript is on disk either way.
    ///
    /// Pending for ever when nothing is running, which is what a `select!` arm
    /// wants: an empty [`JoinSet`] returning `None` at once would spin the loop.
    pub async fn finished(&mut self) -> (String, Option<RunOutcome>) {
        loop {
            // With the id, because a panicked task carries nothing else that
            // says which session it was.
            let Some(joined) = self.running.join_next_with_id().await else {
                std::future::pending::<()>().await;
                continue;
            };

            return match joined {
                Ok((task, (id, agent, outcome))) => {
                    self.tasks.remove(&task);
                    // Give the agent back, unless the session went while it ran.
                    if let Some(session) = self.open.get_mut(&id) {
                        session.agent = Some(agent);
                    }
                    (id, Some(outcome))
                }
                Err(error) => {
                    let Some(id) = self.tasks.remove(&error.id()) else {
                        continue;
                    };
                    self.open.remove(&id);
                    (id, None)
                }
            };
        }
    }

    /// Stop everything, on the way out.
    pub fn shutdown(&mut self) {
        for session in self.open.values() {
            session.cancel();
        }
        self.running.abort_all();
    }
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything needed to make a session, held once and used many times.
///
/// The parts that must happen exactly once for the whole daemon — discovering
/// and loading the scripts, installing a script backend, telling the host a
/// session began — are done by the caller and their results handed here. What
/// is left is per-session: a fresh transcript, a fresh file, and a system
/// prompt built from the memory and the crontab **as they are now**, so a job
/// that fires at nine knows what the alate learned at eight.
pub struct Blueprint {
    pub home: Home,
    pub config: Config,
    pub model: aphid_core::Model,
    pub api_key: Option<String>,
    pub workspace: Workspace,
    pub publisher: Publisher,
    pub memory: memory::Shared,
    pub crontab: cron::Shared,
    pub host: Option<Arc<PluginHost>>,
    pub stream_fn: Option<StreamFn>,
    pub processes: Arc<aphid_agent::exec::Registry>,
    /// The permission gate. One for the whole alate, so "allow always" answered
    /// in one session is remembered in the next.
    pub permissions: Arc<Permissions>,
    /// The colony, when there is one.
    ///
    /// One of the two `cfg`s the bridge costs. Telegram needed none because a
    /// chat is a gateway client and touches nothing inside a run; colony tools
    /// do run inside one.
    #[cfg(feature = "colony")]
    pub colony: Option<crate::colony::Shared>,
}

impl Blueprint {
    /// The gate a configuration asks for.
    ///
    /// `gateway` is what asks whoever is attached; the other two answer without
    /// asking anybody.
    #[must_use]
    pub fn permissions(config: &Config, gateway: Arc<dyn Confirmer>) -> Arc<Permissions> {
        let confirmer: Arc<dyn Confirmer> = match config.permissions {
            Wanted::Ask => gateway,
            Wanted::Allow => Arc::new(AllowAll),
            Wanted::Deny => Arc::new(DenyAll),
        };
        Arc::new(Permissions::new(confirmer))
    }

    /// Open a session of this kind.
    ///
    /// # Errors
    ///
    /// Fails when the session file cannot be opened.
    pub fn open(&self, kind: Kind) -> Result<Session, String> {
        let mut options = HarnessOptions::new(self.workspace.clone());
        options.system = Some(crate::prompts::SYSTEM.to_owned());
        options.cwd = self.workspace.root().to_path_buf();
        options.model = self.model.clone();
        options.thinking = self
            .config
            .thinking
            .and_then(crate::config::Thinking::level);
        options.api_key = self.api_key.clone().map(Into::into);
        options.stream_fn = self.stream_fn.clone();
        options.processes = Arc::clone(&self.processes);

        // The session file is opened first, because its id *is* the session's,
        // and the plugins below have to be told which session they speak for
        // before any of them can send a frame.
        let directory = sessions_dir(&self.workspace);
        let model_id = options.model.id.to_string();
        let (plugin, _resumed) = session::attach(&directory, &options.cwd, Some(&model_id), None)
            .map_err(|error| {
            format!(
                "could not open a session in {}: {error}",
                directory.display()
            )
        })?;
        let id = plugin.id().ok_or("the new session has no id")?;

        // The map of the memory and the list of the jobs, never their contents:
        // the agent sees what exists and calls `recall` for the rest.
        let mut appended = String::new();
        let paths = memory::lock(&self.memory).paths().unwrap_or_default();
        if let Some(section) = memory::prompt_section(&paths) {
            appended.push_str(&section);
        }
        if let Some(section) = cron::lock(&self.crontab).prompt_section() {
            appended.push_str(&section);
        }
        #[cfg(feature = "colony")]
        if let Some(section) = self
            .colony
            .as_ref()
            .and_then(|colony| colony.prompt_section())
        {
            appended.push_str(&section);
        }
        if !appended.is_empty() {
            options.append_system = Some(appended);
        }

        options.plugins.push(Arc::new(GatewayPlugin::new(
            self.publisher.for_session(&id),
        )));
        options.plugins.push(Arc::new(MemoryPlugin::new(
            self.memory.clone(),
            &self.config.memory,
        )));
        options
            .plugins
            .push(Arc::new(CronPlugin::new(self.crontab.clone())));
        // Beside the crontab, so a scheduled job can post to the colony as
        // readily as a conversation can — which is most of the point of a hub.
        #[cfg(feature = "colony")]
        if let Some(colony) = &self.colony {
            options
                .plugins
                .push(Arc::new(crate::colony::ColonyPlugin::new(colony.clone())));
        }
        // One `Permissions` for the whole alate, so "allow always" answered in
        // one session is remembered in the next.
        options.plugins.push(self.permissions.clone());

        // The host is registered in every session. Its hooks are per-run, so
        // that is right; its own *session* is the daemon's lifetime, which is
        // why `session_start` is not called again here.
        if let Some(host) = &self.host {
            options.plugins.push(host.clone());
            options.host = Some(host.clone());
        }

        options.plugins.push(plugin.clone());

        let harness = harness::build(options);
        Ok(Session {
            id,
            kind,
            created: Local::now(),
            handle: harness.agent.handle(),
            agent: Some(harness.agent),
            plugin,
            queued: VecDeque::new(),
        })
    }
}

/// The sessions on disk that are not open, newest first.
///
/// The open ones are filtered out so a list never shows one conversation twice.
#[must_use]
pub fn stored(workspace: &Workspace, open: &Sessions) -> Vec<Info> {
    session::list(&sessions_dir(workspace))
        .into_iter()
        .filter(|summary| !open.contains(&summary.header.id))
        .map(|summary| Info {
            id: summary.header.id.clone(),
            kind: "stored".to_owned(),
            started: summary
                .header
                .started
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            running: false,
        })
        .collect()
}
