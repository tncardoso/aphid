//! Stopping a command, and everything it started.

use std::time::Duration;

use tokio::process::Child;

/// How long a command has to finish after being asked politely.
const GRACE: Duration = Duration::from_millis(500);

/// How long the `kill` helper itself has to launch and report back.
///
/// A busy machine can make even a trivial fork+exec slow; without a bound here
/// that slowness leaks straight into how long a stop takes, unrelated to
/// whether the signal was even delivered.
const SIGNAL_TIMEOUT: Duration = Duration::from_millis(500);

/// Stop the command's whole process group: term, then kill.
///
/// Killing the child alone would only reach the shell, leaving whatever it
/// started — the compiler, the dev server — running with nobody watching. The
/// child leads its own group (see `process_group` in `lib.rs`), so its pid is
/// also the group id, and a negative pid signals the group.
///
/// The signal goes out through `kill` rather than through `libc`, which keeps
/// this crate free of a C dependency and of unsafe code. One short-lived helper
/// process per stop is a fair price.
#[cfg(unix)]
pub(crate) async fn terminate(child: &mut Child, pid: Option<u32>) {
    let Some(group) = pid else {
        // No pid means the child is already gone, or never was.
        let _ = child.kill().await;
        return;
    };

    if !signal(group, "-TERM").await {
        let _ = child.kill().await;
        return;
    }

    tokio::select! {
        _ = child.wait() => return,
        () = tokio::time::sleep(GRACE) => {}
    }

    if !signal(group, "-KILL").await {
        let _ = child.kill().await;
    }
}

/// Windows has no process groups to signal, so this is the child alone.
#[cfg(windows)]
pub(crate) async fn terminate(child: &mut Child, _pid: Option<u32>) {
    let _ = child.kill().await;
}

/// Send one signal to a whole group. `false` when the helper could not run.
#[cfg(unix)]
async fn signal(group: u32, signal: &str) -> bool {
    let status = tokio::process::Command::new("kill")
        .arg(signal)
        .arg(format!("-{group}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    matches!(
        tokio::time::timeout(SIGNAL_TIMEOUT, status).await,
        Ok(Ok(_))
    )
}
