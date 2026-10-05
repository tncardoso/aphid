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

mod detach;
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
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use detach::{Held, Leftover, Release};

use crate::ToolCx;

/// How often a running command notices it was asked to stop.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// How long the pipes may stay open once the shell has ended.
///
/// What the shell wrote is already in the pipe and takes microseconds to read;
/// this is margin. A pipe still open after it is held by something the command
/// started in the background, and its end may never come.
const DRAIN: Duration = Duration::from_millis(250);

/// The line a caller sees when the run stops reading before the pipes end.
const DETACHED: &str = "[aphid] A background process still holds the output of \
    this command, so aphid stopped reading it. To keep that output, send it to a \
    file, for example `cmd > log 2>&1 &`. Type /ps to see or stop the process.";

/// The shell every command runs in.
///
/// One engine means one shell. Both callers used to pick their own — the tool
/// bash, a plugin `sh` — which made a command that worked in one fail in the
/// other for no reason a user could see.
const SHELL: &str = "bash";

/// Builds the child process for one command.
///
/// The ordinary launcher starts `bash -c`. A front end may install a launcher
/// that wraps that shell in a sandbox without making tools choose separate
/// execution paths.
pub trait Launcher: Send + Sync + 'static {
    fn command(&self, spec: &Spec) -> Command;
}

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
///
/// Lines arrive until the shell ends, and for [`DRAIN`] after. Output from a
/// process the command left in the background is read past that, but it does
/// not reach the sink.
pub type Sink = Arc<dyn Fn(Stream, &str) + Send + Sync>;

/// Run a command to its end, and record it while it runs.
///
/// `cx` is the tool call this command belongs to, when it belongs to one. Its
/// cancellation is watched alongside the registry's, so a command stops either
/// when its run is cancelled or when a user stops it from the process list. A
/// plugin's command belongs to no call and passes `None`.
pub async fn run(registry: &Arc<Registry>, spec: Spec, cx: Option<&ToolCx>, sink: Sink) -> Status {
    let entry = registry.start(&spec.origin, &spec.command);

    let mut builder = registry.launcher().map_or_else(
        || {
            let mut builder = Command::new(SHELL);
            builder.arg("-c").arg(&spec.command);
            builder
        },
        |launcher| launcher.command(&spec),
    );
    builder
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // So an abandoned future does not leave a process behind.
        .kill_on_drop(true);
    if registry.launcher().is_none()
        && let Some(cwd) = &spec.cwd
    {
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
    let mut pumps = JoinSet::new();
    let mut releases = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        let (release, released) = oneshot::channel();
        releases.push(release);
        pumps.spawn(pump(
            stdout,
            Stream::Stdout,
            Arc::clone(&sink),
            Arc::clone(&bytes),
            released,
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        let (release, released) = oneshot::channel();
        releases.push(release);
        pumps.spawn(pump(
            stderr,
            Stream::Stderr,
            Arc::clone(&sink),
            Arc::clone(&bytes),
            released,
        ));
    }

    let status = wait(&mut child, spec.timeout, cx, &entry.kill).await;
    let on_its_own = ended_on_its_own(&status);
    if !on_its_own {
        kill::terminate(&mut child, pid).await;
    }

    // After the child is gone, so the last of its output is in. A stop asked
    // for now is still a stop: the shell has ended, but what it left has not.
    let watch = on_its_own.then_some((cx, &entry.kill));
    let (status, leftover) = match settle(&mut pumps, watch).await {
        Settled::Ended => {
            return registry.finish(entry.id, status, bytes.load(Ordering::Relaxed));
        }
        Settled::Lingering if on_its_own => {
            sink(Stream::Stderr, DETACHED);
            registry.detach(entry.id, bytes.load(Ordering::Relaxed));
            (status.clone(), (status, Status::Killed))
        }
        // Stopped already, and something escaped the stop with the pipes.
        Settled::Lingering => {
            registry.kill(entry.id);
            (status.clone(), (status.clone(), status))
        }
        Settled::Stopped(stopped) => {
            registry.kill(entry.id);
            (stopped.clone(), (stopped.clone(), stopped))
        }
    };

    for release in releases {
        let _ = release.send(());
    }
    let mut pipes = Vec::with_capacity(2);
    while let Some(pipe) = pumps.join_next().await {
        if let Ok(Some(pipe)) = pipe {
            pipes.push(pipe);
        }
    }

    let (on_end, on_stop) = leftover;
    detach::spawn(
        pipes,
        Leftover {
            registry: Arc::clone(registry),
            id: entry.id,
            group: pid,
            kill: Arc::clone(&entry.kill),
            bytes,
            on_end,
            on_stop,
        },
    );
    status
}

/// How the pipes went once the shell had ended.
enum Settled {
    /// Both reached their end.
    Ended,
    /// Somebody asked for a stop while they were still open.
    Stopped(Status),
    /// Still open after [`DRAIN`].
    Lingering,
}

/// Wait for both pipes to end, for at most [`DRAIN`].
///
/// `watch` is the run's cancellation and kill flag, for a run that was not
/// already stopped; a stopped run has nothing more to listen for.
async fn settle(
    pumps: &mut JoinSet<Option<Held>>,
    watch: Option<(Option<&ToolCx>, &Arc<AtomicBool>)>,
) -> Settled {
    let ended = async { while pumps.join_next().await.is_some() {} };
    let asked = async {
        match watch {
            Some((cx, kill)) => stopped(cx, kill).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        () = ended => Settled::Ended,
        status = asked => Settled::Stopped(status),
        () = tokio::time::sleep(DRAIN) => Settled::Lingering,
    }
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
///
/// Gives the pipe back when `released` fires before the pipe ends, so it can be
/// read past the end of the run.
async fn pump<R>(
    reader: R,
    stream: Stream,
    sink: Sink,
    bytes: Arc<AtomicU64>,
    mut released: oneshot::Receiver<()>,
) -> Option<Held>
where
    R: tokio::io::AsyncRead + Release + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    // The newline the reader took off still counts as output.
                    bytes.fetch_add(line.len() as u64 + 1, Ordering::Relaxed);
                    sink(stream, &line);
                }
                _ => return None,
            },
            _ = &mut released => return lines.into_inner().into_inner().release(),
        }
    }
}
