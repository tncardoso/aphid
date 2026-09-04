//! The daemon's side of the socket.
//!
//! One listener, any number of connections, and any number of conversations
//! behind them. A connection watches one session at a time and is shown that
//! session's frames plus the daemon's own; [`Request::Watch`] moves it, and the
//! daemon replays whatever it moved to. A connection is therefore not a
//! session, and a session is not a connection: a job's conversation runs with
//! nobody watching, and a terminal can look at one that finished last week.
//!
//! There is no backlog kept in memory. History is the transcript, and the
//! daemon replays it on request — which is the same answer for a session that
//! is running and one that ended yesterday, and cannot go stale.
//!
//! Everything published is also appended to `alate.log`, so the hours when
//! nobody was attached are still readable afterwards.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_code::plugins::permissions::{Confirmer, Decision, Risk};
use aphid_code::tui::runtime::Answers;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

use super::wire::{Envelope, Frame, Request};

/// How many envelopes the fan-out holds for a connection that is behind.
const CHANNEL: usize = 1024;

/// How long a tool waits for somebody to answer it.
///
/// Long, because the answer is a person's, and a person may be making tea.
/// Finite, because a run that blocks for ever holds its session open and its
/// tokens spent.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

/// The longest a socket path may be.
///
/// A Unix socket path goes into a fixed-size field — 108 bytes on Linux, 104 on
/// macOS — and this is under both with room for the file name. It is checked
/// here because the error the kernel gives back names neither the limit nor the
/// path, and a long `$APHID_HOME` is otherwise reported as a mystery.
const MAX_SOCKET_PATH: usize = 90;
/// Keeps attachment permission ids distinct from ordinary gateway questions.
const ATTACHMENT_CONFIRM_BIT: u64 = 1 << 63;

/// What the daemon hears from the socket.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A terminal arrived. The daemon opens a session for it and greets it.
    Opened { connection: u64 },
    /// A terminal left. Whatever it owned goes with it.
    Closed { connection: u64 },
    /// A terminal asked for something, while watching `session`.
    Asked {
        connection: u64,
        session: Option<String>,
        request: Request,
    },
}

/// The listening gateway.
pub struct Server {
    socket: PathBuf,
    shared: Arc<Shared>,
}

/// Sends one file to the gateway connection that owns a session.
#[derive(Clone)]
pub struct AttachmentSender {
    shared: Arc<Shared>,
    connection: u64,
}

/// One attached client.
struct Connection {
    /// What it is watching. Frames for anything else are not sent to it.
    current: Mutex<Option<String>>,
    /// What it said it was when it attached. `None` for a terminal.
    channel: Mutex<Option<String>>,
    attachments: Mutex<bool>,
    /// Envelopes meant for this one alone: its greeting, a session list, a
    /// replay it asked for.
    direct: mpsc::UnboundedSender<Envelope>,
}

/// What the accept loop, the confirmer and the daemon all reach.
struct Shared {
    connections: Mutex<HashMap<u64, Arc<Connection>>>,
    next_connection: AtomicU64,
    /// Tools waiting on an answer, by the id their frame carried.
    answers: Answers<Decision>,
    attachment_confirms: Answers<Decision>,
    attachment_confirm_connections: Mutex<HashMap<u64, (u64, u64)>>,
    attachment_answers: Answers<Result<(), String>>,
    attachment_answer_connections: Mutex<HashMap<u64, u64>>,
    log: Mutex<Option<File>>,
    envelopes: broadcast::Sender<Envelope>,
}

impl Server {
    /// Bind the socket and start accepting.
    ///
    /// The returned receiver carries what the terminals do; the daemon drains
    /// it. A socket file left by a daemon that was killed is removed first — it
    /// cannot be connected to, so nothing is lost with it.
    ///
    /// # Errors
    ///
    /// Fails when the path is too long to be a socket, or when the socket
    /// cannot be bound — usually because another daemon already serves this
    /// instance, which arrives as [`ErrorKind::AddrInUse`].
    ///
    /// [`ErrorKind::AddrInUse`]: std::io::ErrorKind::AddrInUse
    pub fn bind(
        socket: &Path,
        log: Option<&Path>,
    ) -> std::io::Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let length = socket.as_os_str().len();
        if length > MAX_SOCKET_PATH {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "{} is {length} characters, and a socket path cannot be longer than \
                     {MAX_SOCKET_PATH} — point `gateway.socket` at a shorter path",
                    socket.display()
                ),
            ));
        }

        if stale(socket) {
            let _ = std::fs::remove_file(socket);
        }

        let listener = UnixListener::bind(socket)?;
        restrict(socket)?;

        let (envelopes, _) = broadcast::channel(CHANNEL);
        let (events, incoming) = mpsc::unbounded_channel();

        let shared = Arc::new(Shared {
            connections: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(1),
            answers: Answers::default(),
            attachment_confirms: Answers::default(),
            attachment_confirm_connections: Mutex::new(HashMap::new()),
            attachment_answers: Answers::default(),
            attachment_answer_connections: Mutex::new(HashMap::new()),
            log: Mutex::new(log.and_then(open_log)),
            envelopes,
        });

        tokio::spawn(accept(listener, shared.clone(), events));

        tracing::info!(socket = %socket.display(), "gateway socket bound");

        Ok((
            Self {
                socket: socket.to_path_buf(),
                shared,
            },
            incoming,
        ))
    }

    /// A handle for the plugin and the sink, which publish from hooks.
    #[must_use]
    pub fn publisher(&self) -> Publisher {
        Publisher {
            shared: self.shared.clone(),
            session: Mutex::new(None),
        }
    }

    /// The confirmer that asks whoever is attached.
    #[must_use]
    pub fn confirmer(&self) -> Arc<dyn Confirmer> {
        Arc::new(GatewayConfirmer {
            shared: self.shared.clone(),
        })
    }

    /// Send one envelope to every connection watching it, and to the log.
    pub fn send(&self, envelope: Envelope) {
        self.shared.publish(envelope);
    }

    /// Send one envelope to one connection.
    ///
    /// For everything whose audience is a single terminal: its greeting, a
    /// session list it asked for, a replay it asked for.
    pub fn reply(&self, connection: u64, envelope: Envelope) {
        let held = self.shared.connections.lock().ok().and_then(|connections| {
            connections
                .get(&connection)
                .map(|connection| connection.direct.clone())
        });
        if let Some(direct) = held {
            let _ = direct.send(envelope);
        }
    }

    /// Point a connection at a session. From here on it sees that one's frames.
    pub fn watch(&self, connection: u64, session: &str) {
        if let Ok(connections) = self.shared.connections.lock()
            && let Some(held) = connections.get(&connection)
            && let Ok(mut current) = held.current.lock()
        {
            *current = Some(session.to_owned());
        }
    }

    /// Whether any client is attached.
    #[must_use]
    pub fn attached(&self) -> bool {
        self.shared.attached()
    }

    /// What a connection said it was when it attached.
    ///
    /// `None` for a terminal, which says nothing. The daemon reads it to name
    /// the session it opens, so that a listing says where a conversation is
    /// being had and not only that somebody is having it.
    #[must_use]
    pub fn channel(&self, connection: u64) -> Option<String> {
        let connections = self.shared.connections.lock().ok()?;
        let held = connections.get(&connection)?;
        let channel = held.channel.lock().ok()?;
        channel.clone()
    }

    /// A sender for an attachment-capable connection.
    #[must_use]
    pub fn attachment_sender(&self, connection: u64) -> Option<AttachmentSender> {
        let connections = self.shared.connections.lock().ok()?;
        let held = connections.get(&connection)?;
        if !held
            .attachments
            .lock()
            .ok()
            .map(|value| *value)
            .unwrap_or(false)
        {
            return None;
        }
        Some(AttachmentSender {
            shared: self.shared.clone(),
            connection,
        })
    }
}

impl AttachmentSender {
    /// Ask only this gateway connection for permission.
    pub fn confirm(&self, tool: &str, summary: &str, risk: Risk) -> Decision {
        let (raw_id, answer) = self.shared.attachment_confirms.open();
        let id = raw_id | ATTACHMENT_CONFIRM_BIT;
        if let Ok(mut waiting) = self.shared.attachment_confirm_connections.lock() {
            waiting.insert(id, (self.connection, raw_id));
        }
        let direct = self.shared.connections.lock().ok().and_then(|connections| {
            connections
                .get(&self.connection)
                .map(|connection| connection.direct.clone())
        });
        let Some(direct) = direct else {
            self.shared.attachment_confirms.abandon(id);
            if let Ok(mut waiting) = self.shared.attachment_confirm_connections.lock() {
                waiting.remove(&id);
            }
            return Decision::Deny;
        };
        if direct
            .send(Envelope::daemon(Frame::Confirm {
                id,
                tool: tool.to_owned(),
                summary: summary.to_owned(),
                risk: risk.into(),
            }))
            .is_err()
        {
            self.shared.attachment_confirms.abandon(id);
            if let Ok(mut waiting) = self.shared.attachment_confirm_connections.lock() {
                waiting.remove(&id);
            }
            return Decision::Deny;
        }
        let decision = answer
            .recv_timeout(ANSWER_TIMEOUT)
            .unwrap_or(Decision::Deny);
        if let Ok(mut waiting) = self.shared.attachment_confirm_connections.lock() {
            waiting.remove(&id);
        }
        decision
    }

    /// Deliver a file and wait for the gateway's acknowledgement.
    pub fn send(
        &self,
        session: &str,
        name: String,
        data: String,
        caption: Option<String>,
    ) -> Result<(), String> {
        let (id, answer) = self.shared.attachment_answers.open();
        if let Ok(mut waiting) = self.shared.attachment_answer_connections.lock() {
            waiting.insert(id, self.connection);
        }
        let direct = self.shared.connections.lock().ok().and_then(|connections| {
            connections
                .get(&self.connection)
                .map(|connection| connection.direct.clone())
        });
        let Some(direct) = direct else {
            self.shared.attachment_answers.abandon(id);
            if let Ok(mut waiting) = self.shared.attachment_answer_connections.lock() {
                waiting.remove(&id);
            }
            return Err("the gateway disconnected before the attachment could be sent".to_owned());
        };
        if direct
            .send(Envelope::from(
                session,
                Frame::Attachment {
                    id,
                    name,
                    data,
                    caption,
                },
            ))
            .is_err()
        {
            self.shared.attachment_answers.abandon(id);
            if let Ok(mut waiting) = self.shared.attachment_answer_connections.lock() {
                waiting.remove(&id);
            }
            return Err("the gateway disconnected before the attachment could be sent".to_owned());
        }
        let result = answer
            .recv_timeout(ANSWER_TIMEOUT)
            .unwrap_or_else(|_| Err("the gateway did not confirm the attachment".to_owned()));
        if let Ok(mut waiting) = self.shared.attachment_answer_connections.lock() {
            waiting.remove(&id);
        }
        result
    }
}

impl Confirmer for AttachmentSender {
    fn confirm(&self, tool: &str, summary: &str, risk: Risk) -> Decision {
        self.confirm(tool, summary, risk)
    }
}

impl Drop for Server {
    /// Take the socket file with us.
    ///
    /// A socket left behind is one that `attach` tries, fails on, and reports
    /// as though a daemon were misbehaving rather than absent.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Shared {
    fn attached(&self) -> bool {
        self.connections
            .lock()
            .map(|connections| !connections.is_empty())
            .unwrap_or(false)
    }

    fn publish(&self, envelope: Envelope) {
        if let Ok(mut log) = self.log.lock()
            && let Some(file) = log.as_mut()
            && let Ok(line) = serde_json::to_string(&envelope)
        {
            // A log that cannot be written is not worth ending a session over:
            // the terminal it would be reported to may not be attached. It
            // still goes to `tracing`, which is read by whoever runs the
            // daemon rather than whoever is attached to it.
            if let Err(error) = writeln!(file, "{line}") {
                tracing::error!(%error, "could not write to alate.log");
            }
        }

        let _ = self.envelopes.send(envelope);
    }
}

/// Publishes frames from a synchronous hook, on behalf of one session.
///
/// The hooks that produce frames — a delta, a tool call — know nothing about
/// sessions, so each session's plugins get a publisher that already knows which
/// one they belong to.
pub struct Publisher {
    shared: Arc<Shared>,
    session: Mutex<Option<String>>,
}

impl Publisher {
    /// A publisher for one session's frames.
    #[must_use]
    pub fn for_session(&self, session: impl Into<String>) -> Self {
        Self {
            shared: self.shared.clone(),
            session: Mutex::new(Some(session.into())),
        }
    }

    /// Send a frame, tagged with whichever session this publisher speaks for.
    pub fn send(&self, frame: Frame) {
        let session = match self.session.lock() {
            Ok(session) => session.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        self.shared.publish(Envelope { session, frame });
    }

    /// Get a file sender for one attachment-capable gateway connection.
    #[must_use]
    pub fn attachment_sender(&self, connection: u64) -> Option<AttachmentSender> {
        let connections = self.shared.connections.lock().ok()?;
        let held = connections.get(&connection)?;
        if !held
            .attachments
            .lock()
            .ok()
            .map(|value| *value)
            .unwrap_or(false)
        {
            return None;
        }
        Some(AttachmentSender {
            shared: self.shared.clone(),
            connection,
        })
    }
}

impl Clone for Publisher {
    fn clone(&self) -> Self {
        let session = match self.session.lock() {
            Ok(session) => session.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        Self {
            shared: self.shared.clone(),
            session: Mutex::new(session),
        }
    }
}

/// Asks whoever is attached, and refuses when nobody is.
///
/// The question goes to every terminal rather than only to the ones watching
/// that session. A permission question is about the machine, not about a
/// conversation, and being asked wherever you happen to be looking beats
/// finding out later that a job was refused because nobody was in its window.
struct GatewayConfirmer {
    shared: Arc<Shared>,
}

impl Confirmer for GatewayConfirmer {
    fn confirm(&self, tool: &str, summary: &str, risk: Risk) -> Decision {
        // Nobody attached is nobody to ask. An unattended alate that allowed
        // instead would be one that talks itself into anything overnight.
        if !self.shared.attached() {
            return Decision::Deny;
        }

        let (id, answer) = self.shared.answers.open();

        self.shared.publish(Envelope::daemon(Frame::Confirm {
            id,
            tool: tool.to_owned(),
            summary: summary.to_owned(),
            risk: risk.into(),
        }));

        // Blocking is correct here and matches `UiConfirmer`: this runs on the
        // session's own task, and the loop that serves terminals is another one.
        let decision = answer
            .recv_timeout(ANSWER_TIMEOUT)
            .unwrap_or(Decision::Deny);
        self.shared.answers.abandon(id);
        decision
    }
}

/// Take every terminal that arrives.
async fn accept(listener: UnixListener, shared: Arc<Shared>, events: mpsc::UnboundedSender<Event>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // The listener is gone, which is the daemon shutting down.
            return;
        };
        let id = shared.next_connection.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(serve(stream, id, shared.clone(), events.clone()));
    }
}

/// One terminal, until it hangs up.
async fn serve(
    stream: UnixStream,
    id: u64,
    shared: Arc<Shared>,
    events: mpsc::UnboundedSender<Event>,
) {
    // Subscribe before announcing, so a frame published while the daemon opens
    // this connection's session is received rather than lost in the gap.
    let mut envelopes = shared.envelopes.subscribe();
    let (direct, mut mine) = mpsc::unbounded_channel();

    let connection = Arc::new(Connection {
        current: Mutex::new(None),
        channel: Mutex::new(None),
        attachments: Mutex::new(false),
        direct,
    });
    if let Ok(mut connections) = shared.connections.lock() {
        connections.insert(id, connection.clone());
    }

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let mut announced = false;
    loop {
        tokio::select! {
            envelope = envelopes.recv() => match envelope {
                Ok(envelope) => {
                    let current = watching(&connection);
                    if envelope.is_for(current.as_deref())
                        && write(&mut writer, &envelope).await.is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    let notice = Envelope::daemon(Frame::Notice {
                        text: format!("this terminal fell behind and missed {missed} frames"),
                    });
                    if write(&mut writer, &notice).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // Meant for this terminal alone, so no filtering.
            envelope = mine.recv() => match envelope {
                Some(envelope) => {
                    if write(&mut writer, &envelope).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    let Ok(request) = serde_json::from_str::<Request>(&line) else {
                        continue;
                    };
                    // An answer belongs to the tool that is waiting on it, not
                    // to the daemon loop, so it never reaches the queue.
                    // A probe connects and hangs up without saying this, which
                    // is how `is_listening` checks that an alate is awake
                    // without leaving a conversation behind it.
                    if let Request::Attach { channel, attachments } = request {
                        announced = true;
                        // Before the event, so the daemon can read it back the
                        // moment it hears that somebody arrived.
                        if let Ok(mut said) = connection.channel.lock() {
                            *said = stated(channel);
                        }
                        if let Ok(mut supported) = connection.attachments.lock() {
                            *supported = attachments;
                        }
                        if events.send(Event::Opened { connection: id }).is_err() {
                            break;
                        }
                        continue;
                    }
                    if let Request::Answer { id: call, decision } = request {
                        let belongs = shared
                            .attachment_confirm_connections
                            .lock()
                            .ok()
                            .and_then(|waiting| waiting.get(&call).copied());
                        if let Some((owner, raw)) = belongs
                            && owner == id
                        {
                            shared.attachment_confirms.answer(raw, decision.into());
                        } else {
                            shared.answers.answer(call, decision.into());
                        }
                        continue;
                    }
                    if let Request::AttachmentResult {
                        id: attachment,
                        error,
                    } = request {
                        if shared
                            .attachment_answer_connections
                            .lock()
                            .ok()
                            .and_then(|waiting| waiting.get(&attachment).copied())
                            == Some(id)
                        {
                            shared
                                .attachment_answers
                                .answer(attachment, error.map_or(Ok(()), Err));
                        }
                        continue;
                    }
                    let asked = Event::Asked {
                        connection: id,
                        session: watching(&connection),
                        request,
                    };
                    if events.send(asked).is_err() {
                        break;
                    }
                }
                // End of stream, or a terminal that sent something unreadable.
                Ok(None) | Err(_) => break,
            },
        }
    }

    if announced {
        let _ = events.send(Event::Closed { connection: id });
    }
    finish(&shared, id);
}

fn watching(connection: &Arc<Connection>) -> Option<String> {
    match connection.current.lock() {
        Ok(current) => current.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Forget a terminal that has gone.
fn finish(shared: &Arc<Shared>, id: u64) {
    if let Ok(mut connections) = shared.connections.lock() {
        connections.remove(&id);
    }
    abandon_confirms(
        &shared.attachment_confirm_connections,
        &shared.attachment_confirms,
        id,
    );
    abandon_for(
        &shared.attachment_answer_connections,
        &shared.attachment_answers,
        id,
    );
}

fn abandon_confirms(
    waiting: &Mutex<HashMap<u64, (u64, u64)>>,
    answers: &Answers<Decision>,
    connection: u64,
) {
    let ids = waiting
        .lock()
        .map(|mut waiting| {
            let ids: Vec<(u64, u64)> = waiting
                .iter()
                .filter_map(|(id, (owner, raw))| (*owner == connection).then_some((*id, *raw)))
                .collect();
            for (id, _) in &ids {
                waiting.remove(id);
            }
            ids
        })
        .unwrap_or_default();
    for (_, raw) in ids {
        answers.abandon(raw);
    }
}

fn abandon_for<A>(waiting: &Mutex<HashMap<u64, u64>>, answers: &Answers<A>, connection: u64) {
    let ids = waiting
        .lock()
        .map(|mut waiting| {
            let ids: Vec<u64> = waiting
                .iter()
                .filter_map(|(id, owner)| (*owner == connection).then_some(*id))
                .collect();
            for id in &ids {
                waiting.remove(id);
            }
            ids
        })
        .unwrap_or_default();
    for id in ids {
        answers.abandon(id);
    }
}

async fn write(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    envelope: &Envelope,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(envelope)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await
}

/// Whether a socket file is one nobody is listening on.
fn stale(socket: &Path) -> bool {
    if !socket.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(socket).is_err()
}

/// What a client says it is, made fit to print.
///
/// Not a trust boundary: anything that can open the socket can already make the
/// agent run commands, so there is nothing here to defend. It is a rendering
/// one. This string goes into a session list, and a newline or a kilobyte of it
/// would break the list.
fn stated(channel: Option<String>) -> Option<String> {
    /// Long enough for `telegram: -1001234567890`, and short enough to sit in a
    /// column beside an id and a time.
    const LONGEST: usize = 32;

    let said = channel?;
    let clean: String = said
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(LONGEST)
        .collect();
    (!clean.is_empty()).then_some(clean)
}

/// Keep the socket to the user who made it.
///
/// Anything that can connect can make this agent run commands, so the file
/// permissions are the whole of the access control and have to be right.
fn restrict(socket: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
}

fn open_log(path: &Path) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}
