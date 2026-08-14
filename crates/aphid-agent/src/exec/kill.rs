//! Stopping a command, and everything it started.

use std::time::Duration;

use tokio::process::Child;

/// How long a command has to finish after being asked politely.
const GRACE: Duration = Duration::from_millis(500);

/// Stop the command's whole process group: term, then kill.
///
/// Killing the child alone would only reach the shell, leaving whatever it
/// started — the compiler, the dev server — running with nobody watching. The
/// child leads its own group (see `process_group` in `lib.rs`), so its pid is
/// also the group id, and a negative pid signals the group.
///
/// The signal goes out through `libc::kill` directly rather than by spawning
/// the `kill` binary. A helper process adds a fork+exec to every stop, and on
/// a loaded CI runner that fork can queue behind everything else for longer
/// than the grace period, so the group never actually got the signal in time
/// — the direct syscall has nothing left to queue behind.
#[cfg(unix)]
pub(crate) async fn terminate(child: &mut Child, pid: Option<u32>) {
    let Some(group) = pid else {
        // No pid means the child is already gone, or never was.
        let _ = child.kill().await;
        return;
    };

    if !signal(group, libc::SIGTERM) {
        let _ = child.kill().await;
        return;
    }

    tokio::select! {
        _ = child.wait() => return,
        () = tokio::time::sleep(GRACE) => {}
    }

    if !signal(group, libc::SIGKILL) {
        let _ = child.kill().await;
    }
}

/// Windows has no process groups to signal, so this is the child alone.
#[cfg(windows)]
pub(crate) async fn terminate(child: &mut Child, _pid: Option<u32>) {
    let _ = child.kill().await;
}

/// Send one signal to a whole group. `false` on an error that a fallback
/// `child.kill()` might still recover from; a group that is already gone
/// counts as delivered, not as failure.
#[cfg(unix)]
fn signal(group: u32, signal: libc::c_int) -> bool {
    // SAFETY: `kill(2)` only reads its arguments and reports through errno; a
    // negative pid addresses the whole process group rather than one process.
    let result = unsafe { libc::kill(-(group as libc::pid_t), signal) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}
