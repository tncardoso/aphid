//! What the runtime has started, and what became of it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many finished processes are kept. Enough to answer "what just happened",
/// not so many that the list stops being a list of what is running.
pub const RECENT: usize = 4;

/// Where a process is.
///
/// One enum for both halves of a life: `Running` and `Killing` are the states a
/// process is in, the rest are how it ended. A record never needs a second type
/// when it finishes — only a new status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    /// Asked to stop, not yet reaped.
    Killing,
    Exited(i32),
    /// Died from a signal, which has no exit code to report.
    Signalled,
    TimedOut,
    /// The run it belonged to was cancelled.
    Cancelled,
    /// Stopped from `/ps`.
    Killed,
    /// Never started, or could not be waited for.
    Failed(String),
}

impl Status {
    /// Whether this is a state rather than an ending.
    #[must_use]
    pub fn running(&self) -> bool {
        matches!(self, Status::Running | Status::Killing)
    }
}

/// One command the runtime started.
#[derive(Clone, Debug)]
pub struct Process {
    /// The runtime's own numbering, never reused.
    pub id: u32,
    /// The system pid, once it has one.
    pub pid: Option<u32>,
    /// Who started it: `bash`, or the name of the plugin that called `exec`.
    pub origin: String,
    pub command: String,
    pub status: Status,
    /// Output produced, both pipes together.
    pub bytes: u64,
    started: Instant,
    finished: Option<Instant>,
    kill: Arc<AtomicBool>,
}

impl Process {
    /// Counts up while it runs, then holds at the total.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.finished.unwrap_or_else(Instant::now) - self.started
    }

    /// Whether it is still going.
    #[must_use]
    pub fn running(&self) -> bool {
        self.status.running()
    }
}

/// The handle [`run`](crate::run) keeps while a process is alive.
pub(crate) struct Entry {
    pub(crate) id: u32,
    pub(crate) kill: Arc<AtomicBool>,
}

/// Every process the runtime started, running or lately finished.
///
/// Shared behind an `Arc` by whoever starts processes and whoever displays
/// them. There is one per session, created by the front end and passed down:
/// nothing here is global.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
    launcher: Option<Arc<dyn super::Launcher>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("processes", &self.snapshot())
            .field("sandboxed", &self.launcher.is_some())
            .finish()
    }
}

#[derive(Debug, Default)]
struct Inner {
    next: u32,
    processes: VecDeque<Process>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry whose commands are built by `launcher`.
    #[must_use]
    pub fn with_launcher(launcher: Arc<dyn super::Launcher>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            launcher: Some(launcher),
        }
    }

    pub(crate) fn launcher(&self) -> Option<&Arc<dyn super::Launcher>> {
        self.launcher.as_ref()
    }

    /// Every process, oldest first.
    ///
    /// Owned clones, so a caller can hold the list while more processes start
    /// and finish underneath it.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Process> {
        self.lock().processes.iter().cloned().collect()
    }

    /// Ask a process to stop. Doing the stopping is [`run`](crate::run)'s job,
    /// which is watching this flag.
    pub fn kill(&self, id: u32) {
        let mut inner = self.lock();
        if let Some(process) = inner.processes.iter_mut().find(|p| p.id == id)
            && process.running()
        {
            process.kill.store(true, Ordering::Relaxed);
            process.status = Status::Killing;
        }
    }

    /// Record a process about to be started.
    pub(crate) fn start(&self, origin: &str, command: &str) -> Entry {
        let mut inner = self.lock();
        inner.next += 1;
        let entry = Entry {
            id: inner.next,
            kill: Arc::new(AtomicBool::new(false)),
        };
        inner.processes.push_back(Process {
            id: entry.id,
            pid: None,
            origin: origin.to_owned(),
            command: command.to_owned(),
            status: Status::Running,
            bytes: 0,
            started: Instant::now(),
            finished: None,
            kill: Arc::clone(&entry.kill),
        });
        entry
    }

    /// Note the pid, which is only known after the spawn.
    pub(crate) fn attach(&self, id: u32, pid: Option<u32>) {
        let mut inner = self.lock();
        if let Some(process) = inner.processes.iter_mut().find(|p| p.id == id) {
            process.pid = pid;
        }
    }

    /// Write the ending into the record, and forget the oldest endings.
    ///
    /// Returns the status, so the caller can finish and report in one step.
    pub(crate) fn finish(&self, id: u32, status: Status, bytes: u64) -> Status {
        let mut inner = self.lock();
        if let Some(process) = inner.processes.iter_mut().find(|p| p.id == id) {
            process.status = status.clone();
            process.bytes = bytes;
            process.finished = Some(Instant::now());
        }
        inner.prune();
        status
    }

    /// A poisoned registry is not worth taking the process down for: the list
    /// is a report, and losing it should not stop a command from running.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    /// Keep everything that is running, and the last [`RECENT`] that are not.
    fn prune(&mut self) {
        let mut spare = self
            .processes
            .iter()
            .filter(|process| !process.running())
            .count()
            .saturating_sub(RECENT);
        while spare > 0 {
            let Some(oldest) = self.processes.iter().position(|process| !process.running()) else {
                break;
            };
            self.processes.remove(oldest);
            spare -= 1;
        }
    }
}
