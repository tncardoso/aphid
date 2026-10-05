//! Reading the pipes of a command whose shell has ended.
//!
//! A command such as `server &` ends its shell at once, but the server inherits
//! both pipes and holds them open for as long as it lives. The run cannot wait
//! for their end, so it returns and hands the pipes to a thread of their own.
//! That thread keeps reading and throws the output away: closing the pipes
//! instead would kill the server with `SIGPIPE` the next time it writes.
//!
//! It is a plain thread rather than a task because a runtime does not always
//! run between calls: the plugin worker drives a current-thread runtime only
//! while one of its commands is running, and a task parked there would stop
//! reading until the pipe filled and the server blocked on its next write.
//!
//! The thread also owns what is left of the record. It finishes it when the
//! pipes reach their end, and stops the group when somebody asks from `/ps`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{Registry, Status};

/// A pipe taken back from its reader, in the form the thread reads.
#[cfg(unix)]
pub(crate) type Held = std::fs::File;

/// Windows has no `poll` to read with, so a pipe is not kept at all.
#[cfg(not(unix))]
pub(crate) type Held = ();

/// A pipe that can leave the runtime that read it.
pub(crate) trait Release {
    fn release(self) -> Option<Held>;
}

#[cfg(unix)]
macro_rules! release {
    ($type:ty) => {
        impl Release for $type {
            fn release(self) -> Option<Held> {
                self.into_owned_fd().ok().map(std::fs::File::from)
            }
        }
    };
}

#[cfg(not(unix))]
macro_rules! release {
    ($type:ty) => {
        impl Release for $type {
            fn release(self) -> Option<Held> {
                None
            }
        }
    };
}

release!(tokio::process::ChildStdout);
release!(tokio::process::ChildStderr);

/// What the thread needs to finish one record.
pub(crate) struct Leftover {
    pub(crate) registry: Arc<Registry>,
    pub(crate) id: u32,
    /// The group to signal: the shell's pid, which led it.
    pub(crate) group: Option<u32>,
    pub(crate) kill: Arc<AtomicBool>,
    pub(crate) bytes: Arc<AtomicU64>,
    /// The ending to record when the pipes reach their end by themselves.
    pub(crate) on_end: Status,
    /// The ending to record when the group had to be stopped.
    pub(crate) on_stop: Status,
}

/// Read `pipes` to their end on a thread of their own.
pub(crate) fn spawn(pipes: Vec<Held>, leftover: Leftover) {
    let (registry, id, bytes) = (
        Arc::clone(&leftover.registry),
        leftover.id,
        Arc::clone(&leftover.bytes),
    );
    let started = std::thread::Builder::new()
        .name("aphid-detached".to_owned())
        .spawn(move || drain(pipes, leftover));
    if started.is_err() {
        // No thread, no reader: the pipes closed when the closure was dropped,
        // so the record ends here.
        registry.finish(id, Status::Killed, bytes.load(Ordering::Relaxed));
    }
}

/// How long one `poll` waits before it looks at the kill flag again.
#[cfg(unix)]
const POLL_MS: libc::c_int = 100;

#[cfg(unix)]
fn drain(mut pipes: Vec<Held>, leftover: Leftover) {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    use super::kill::{GRACE, signal};

    let Leftover {
        registry,
        id,
        group,
        kill,
        bytes,
        on_end,
        on_stop,
    } = leftover;

    let mut buffer = [0u8; 8192];
    // Set once the group was asked to stop: by then it has `GRACE` to go.
    let mut deadline: Option<Instant> = None;

    while !pipes.is_empty() {
        if deadline.is_none() && kill.load(Ordering::Relaxed) {
            if let Some(group) = group {
                signal(group, libc::SIGTERM);
            }
            deadline = Some(Instant::now() + GRACE);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            if let Some(group) = group {
                signal(group, libc::SIGKILL);
            }
            // Whatever left the group still holds the pipes. It is not ours to
            // reach, so the pipes close under it and the record ends.
            break;
        }

        let mut polls: Vec<libc::pollfd> = pipes
            .iter()
            .map(|pipe| libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: `polls` is a live, correctly sized array of `pollfd`, and
        // every descriptor in it is owned by `pipes` for the whole call.
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, POLL_MS) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }

        // Backwards, so removing a pipe does not move the ones still to read.
        for index in (0..pipes.len()).rev() {
            if polls[index].revents == 0 {
                continue;
            }
            match pipes[index].read(&mut buffer) {
                Ok(0) => {
                    pipes.swap_remove(index);
                }
                Ok(read) => {
                    bytes.fetch_add(read as u64, Ordering::Relaxed);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    pipes.swap_remove(index);
                }
            }
        }
    }

    let ending = if deadline.is_some() { on_stop } else { on_end };
    drop(pipes);
    registry.finish(id, ending, bytes.load(Ordering::Relaxed));
}

#[cfg(not(unix))]
fn drain(_pipes: Vec<Held>, leftover: Leftover) {
    leftover.registry.finish(
        leftover.id,
        leftover.on_end,
        leftover.bytes.load(Ordering::Relaxed),
    );
}
