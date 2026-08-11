//! The one place the aphid runtime starts a process.
//!
//! Both callers — a harness tool such as `bash` and a plugin's `exec` — come
//! through [`run`], so a command started from a script and a command started by
//! the model are spawned, timed, stopped and recorded by the same code. What
//! they still choose for themselves is what to do with the output: [`run`]
//! hands each line to a [`Sink`], and the caller decides whether that means
//! streaming it to a terminal or collecting it into a string.
//!
//! Every process is entered in a [`Registry`] while it runs and left there,
//! with its ending, for a while after. That record is what a `/ps` command
//! shows, and what makes a running command something a user can stop.
//!
//! This lives beside the agent loop rather than inside the tool that uses it
//! because two crates need the same runner, and the loop is the deepest thing
//! they share.
//!
//! ```no_run
//! # async fn example() {
//! use std::sync::Arc;
//! use aphid_agent::exec::{self, Registry, Spec, Status};
//!
//! let processes = Arc::new(Registry::new());
//! let status = exec::run(
//!     &processes,
//!     Spec::new("bash", "echo hello"),
//!     None,
//!     Arc::new(|_stream, line: &str| println!("{line}")),
//! )
//! .await;
//! assert_eq!(status, Status::Exited(0));
//! # }
//! ```

mod kill;
mod registry;

pub use registry::{Process, RECENT, Registry, Status};

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::ToolCx;

/// How often a running command notices it was asked to stop.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// The shell every command runs in.
///
/// One engine means one shell. Both callers used to pick their own — the tool
/// bash, a plugin `sh` — which made a command that worked in one fail in the
/// other for no reason a user could see.
const SHELL: &str = "bash";

/// What to run.
pub struct Spec {
    pub command: String,
    /// Where to run it. `None` inherits the runtime's directory.
    pub cwd: Option<PathBuf>,
    /// How long to allow. `None` waits for as long as it takes.
    pub timeout: Option<Duration>,
    /// Who asked: `bash`, or the name of the plugin.
    pub origin: String,
}

impl Spec {
    /// A command with no directory and no timeout.
    #[must_use]
    pub fn new(origin: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            timeout: None,
            origin: origin.into(),
        }
    }

    #[must_use]
    pub fn cwd(mut self, cwd: Option<PathBuf>) -> Self {
        self.cwd = cwd;
        self
    }

    #[must_use]
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Which pipe a line arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Where output goes, one line at a time, as it arrives.
///
/// Both pipes are read at once and each line is published the moment it lands,
/// which is what lets a caller stream progress — and what stops a command that
/// writes more than a pipe buffer from blocking for ever on a full pipe.
pub type Sink = Arc<dyn Fn(Stream, &str) + Send + Sync>;

/// Run a command to its end, and record it while it runs.
///
/// `cx` is the tool call this command belongs to, when it belongs to one. Its
/// cancellation is watched alongside the registry's, so a command stops either
/// when its run is cancelled or when a user stops it from the process list. A
/// plugin's command belongs to no call and passes `None`.
pub async fn run(registry: &Arc<Registry>, spec: Spec, cx: Option<&ToolCx>, sink: Sink) -> Status {
    let entry = registry.start(&spec.origin, &spec.command);

    let mut builder = Command::new(SHELL);
    builder
        .arg("-c")
        .arg(&spec.command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // So an abandoned future does not leave a process behind.
        .kill_on_drop(true);
    if let Some(cwd) = &spec.cwd {
        builder.current_dir(cwd);
    }
    // Its own group, so stopping it can reach whatever it starts.
    #[cfg(unix)]
    builder.process_group(0);

    let mut child = match builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            let failed = Status::Failed(format!("could not run `{}`: {error}", spec.command));
            return registry.finish(entry.id, failed, 0);
        }
    };

    let pid = child.id();
    registry.attach(entry.id, pid);

    let bytes = Arc::new(AtomicU64::new(0));
    let mut pumps = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        pumps.push(tokio::spawn(pump(
            stdout,
            Stream::Stdout,
            Arc::clone(&sink),
            Arc::clone(&bytes),
        )));
    }
    if let Some(stderr) = child.stderr.take() {
        pumps.push(tokio::spawn(pump(
            stderr,
            Stream::Stderr,
            Arc::clone(&sink),
            Arc::clone(&bytes),
        )));
    }

    let status = wait(&mut child, spec.timeout, cx, &entry.kill).await;
    if !ended_on_its_own(&status) {
        kill::terminate(&mut child, pid).await;
    }

    // After the child is gone, so the last of its output is in.
    for pump in pumps {
        let _ = pump.await;
    }

    registry.finish(entry.id, status, bytes.load(Ordering::Relaxed))
}

/// Whether the command reached its own end rather than being stopped.
fn ended_on_its_own(status: &Status) -> bool {
    matches!(
        status,
        Status::Exited(_) | Status::Signalled | Status::Failed(_)
    )
}

async fn wait(
    child: &mut tokio::process::Child,
    timeout: Option<Duration>,
    cx: Option<&ToolCx>,
    kill: &Arc<AtomicBool>,
) -> Status {
    match timeout {
        Some(limit) => tokio::select! {
            status = child.wait() => exited(status),
            stopped = stopped(cx, kill) => stopped,
            () = tokio::time::sleep(limit) => Status::TimedOut,
        },
        None => tokio::select! {
            status = child.wait() => exited(status),
            stopped = stopped(cx, kill) => stopped,
        },
    }
}

fn exited(status: std::io::Result<std::process::ExitStatus>) -> Status {
    match status {
        Ok(status) => match status.code() {
            Some(code) => Status::Exited(code),
            None => Status::Signalled,
        },
        Err(error) => Status::Failed(format!("could not wait for the command: {error}")),
    }
}

/// Resolves when somebody wants the command stopped, saying who.
///
/// Both are flags rather than futures — a run's cancellation is an
/// `AtomicBool` the agent shares with its tools — so they have to be polled.
async fn stopped(cx: Option<&ToolCx>, kill: &Arc<AtomicBool>) -> Status {
    loop {
        if kill.load(Ordering::Relaxed) {
            return Status::Killed;
        }
        if cx.is_some_and(ToolCx::cancelled) {
            return Status::Cancelled;
        }
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

/// Forward one pipe to the sink, counting what went through it.
async fn pump<R>(reader: R, stream: Stream, sink: Sink, bytes: Arc<AtomicU64>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        // The newline the reader took off still counts as output.
        bytes.fetch_add(line.len() as u64 + 1, Ordering::Relaxed);
        sink(stream, &line);
    }
}
