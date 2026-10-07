//! The shared runner, against real processes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_agent::exec::{self, Registry, Spec, Status, Stream};
use aphid_agent::{AgentHandle, ToolCx};

/// A sink that keeps everything, so a test can read what a command wrote.
#[derive(Clone, Default)]
struct Collected(Arc<Mutex<Vec<(Stream, String)>>>);

impl Collected {
    fn sink(&self) -> exec::Sink {
        let lines = Arc::clone(&self.0);
        Arc::new(move |stream, line: &str| {
            lines.lock().expect("lines").push((stream, line.to_owned()));
        })
    }

    fn text(&self) -> String {
        self.0
            .lock()
            .expect("lines")
            .iter()
            .map(|(_, line)| line.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn on(&self, stream: Stream) -> String {
        self.0
            .lock()
            .expect("lines")
            .iter()
            .filter(|(which, _)| *which == stream)
            .map(|(_, line)| line.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn discard() -> exec::Sink {
    Arc::new(|_, _| {})
}

#[tokio::test]
async fn a_command_reports_its_exit_code() {
    let processes = Arc::new(Registry::new());
    let status = exec::run(&processes, Spec::new("test", "exit 3"), None, discard()).await;

    assert_eq!(status, Status::Exited(3));
}

#[tokio::test]
async fn both_pipes_reach_the_sink() {
    let processes = Arc::new(Registry::new());
    let collected = Collected::default();
    let status = exec::run(
        &processes,
        Spec::new("test", "echo out; echo err >&2"),
        None,
        collected.sink(),
    )
    .await;

    assert_eq!(status, Status::Exited(0));
    assert_eq!(collected.on(Stream::Stdout), "out");
    assert_eq!(collected.on(Stream::Stderr), "err");
}

#[tokio::test]
async fn a_lot_of_output_does_not_block_the_command() {
    // The pipe buffer is about 64 KiB. Draining only after the child exits —
    // which the plugin worker used to do — deadlocks well before this much.
    let processes = Arc::new(Registry::new());
    let collected = Collected::default();
    let status = exec::run(
        &processes,
        Spec::new(
            "test",
            "for i in $(seq 1 20000); do echo 'a line of output'; done",
        )
        .timeout(Some(Duration::from_secs(30))),
        None,
        collected.sink(),
    )
    .await;

    assert_eq!(status, Status::Exited(0));
    assert_eq!(collected.text().lines().count(), 20_000);
    let recorded = processes.snapshot();
    assert!(
        recorded[0].bytes > 64 * 1024,
        "bytes: {}",
        recorded[0].bytes
    );
}

#[tokio::test]
async fn a_timeout_stops_the_command() {
    let processes = Arc::new(Registry::new());
    let status = exec::run(
        &processes,
        Spec::new("test", "sleep 30").timeout(Some(Duration::from_millis(200))),
        None,
        discard(),
    )
    .await;

    assert_eq!(status, Status::TimedOut);
}

#[tokio::test]
async fn a_cancelled_run_stops_the_command() {
    let processes = Arc::new(Registry::new());

    // The cancellation a tool sees is the run's own, so the test cancels the
    // way the terminal UI does: through the agent's handle.
    let handle = AgentHandle::default();
    let cx = ToolCx::for_handle(&handle);
    let canceller = handle.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        canceller.cancel();
    });

    let status = exec::run(
        &processes,
        Spec::new("test", "sleep 30"),
        Some(&cx),
        discard(),
    )
    .await;

    assert_eq!(status, Status::Cancelled);
}

#[tokio::test]
async fn killing_from_the_registry_stops_the_command() {
    let processes = Arc::new(Registry::new());

    let registry = Arc::clone(&processes);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let id = registry.snapshot().first().expect("one process").id;
        registry.kill(id);
    });

    let status = exec::run(&processes, Spec::new("test", "sleep 30"), None, discard()).await;

    assert_eq!(status, Status::Killed);
}

#[tokio::test]
async fn killing_takes_the_whole_group() {
    let processes = Arc::new(Registry::new());
    let collected = Collected::default();

    let registry = Arc::clone(&processes);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let id = registry.snapshot().first().expect("one process").id;
        registry.kill(id);
    });

    // The shell reports the grandchild's pid, then waits on it. Killing the
    // shell alone would leave the sleep running.
    let status = exec::run(
        &processes,
        Spec::new("test", "sleep 30 & echo $!; wait"),
        None,
        collected.sink(),
    )
    .await;

    assert_eq!(status, Status::Killed);

    let grandchild: u32 = collected.text().trim().parse().expect("a pid");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let alive = tokio::process::Command::new("kill")
        .arg("-0")
        .arg(grandchild.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("kill -0")
        .success();
    assert!(!alive, "the grandchild {grandchild} outlived its group");
}

#[tokio::test]
async fn a_command_that_cannot_start_is_recorded_as_failed() {
    let processes = Arc::new(Registry::new());
    let status = exec::run(
        &processes,
        Spec::new("test", "true").cwd(Some("/no/such/directory".into())),
        None,
        discard(),
    )
    .await;

    assert!(matches!(status, Status::Failed(_)), "{status:?}");
    assert_eq!(processes.snapshot().len(), 1);
}

#[tokio::test]
async fn the_registry_follows_one_process_from_running_to_finished() {
    let processes = Arc::new(Registry::new());

    let registry = Arc::clone(&processes);
    let seen = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        registry.snapshot()
    });

    let status = exec::run(
        &processes,
        Spec::new("webchat", "sleep 0.4"),
        None,
        discard(),
    )
    .await;
    assert_eq!(status, Status::Exited(0));

    let while_running = seen.await.expect("the watcher");
    assert_eq!(while_running.len(), 1);
    assert_eq!(while_running[0].origin, "webchat");
    assert_eq!(while_running[0].command, "sleep 0.4");
    assert_eq!(while_running[0].status, Status::Running);
    assert!(while_running[0].pid.is_some());
    assert!(while_running[0].running());

    let after = processes.snapshot();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, while_running[0].id);
    assert_eq!(after[0].pid, while_running[0].pid);
    assert_eq!(after[0].status, Status::Exited(0));
    assert!(!after[0].running());
    assert!(after[0].elapsed() >= Duration::from_millis(400));
}

#[tokio::test]
async fn only_the_last_four_endings_are_kept() {
    let processes = Arc::new(Registry::new());

    for index in 0..7 {
        exec::run(
            &processes,
            Spec::new("test", format!("exit {index}")),
            None,
            discard(),
        )
        .await;
    }

    let kept = processes.snapshot();
    assert_eq!(kept.len(), exec::RECENT);
    let commands: Vec<&str> = kept.iter().map(|p| p.command.as_str()).collect();
    assert_eq!(commands, ["exit 3", "exit 4", "exit 5", "exit 6"]);
    // Ids are never reused, so the oldest kept is the fourth started.
    assert_eq!(kept[0].id, 4);
}

#[tokio::test]
async fn a_running_process_is_never_pruned() {
    let processes = Arc::new(Registry::new());

    let registry = Arc::clone(&processes);
    let slow = tokio::spawn(async move {
        exec::run(&registry, Spec::new("test", "sleep 1"), None, discard()).await
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    for index in 0..6 {
        exec::run(
            &processes,
            Spec::new("test", format!("exit {index}")),
            None,
            discard(),
        )
        .await;
    }

    let kept = processes.snapshot();
    assert_eq!(kept.len(), exec::RECENT + 1);
    assert_eq!(kept[0].command, "sleep 1");
    assert!(kept[0].running());

    slow.await.expect("the slow one");
}

#[tokio::test]
async fn a_backgrounded_command_that_redirects_its_output_does_not_hold_the_run() {
    // What a plugin does to start a server: the pipes are the child's only tie
    // to the runner, so it has to hand them off before it detaches.
    let directory = std::env::temp_dir().join(format!("aphid-run-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("scratch directory");

    let processes = Arc::new(Registry::new());
    let started = std::time::Instant::now();
    let status = exec::run(
        &processes,
        Spec::new("plugin", "nohup sleep 20 > log 2>&1 & echo $! > pid")
            .cwd(Some(directory.clone()))
            .timeout(Some(Duration::from_secs(10))),
        None,
        discard(),
    )
    .await;

    assert_eq!(status, Status::Exited(0));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the detached child should not hold the pipes open"
    );

    let pid = std::fs::read_to_string(directory.join("pid")).expect("the pid file");
    let _ = tokio::process::Command::new("kill")
        .arg(pid.trim())
        .status()
        .await;
    let _ = std::fs::remove_dir_all(&directory);
}

/// Whether a pid still names a live process.
async fn alive(pid: u32) -> bool {
    tokio::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("kill -0")
        .success()
}

async fn stop(pid: u32) {
    let _ = tokio::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .await;
}

#[tokio::test]
async fn a_backgrounded_command_that_keeps_the_pipes_does_not_hold_the_run() {
    // The background sleep inherits both pipes, so they never reach their end
    // while it lives. Waiting for that end froze the agent.
    let processes = Arc::new(Registry::new());
    let collected = Collected::default();
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        exec::run(
            &processes,
            Spec::new("test", "sleep 20 & echo $!; echo done"),
            None,
            collected.sink(),
        ),
    )
    .await
    .expect("the run should not wait for the background process");

    assert_eq!(status, Status::Exited(0));
    let pid: u32 = collected
        .on(Stream::Stdout)
        .lines()
        .find_map(|line| line.parse().ok())
        .expect("a pid");
    assert!(collected.on(Stream::Stdout).contains("done"));
    assert!(collected.on(Stream::Stderr).contains("[aphid]"));
    stop(pid).await;
}

/// The status of the only process, once `done` accepts it, or after 5 s.
async fn settles(processes: &Registry, done: impl Fn(&Status) -> bool) -> Status {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = processes.snapshot()[0].status.clone();
        if done(&status) || std::time::Instant::now() > deadline {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_background_process_keeps_running_and_writing_after_the_run() {
    // Closing the pipes would kill it with SIGPIPE on its next tick. The
    // first tick can come before the pid, because the child can run first.
    let processes = Arc::new(Registry::new());
    let collected = Collected::default();
    let status = exec::run(
        &processes,
        Spec::new(
            "test",
            "(for i in 1 2 3 4 5 6 7 8; do echo tick; sleep 0.1; done) & echo $!",
        ),
        None,
        collected.sink(),
    )
    .await;

    assert_eq!(status, Status::Exited(0));
    assert_eq!(processes.snapshot()[0].status, Status::Detached);
    let pid: u32 = collected
        .on(Stream::Stdout)
        .lines()
        .find_map(|line| line.parse().ok())
        .expect("a pid");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(alive(pid).await, "the background process died with the run");

    // It ends by itself, and the record ends with the shell's own status.
    assert_eq!(
        settles(&processes, |status| !status.running()).await,
        Status::Exited(0)
    );
    assert!(processes.snapshot()[0].bytes > 20);
}

#[tokio::test]
async fn killing_a_detached_process_stops_its_group() {
    let processes = Arc::new(Registry::new());
    let collected = Collected::default();
    let status = exec::run(
        &processes,
        Spec::new("test", "sleep 30 & echo $!"),
        None,
        collected.sink(),
    )
    .await;
    assert_eq!(status, Status::Exited(0));
    let pid: u32 = collected
        .on(Stream::Stdout)
        .lines()
        .find_map(|line| line.parse().ok())
        .expect("a pid");

    let id = processes.snapshot()[0].id;
    processes.kill(id);

    assert_eq!(
        settles(&processes, |status| !status.running()).await,
        Status::Killed
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    if alive(pid).await {
        stop(pid).await;
        panic!("the background process {pid} outlived the kill");
    }
}

#[tokio::test]
async fn cancelling_while_the_pipes_drain_stops_the_run() {
    let processes = Arc::new(Registry::new());
    let collected = Collected::default();

    let handle = AgentHandle::default();
    let cx = ToolCx::for_handle(&handle);
    let canceller = handle.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        canceller.cancel();
    });

    // The shell ends at once; the cancel lands while the run still waits for
    // the pipes the sleep holds.
    let status = exec::run(
        &processes,
        Spec::new("test", "sleep 30 & echo $!"),
        Some(&cx),
        collected.sink(),
    )
    .await;
    assert_eq!(status, Status::Cancelled);

    assert_eq!(
        settles(&processes, |status| !status.running()).await,
        Status::Cancelled
    );
    let pid: u32 = collected
        .on(Stream::Stdout)
        .lines()
        .find_map(|line| line.parse().ok())
        .expect("a pid");
    tokio::time::sleep(Duration::from_millis(100)).await;
    if alive(pid).await {
        stop(pid).await;
        panic!("the background process {pid} outlived the cancel");
    }
}

#[test]
fn a_detached_process_is_read_while_the_runtime_is_idle() {
    // The plugin worker drives a current-thread runtime only while a command
    // runs. A reader parked in that runtime would leave a background process
    // blocked on a full pipe; this one writes far more than a pipe holds.
    let directory = std::env::temp_dir().join(format!("aphid-idle-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("scratch directory");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let processes = Arc::new(Registry::new());
    let status = runtime.block_on(exec::run(
        &processes,
        Spec::new(
            "plugin",
            "(sleep 0.3; head -c 1000000 /dev/zero | tr '\\0' x; echo; touch finished) &",
        )
        .cwd(Some(directory.clone())),
        None,
        discard(),
    ));
    assert_eq!(status, Status::Exited(0));

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !directory.join("finished").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let finished = directory.join("finished").exists();
    let _ = std::fs::remove_dir_all(&directory);
    assert!(finished, "the background process blocked on a full pipe");
}
