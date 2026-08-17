//! The loop that keeps an alate awake.
//!
//! It is the coding agent's loop with the terminal taken out, a clock put in,
//! and one conversation grown into several. Every session runs on its own task
//! and hands its agent back when the run ends; the loop meanwhile serves
//! terminals, ticks the plugins and watches the time.
//!
//! It does not run on [`aphid_code::tui::runtime`], as the three terminals do.
//! That loop waits on a channel and up to three clocks; this one also waits on
//! a [`JoinSet`](tokio::task::JoinSet) of running sessions, and the same set is
//! read by the code that would be its update.
//!
//! Four things put words to an alate, and each says which conversation it means:
//!
//! - a terminal, into the session it is watching;
//! - a rhai plugin calling `prompt`, into the resident session;
//! - the heartbeat, into the resident session;
//! - a cron job, into a session opened for it, which is why a job never lands
//!   in the middle of what somebody was saying.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_agent::StreamFn;
use aphid_code::model::Catalog;
use aphid_code::plugins::scripts;
use aphid_code::session::sessions_dir;
use aphid_code::tools::Workspace;
use aphid_plugin::{PluginHost, ScriptBackend, SessionInfo};
use chrono::Local;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::config::Config;
use crate::cron::{self, Crontab};
use crate::gateway::wire::{Envelope, Frame, Request};
use crate::gateway::{Event, GatewaySink, Server};
use crate::heartbeat::Schedule;
use crate::home::Home;
use crate::memory::Memory;
use crate::sessions::{Blueprint, Kind, Sessions, stored};

/// How often the rhai plugins get their `on_tick`. The terminal UI's cadence,
/// for the same reason: often enough to feel live, rare enough to cost nothing.
const TICK: Duration = Duration::from_millis(250);

/// How often the clock is read. A heartbeat and a crontab are measured in
/// minutes, so this is finer than either needs.
const CLOCK: Duration = Duration::from_secs(5);

/// What to start an alate with.
pub struct Options {
    pub home: Home,
    pub config: Config,
    /// The model to run, when the caller already resolved one. `None` resolves
    /// it from the catalog, as the CLI does. Tests pass a dummy — the scripted
    /// backend never contacts it — so a run does not depend on the machine's
    /// `~/.aphid/models.json`.
    pub model: Option<aphid_core::Model>,
    /// Replace the provider backend. `None` talks to the real provider; tests
    /// pass a scripted one, exactly as [`HarnessOptions::stream_fn`] does.
    ///
    /// [`HarnessOptions::stream_fn`]: aphid_code::harness::HarnessOptions::stream_fn
    pub stream_fn: Option<StreamFn>,
}

/// Everything the loop holds between passes.
struct Alate {
    home: Home,
    server: Server,
    host: Arc<PluginHost>,
    blueprint: Blueprint,
    sessions: Sessions,
    schedule: Schedule,
    crontab: cron::Shared,
    workspace: Workspace,
    model: String,
    context_window: u32,
    thinking: Option<String>,
}

/// Run one instance until it is asked to stop.
///
/// # Errors
///
/// Fails when the model cannot be resolved, the API key is missing, the socket
/// cannot be bound — which usually means this instance is already running — or
/// the resident session cannot be opened.
pub async fn run(options: Options) -> Result<(), String> {
    // Ignored: a test that runs more than one alate in the same process calls
    // this more than once, and a second `set_global_default` must not panic.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();

    let Options {
        home,
        config,
        model: given,
        stream_fn,
    } = options;

    // A caller that already resolved a model — the tests — needs no catalog
    // and no `~/.aphid/models.json`.
    let catalog = Catalog::new();
    let model = match given {
        Some(model) => model,
        None => match &config.model {
            Some(name) => catalog.resolve(name).map_err(|error| error.to_string())?,
            None => catalog.default_model().ok_or_else(|| {
                "no models configured. Add one with `aphid models add <provider/model>`.".to_owned()
            })?,
        },
    };

    // A scripted backend reaches no provider, so it needs no key. Demanding one
    // would mean a test had to put a fake in the environment of the whole
    // process, which is both a lie and a race between tests.
    let api_key = match stream_fn {
        Some(_) => None,
        None => Some(api_key(&model)?),
    };

    // The workspace is the home unless the configuration points elsewhere, so a
    // fresh alate can only write inside the directory it was given.
    let workspace = Workspace::new(
        config
            .workspace
            .clone()
            .unwrap_or_else(|| home.root().to_path_buf()),
    );

    let memory = Arc::new(Mutex::new(
        Memory::open(&home.memory_dir()).map_err(|error| error.to_string())?,
    ));
    let (crontab, crontab_problems) = Crontab::open(&home.cron_file());
    let crontab = Arc::new(Mutex::new(crontab));
    let schedule = Schedule::open(
        &home.state_file(),
        &config.heartbeat,
        std::fs::read_to_string(home.heartbeat_file()).ok(),
    );

    let socket = config
        .gateway
        .socket
        .clone()
        .unwrap_or_else(|| home.socket());
    let (server, events) =
        Server::bind(&socket, Some(&home.log_file())).map_err(|error| match error.kind() {
            // The one failure with an obvious cause and an obvious fix, so it
            // gets its own sentence rather than being guessed at for every
            // other kind of failure too.
            std::io::ErrorKind::AddrInUse => format!(
                "{} is already in use — this alate is probably already running",
                socket.display()
            ),
            _ => format!("could not listen on {}: {error}", socket.display()),
        })?;

    // Once for the whole daemon, not once per session: loading the scripts
    // twice would double every hook, and the host's own session is the
    // daemon's lifetime.
    let processes = Arc::new(aphid_agent::exec::Registry::new());
    let (files, discovery) = scripts::discover(&workspace, None);
    let (host, mut problems) = scripts::load(
        &workspace,
        &files,
        Arc::new(GatewaySink::new(server.publisher())),
        &processes,
    );
    problems.extend(discovery);

    let mut stream_fn = stream_fn;
    if let Some(backend) = ScriptBackend::install(&host)
        && stream_fn.is_none()
    {
        stream_fn = Some(backend);
    }
    host.session_start(&SessionInfo {
        id: None,
        path: None,
        reason: "new",
        restored: 0,
    });

    // The tools' half of the colony, made before the blueprint because every
    // session registers them. The bridge's half is started after the socket is
    // bound, below.
    #[cfg(feature = "colony")]
    let (colony, colony_outbound) = colony_handle(&config);

    let blueprint = Blueprint {
        home: home.clone(),
        config: config.clone(),
        model: model.clone(),
        api_key,
        workspace: workspace.clone(),
        publisher: server.publisher(),
        memory,
        crontab: crontab.clone(),
        host: (!host.is_empty()).then(|| host.clone()),
        stream_fn,
        processes,
        permissions: Blueprint::permissions(&config, server.confirmer()),
        #[cfg(feature = "colony")]
        colony: colony.clone(),
    };

    let mut alate = Alate {
        model: model.id.to_string(),
        context_window: model.context_window,
        thinking: config
            .thinking
            .map(|level| format!("{level:?}").to_lowercase()),
        home,
        server,
        host: host.clone(),
        blueprint,
        sessions: Sessions::new(),
        schedule,
        crontab,
        workspace,
    };

    // The resident session, which the heartbeat wakes into and which outlives
    // every terminal.
    let resident = alate.blueprint.open(Kind::Resident)?;
    let resident_id = resident.id.clone();
    alate.sessions.insert(resident);
    alate.opened(&resident_id);

    for problem in problems
        .iter()
        .map(ToString::to_string)
        .chain(crontab_problems)
    {
        tracing::warn!(problem = %problem, "startup problem");
        alate
            .server
            .send(Envelope::daemon(Frame::Notice { text: problem }));
    }

    // Said here and not by the caller, because here is where it becomes true:
    // the socket is bound and the resident session exists.
    let name = alate.home.name();
    eprintln!(
        "aphid: {name} is awake in {}\naphid: attach with `aphid alate attach --name {name}`",
        alate.home.root().display()
    );
    tracing::info!(name = %name, home = %alate.home.root().display(), "daemon awake");

    // A second client on the same socket, when one is asked for. It is started
    // here because here is where the socket is bound; the loop below neither
    // knows about it nor waits on it.
    #[cfg(feature = "telegram")]
    let telegram = telegram_bridge(&config, &socket, &alate.server);
    #[cfg(feature = "colony")]
    let colony_bridge = colony_bridge(
        &config,
        &socket,
        &alate.server,
        alate.home.name(),
        colony,
        colony_outbound,
    );
    #[cfg(not(feature = "colony"))]
    if config.gateway.colony.is_some() {
        tracing::warn!("colony configured but this build has no colony feature");
        alate.server.send(Envelope::daemon(Frame::Notice {
            text: "gateway.colony is set, but this build has no colony in it; \
                   build with `--features colony`"
                .to_owned(),
        }));
    }

    #[cfg(not(feature = "telegram"))]
    if config.gateway.telegram.is_some() {
        tracing::warn!("telegram configured but this build has no telegram feature");
        alate.server.send(Envelope::daemon(Frame::Notice {
            text: "gateway.telegram is set, but this build has no Telegram in it; \
                   build with `--features telegram`"
                .to_owned(),
        }));
    }

    drive(&mut alate, events).await;

    #[cfg(feature = "telegram")]
    if let Some(telegram) = telegram {
        telegram.abort();
    }
    #[cfg(feature = "colony")]
    if let Some(colony) = colony_bridge {
        colony.abort();
    }
    alate.sessions.shutdown();
    alate.host.session_end(&SessionInfo {
        id: None,
        path: None,
        reason: "end",
        restored: 0,
    });
    Ok(())
}

impl Alate {
    /// Tell everybody a session started.
    fn opened(&self, id: &str) {
        if let Some(session) = self.sessions.get(id) {
            tracing::info!(session = %id, kind = %session.kind.label(), "session opened");
            self.server.send(Envelope::daemon(Frame::SessionOpened {
                info: session.info(),
            }));
        }
    }

    /// Put words into a session, and show everybody watching it what was said.
    fn enqueue(&mut self, id: &str, text: String) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        session.enqueue(text.clone());
        tracing::info!(session = %id, channel = %session.kind.label(), "message enqueued");
        self.server.send(Envelope::from(id, Frame::Prompt { text }));
    }

    /// The session a plugin's `prompt` and the heartbeat go to.
    fn resident(&self) -> Option<String> {
        self.sessions.resident().map(ToOwned::to_owned)
    }

    /// Open a session, announce it, and give back its id.
    fn open(&mut self, kind: Kind) -> Option<String> {
        match self.blueprint.open(kind) {
            Ok(session) => {
                let id = session.id.clone();
                self.sessions.insert(session);
                self.opened(&id);
                Some(id)
            }
            Err(error) => {
                self.server
                    .send(Envelope::daemon(Frame::Notice { text: error }));
                None
            }
        }
    }

    fn close(&mut self, id: &str) {
        if self.sessions.close(id).is_some() {
            tracing::info!(session = %id, "session closed");
            self.server
                .send(Envelope::daemon(Frame::SessionClosed { id: id.to_owned() }));
        }
    }

    /// Replay a session to one terminal, live or stored, and point it there.
    ///
    /// The same path for both: a running session's transcript is the same shape
    /// as a finished one's, so there is one way to draw a conversation and no
    /// second one to keep in step.
    fn watch(&self, connection: u64, id: &str) {
        let path = match self
            .sessions
            .get(id)
            .and_then(crate::sessions::Session::path)
        {
            Some(path) => Some(path),
            None => aphid_code::session::resolve(&sessions_dir(&self.workspace), id)
                .map(|summary| summary.path),
        };
        let Some(path) = path else {
            self.server.reply(
                connection,
                Envelope::daemon(Frame::Notice {
                    text: format!("there is no session {id}"),
                }),
            );
            return;
        };

        self.server.watch(connection, id);
        self.server.reply(
            connection,
            Envelope::from(id, Frame::HistoryStart { id: id.to_owned() }),
        );

        match aphid_code::session::load(&path) {
            Ok((_header, transcript)) => {
                for frame in replay(&transcript) {
                    self.server.reply(connection, Envelope::from(id, frame));
                }
            }
            Err(error) => self.server.reply(
                connection,
                Envelope::from(
                    id,
                    Frame::Notice {
                        text: format!("could not read {}: {error}", path.display()),
                    },
                ),
            ),
        }

        self.server.reply(
            connection,
            Envelope::from(id, Frame::HistoryEnd { id: id.to_owned() }),
        );
    }
}

/// The loop.
async fn drive(alate: &mut Alate, mut events: UnboundedReceiver<Event>) {
    // Armed only when a plugin is waiting on it, so an instance with no scripts
    // pays for no timer. An `interval` and not a fresh `sleep`: a sleep built
    // inside `select!` restarts whenever another branch wins.
    let ticked = alate
        .host
        .any_defines("on_tick")
        .then(|| alate.host.clone());
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut clock = tokio::time::interval(CLOCK);
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            (id, outcome) = alate.sessions.finished() => {
                if outcome.is_none() {
                    tracing::error!(session = %id, "session panicked");
                    alate.server.send(Envelope::daemon(Frame::Notice {
                        text: format!("the run in session {id} panicked; the session is closed"),
                    }));
                    alate.server.send(Envelope::daemon(Frame::SessionClosed { id: id.clone() }));
                    continue;
                }
                // A job's session exists for its run, and the run is over. Its
                // transcript stays on disk, where `/session` can still open it.
                if matches!(alate.sessions.get(&id).map(|s| &s.kind), Some(Kind::Cron { .. })) {
                    alate.close(&id);
                }
            }
            event = events.recv() => match event {
                // The listener is gone, which is the daemon shutting down.
                None => break,
                Some(event) => handle(alate, event),
            },
            // Dispatched off the loop: a hook that reaches for `exec` blocks the
            // plugin worker, and the loop that serves terminals must not wait on
            // that. A plugin's `prompt` arrives through the sink, so nothing is
            // lost by not awaiting the tick.
            _ = ticker.tick(), if ticked.is_some() => {
                let host = ticked.clone().expect("only polled while some");
                tokio::task::spawn_blocking(move || host.tick());
            }
            _ = clock.tick() => wake(alate),
            () = shutdown() => break,
        }

        // Every session that can run, runs. This is what makes a job concurrent
        // with a conversation: a long turn in one session holds up nothing in
        // another.
        //
        // Nothing is announced here. `GatewayPlugin::on_turn_start` reports the
        // turn from inside the run, and a frame sent here as well would be the
        // same news twice — on the wire and in `alate.log`.
        alate.sessions.start_ready();
    }
}

/// What a terminal asked for.
fn handle(alate: &mut Alate, event: Event) {
    match event {
        Event::Opened { connection } => {
            // A client gets a conversation of its own, so two people attached
            // are not typing into one transcript. It is named after whatever
            // the client said it was, so a listing tells a terminal from a chat.
            let channel = alate.server.channel(connection);
            tracing::info!(connection, channel = ?channel, "client connected");
            let Some(id) = alate.open(Kind::Attached {
                connection,
                channel,
            }) else {
                return;
            };
            alate.server.watch(connection, &id);
            // In an envelope naming the session, which is how the terminal
            // learns which conversation is its own.
            alate.server.reply(
                connection,
                Envelope::from(
                    &id,
                    Frame::Hello {
                        instance: alate.home.name().to_owned(),
                        model: alate.model.clone(),
                        context_window: alate.context_window,
                        thinking: alate.thinking.clone(),
                    },
                ),
            );
        }
        Event::Closed { connection } => {
            tracing::info!(connection, "client disconnected");
            for id in alate.sessions.owned_by(connection) {
                alate.close(&id);
            }
        }
        Event::Asked {
            connection,
            session,
            request,
        } => match request {
            Request::Prompt { text } => {
                if let Some(id) = session {
                    alate.enqueue(&id, text);
                }
            }
            Request::Cancel => {
                if let Some(session) = session.and_then(|id| alate.sessions.get(&id)) {
                    session.cancel();
                }
            }
            Request::Watch { id } => alate.watch(connection, &id),
            Request::Sessions => {
                let live = alate.sessions.list();
                let stored = stored(&alate.workspace, &alate.sessions);
                alate.server.reply(
                    connection,
                    Envelope::daemon(Frame::Sessions { live, stored }),
                );
            }
            Request::New => {
                let channel = alate.server.channel(connection);
                if let Some(id) = alate.open(Kind::Attached {
                    connection,
                    channel,
                }) {
                    alate.watch(connection, &id);
                }
            }
            // Both answered inside the server: an answer belongs to the tool
            // waiting on it, and an attach has already been turned into
            // `Event::Opened`.
            Request::Answer { .. } | Request::Attach { .. } => {}
        },
    }
}

/// The clock: the heartbeat, and anything the crontab has come due.
fn wake(alate: &mut Alate) {
    let now = Local::now();

    // Into the resident session, and only while it is idle. A heartbeat that
    // queued behind a running turn would arrive with its moment already past.
    if let Some(id) = alate.resident()
        && alate
            .sessions
            .get(&id)
            .is_some_and(|session| session.ready() || !session.info().running)
        && let Some(note) = alate.schedule.due(now.with_timezone(&chrono::Utc))
    {
        alate.server.send(Envelope::daemon(Frame::Heartbeat {
            at: now.format("%Y-%m-%d %H:%M %Z").to_string(),
            note: note.clone(),
        }));
        alate.enqueue(&id, note);
    }

    let due = cron::lock(&alate.crontab).due(now);
    for entry in due {
        // Its own session, always: a job starts from nothing and leaves nothing
        // behind in a conversation somebody else is having.
        let Some(id) = alate.open(Kind::Cron {
            name: entry.name.clone(),
        }) else {
            continue;
        };
        // Nothing is announced here: `open` already sent `SessionOpened`, and
        // its kind names the job.
        alate.enqueue(&id, entry.prompt.clone());
    }
}

/// A stored conversation, as the frames that would have drawn it.
///
/// System messages are left out: the prompt, the recalled facts and the map of
/// the memory are how the agent was set up, not what was said.
fn replay(transcript: &aphid_core::Transcript) -> Vec<Frame> {
    use aphid_core::{ContentRef, Role};

    let mut frames = Vec::new();
    for message in transcript.iter() {
        match message.role() {
            Role::System => {}
            Role::User => {
                let text: String = message
                    .content()
                    .filter_map(|content| match content {
                        ContentRef::Text(text) => Some(text.text().to_owned()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    frames.push(Frame::Prompt { text });
                }
            }
            Role::Assistant => {
                for content in message.content() {
                    match content {
                        ContentRef::Text(text) if !text.text().is_empty() => {
                            frames.push(Frame::Text {
                                text: text.text().to_owned(),
                            });
                        }
                        ContentRef::Thinking(thinking) if !thinking.text().is_empty() => {
                            frames.push(Frame::Thinking {
                                text: thinking.text().to_owned(),
                            });
                        }
                        ContentRef::ToolCall(call) => frames.push(Frame::ToolCall {
                            id: call.id().to_owned(),
                            name: call.name().to_owned(),
                            arguments: call.arguments_raw().to_owned(),
                        }),
                        _ => {}
                    }
                }
            }
            Role::ToolResult => {
                let text: String = message
                    .content()
                    .filter_map(|content| match content {
                        ContentRef::Text(text) => Some(text.text().to_owned()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let meta = message.tool_result();
                frames.push(Frame::ToolResult {
                    id: meta
                        .map(|meta| meta.tool_call_id.to_string())
                        .unwrap_or_default(),
                    name: meta
                        .map(|meta| meta.tool_name.to_string())
                        .unwrap_or_default(),
                    text,
                    is_error: meta.is_some_and(|meta| meta.is_error),
                    details: None,
                });
            }
        }
    }
    frames
}

/// Resolves when the process is asked to stop.
///
/// Both signals, because an alate is as likely to be stopped by a service
/// manager as by somebody at a keyboard.
async fn shutdown() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        // With no way to listen, waiting for ever is right: the loop's other
        // branches still work, and the process can still be killed.
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// Start the Telegram bridge, when the configuration asks for one.
///
/// Every way this can fail is reported and shrugged off. A bot with no token is
/// a reason to have no bot, not a reason for the alate not to wake up: the
/// socket works, a terminal works, and the sentence saying why is on the wire
/// and in `alate.log`.
#[cfg(feature = "telegram")]
fn telegram_bridge(
    config: &Config,
    socket: &std::path::Path,
    server: &Server,
) -> Option<tokio::task::JoinHandle<()>> {
    let wanted = config.gateway.telegram.as_ref()?;
    let notices = server.publisher();

    let poll = match wanted.interval() {
        Ok(poll) => poll,
        Err(why) => {
            tracing::error!(%why, "telegram: bad poll interval");
            notices.send(Frame::Notice {
                text: format!("telegram: {why}"),
            });
            return None;
        }
    };

    let token = match std::env::var(&wanted.token_env) {
        Ok(token) if !token.is_empty() => token,
        _ => {
            tracing::error!(env = %wanted.token_env, "telegram: bot token not set");
            notices.send(Frame::Notice {
                text: format!(
                    "telegram: {} is not set, and the bot needs it",
                    wanted.token_env
                ),
            });
            return None;
        }
    };

    let api = match crate::telegram::Live::new(
        wanted.api.as_deref().unwrap_or(crate::telegram::API),
        &token,
        poll,
    ) {
        Ok(api) => api,
        Err(why) => {
            tracing::error!(%why, "telegram: could not start api client");
            notices.send(Frame::Notice {
                text: format!("telegram: {why}"),
            });
            return None;
        }
    };

    // Said out loud, because a bot that answers every chat with a refusal looks
    // broken and is only unconfigured.
    if wanted.chats.is_empty() {
        tracing::warn!("telegram: chats allow-list is empty");
        notices.send(Frame::Notice {
            text: "telegram: gateway.telegram.chats is empty, so every chat is refused. \
                   A refused chat is told the id to add."
                .to_owned(),
        });
    }

    tracing::info!("telegram bridge started");
    Some(crate::telegram::spawn(crate::telegram::Bridge {
        socket: socket.to_path_buf(),
        config: wanted.clone(),
        api: Arc::new(api),
        notices: server.publisher(),
    }))
}

/// The API key for a model, from the variable it names.
fn api_key(model: &aphid_core::Model) -> Result<String, String> {
    let variable = model
        .api_key_env
        .as_deref()
        .unwrap_or(aphid_core::providers::deepseek::API_KEY_ENV);
    match std::env::var(variable) {
        Ok(key) if !key.is_empty() => Ok(key),
        _ => Err(format!("{variable} is not set, and {} needs it", model.id)),
    }
}

/// The tools' half of a colony.
///
/// Made whether or not a colony is configured, because the alternative is a
/// `None` threaded through every session for a feature that is off. With no
/// bridge on the other end the tools answer "the colony bridge has stopped",
/// which is true and is what a model needs to hear.
#[cfg(feature = "colony")]
fn colony_handle(
    config: &Config,
) -> (
    Option<crate::colony::Shared>,
    tokio::sync::mpsc::UnboundedReceiver<crate::colony::Outbound>,
) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let wanted = config.gateway.colony.as_ref();

    let colony = wanted.and_then(|wanted| match crate::colony::keys(&wanted.key_env) {
        Ok(keys) => Some(Arc::new(crate::colony::Colony::new(
            sender,
            keys.public_key(),
        ))),
        Err(why) => {
            tracing::error!(%why, "colony: no key");
            None
        }
    });
    (colony, receiver)
}

/// The bridge's half.
///
/// Every failure here is a notice and a `None`, never a stop: a colony with no
/// relay is a reason to have no colony, not a reason for the alate not to wake
/// up. The same rule [`telegram_bridge`] follows.
#[cfg(feature = "colony")]
fn colony_bridge(
    config: &Config,
    socket: &std::path::Path,
    server: &Server,
    name: &str,
    colony: Option<crate::colony::Shared>,
    outbound: tokio::sync::mpsc::UnboundedReceiver<crate::colony::Outbound>,
) -> Option<tokio::task::JoinHandle<()>> {
    let wanted = config.gateway.colony.as_ref()?;
    let notices = server.publisher();

    let Some(colony) = colony else {
        notices.send(Frame::Notice {
            text: format!(
                "colony: {} is not set, and this agent needs a key to talk with",
                wanted.key_env
            ),
        });
        return None;
    };

    let keys = match crate::colony::keys(&wanted.key_env) {
        Ok(keys) => keys,
        Err(why) => {
            notices.send(Frame::Notice {
                text: format!("colony: {why}"),
            });
            return None;
        }
    };

    if let Err(why) = wanted.interval() {
        notices.send(Frame::Notice {
            text: format!("colony: {why}"),
        });
        return None;
    }

    let url = wanted.relay.clone();
    let connect: crate::colony::Connect = Arc::new(move || {
        let url = url.clone();
        Box::pin(async move {
            crate::colony::Live::connect(&url)
                .await
                .map(|live| Arc::new(live) as crate::colony::RelayFn)
        })
    });

    tracing::info!(relay = %wanted.relay, "colony bridge started");
    Some(crate::colony::spawn(crate::colony::Bridge {
        socket: socket.to_path_buf(),
        config: wanted.clone(),
        keys,
        name: wanted.name.clone().unwrap_or_else(|| name.to_owned()),
        connect,
        notices,
        colony,
        outbound,
    }))
}
