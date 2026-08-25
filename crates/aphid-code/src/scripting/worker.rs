//! The thread that runs blocking capabilities.
//!
//! `exec` and `http` are synchronous, and hooks run on the agent's task inside a
//! tokio runtime, where a blocking HTTP client is a hazard and `block_on` would
//! deadlock the current-thread runtime that `aphid raw` uses. So the host owns
//! one plain thread, outside any runtime, and a script's call to `exec` sends a
//! job there and blocks on the reply.
//!
//! Blocking the agent task is deliberate and already precedented: the terminal
//! UI blocks it the same way while a permission prompt waits for an answer. The
//! per-job timeout is what keeps it bounded.
//!
//! The command itself is not run here: it goes to [`aphid_agent::exec`], the
//! one place the runtime starts a process, so a plugin's command is spawned,
//! stopped and recorded exactly like a harness tool's. This thread only lends it
//! a runtime to run in, which is safe because the thread is outside every other
//! one.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_agent::exec::{self, Registry, Spec, Status, Stream};

/// What a script asked the worker to do.
pub(crate) enum Job {
    Exec {
        command: String,
        cwd: Option<PathBuf>,
        timeout: Duration,
        /// The plugin that asked, so `/ps` can say who started the command.
        origin: String,
    },
    Http {
        method: &'static str,
        url: String,
        body: Option<String>,
        headers: Vec<(String, String)>,
        timeout: Duration,
    },
}

/// What one finished job produced. `Err` is a message the script sees as a
/// runtime error, so a failed capability is recoverable rather than fatal.
pub(crate) type Answer = Result<Outcome, String>;

pub(crate) struct Outcome {
    pub status: i64,
    pub stdout: String,
    pub stderr: String,
    pub headers: Vec<(String, String)>,
}

impl Outcome {
    fn exec(status: i64, stdout: String, stderr: String) -> Self {
        Self {
            status,
            stdout,
            stderr,
            headers: Vec::new(),
        }
    }
}

/// A handle to the worker thread.
///
/// The `Sender` is behind a `Mutex` because `std::sync::mpsc::Sender` is `Send`
/// but not `Sync`, and this handle is shared across every plugin. Serialising
/// submissions costs nothing: there is one worker thread to submit to.
pub struct Worker {
    jobs: Mutex<Sender<(Job, Sender<Answer>)>>,
}

impl Worker {
    /// Start the worker thread.
    ///
    /// `processes` is the runtime's record of what it is running; every command
    /// a plugin starts is entered there.
    #[must_use]
    pub fn spawn(processes: &Arc<Registry>) -> Self {
        let (jobs, inbox) = channel::<(Job, Sender<Answer>)>();
        let processes = Arc::clone(processes);

        std::thread::Builder::new()
            .name("aphid-plugin-worker".to_owned())
            .spawn(move || serve(&inbox, &processes))
            // A host that cannot start its worker is still useful: `exec` and
            // `http` then fail per call rather than taking the process down.
            .ok();

        Self {
            jobs: Mutex::new(jobs),
        }
    }

    /// Run a job and wait for its answer.
    pub(crate) fn run(&self, job: Job) -> Answer {
        let (reply, answers) = channel();

        {
            let jobs = self
                .jobs
                .lock()
                .map_err(|_| "the plugin worker is poisoned".to_owned())?;
            jobs.send((job, reply))
                .map_err(|_| "the plugin worker has stopped".to_owned())?;
        }

        answers
            .recv()
            .map_err(|_| "the plugin worker dropped the job".to_owned())?
    }
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Worker")
    }
}

fn serve(inbox: &Receiver<(Job, Sender<Answer>)>, processes: &Arc<Registry>) {
    // Built once and reused: each blocking client owns a runtime, and one per
    // request would be wasteful.
    let client = reqwest::blocking::Client::builder().build();

    // The runtime commands run in, entered for `exec` and for nothing else: the
    // blocking HTTP client below builds its own and panics inside another.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();

    while let Ok((job, reply)) = inbox.recv() {
        let answer = match job {
            Job::Exec {
                command,
                cwd,
                timeout,
                origin,
            } => match &runtime {
                Ok(runtime) => exec(runtime, processes, &command, cwd, timeout, &origin),
                Err(error) => Err(format!("no runtime to run commands in: {error}")),
            },
            Job::Http {
                method,
                url,
                body,
                headers,
                timeout,
            } => match &client {
                Ok(client) => http(client, method, &url, body, &headers, timeout),
                Err(error) => Err(format!("no http client: {error}")),
            },
        };

        // A caller that gave up is not an error worth reporting anywhere.
        let _ = reply.send(answer);
    }
}

/// Run one command, and shape its ending the way a script expects.
///
/// A script sees a non-zero status as an ordinary result and everything else as
/// a recoverable runtime error, which is why only the two exit cases are `Ok`.
fn exec(
    runtime: &tokio::runtime::Runtime,
    processes: &Arc<Registry>,
    command: &str,
    cwd: Option<PathBuf>,
    timeout: Duration,
    origin: &str,
) -> Answer {
    let output = Arc::new(Mutex::new((String::new(), String::new())));
    let sink = {
        let output = Arc::clone(&output);
        Arc::new(move |stream, line: &str| {
            if let Ok(mut output) = output.lock() {
                let pipe = match stream {
                    Stream::Stdout => &mut output.0,
                    Stream::Stderr => &mut output.1,
                };
                pipe.push_str(line);
                pipe.push('\n');
            }
        })
    };

    let spec = Spec::new(origin, command).cwd(cwd).timeout(Some(timeout));
    let status = runtime.block_on(exec::run(processes, spec, None, sink));

    let (stdout, stderr) = output
        .lock()
        .map_or_else(|_| (String::new(), String::new()), |output| output.clone());

    match status {
        Status::Exited(code) => Ok(Outcome::exec(i64::from(code), stdout, stderr)),
        // A signal has no exit code of its own to report.
        Status::Signalled => Ok(Outcome::exec(-1, stdout, stderr)),
        Status::TimedOut => Err(format!(
            "`{command}` did not finish within {}s",
            timeout.as_secs()
        )),
        Status::Killed | Status::Killing => Err(format!("`{command}` was stopped")),
        Status::Cancelled => Err(format!("`{command}` was cancelled")),
        Status::Failed(error) => Err(error),
        Status::Running => Err(format!("`{command}` never finished")),
    }
}

fn http(
    client: &reqwest::blocking::Client,
    method: &str,
    url: &str,
    body: Option<String>,
    headers: &[(String, String)],
    timeout: Duration,
) -> Answer {
    let mut request = match method {
        "POST" => client.post(url),
        _ => client.get(url),
    }
    .timeout(timeout);

    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    if let Some(body) = body {
        request = request.body(body);
    }

    let response = request
        .send()
        .map_err(|error| format!("{method} {url} failed: {error}"))?;

    let status = i64::from(response.status().as_u16());
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let text = response
        .text()
        .map_err(|error| format!("{method} {url} gave an unreadable body: {error}"))?;

    Ok(Outcome {
        status,
        stdout: text,
        stderr: String::new(),
        headers,
    })
}
