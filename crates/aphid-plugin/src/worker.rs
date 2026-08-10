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

use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// How often a running command is checked for having finished.
const POLL: Duration = Duration::from_millis(20);

/// What a script asked the worker to do.
pub(crate) enum Job {
    Exec {
        command: String,
        cwd: Option<PathBuf>,
        timeout: Duration,
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
    #[must_use]
    pub fn spawn() -> Self {
        let (jobs, inbox) = channel::<(Job, Sender<Answer>)>();

        std::thread::Builder::new()
            .name("aphid-plugin-worker".to_owned())
            .spawn(move || serve(&inbox))
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

fn serve(inbox: &Receiver<(Job, Sender<Answer>)>) {
    // Built once and reused: each blocking client owns a runtime, and one per
    // request would be wasteful.
    let client = reqwest::blocking::Client::builder().build();

    while let Ok((job, reply)) = inbox.recv() {
        let answer = match job {
            Job::Exec {
                command,
                cwd,
                timeout,
            } => exec(&command, cwd.as_deref(), timeout),
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

fn exec(command: &str, cwd: Option<&std::path::Path>, timeout: Duration) -> Answer {
    use std::process::{Command, Stdio};

    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let flag = if cfg!(windows) { "/C" } else { "-c" };

    let mut builder = Command::new(shell);
    builder
        .arg(flag)
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        builder.current_dir(cwd);
    }

    let mut child = builder
        .spawn()
        .map_err(|error| format!("could not run `{command}`: {error}"))?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "`{command}` did not finish within {}s",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(POLL);
            }
            Err(error) => return Err(format!("could not wait for `{command}`: {error}")),
        }
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }

    Ok(Outcome::exec(
        i64::from(status.code().unwrap_or(-1)),
        stdout,
        stderr,
    ))
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
