//! The app: state, the command set, and the loop that drives both.

use std::io::Stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aphid_agent::{Agent, AgentHandle, RunOutcome};
use aphid_core::{Model, ThinkingLevel, Transcript};
use aphid_plugin::{ScriptBackend, SessionInfo};
use compact_str::CompactString;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{ExecutableCommand, cursor};
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::prelude::CrosstermBackend;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::harness::{self, Harness, HarnessOptions};
use crate::model::{Catalog, ResolveError, clamp_thinking};
use crate::plugins::permissions::{Decision, Permissions};
use crate::plugins::scripts;
use crate::session::{self, SessionPlugin, sessions_dir};
use crate::tui::event::{UiConfirmer, UiEvent, UiPlugin, UiSink, spawn_input_thread};
use crate::tui::input::{Action, Input};
use crate::tui::modal::{Confirm, Modal};
use crate::tui::status::Status;
use crate::tui::view::View;

/// How often the screen is repainted while something is happening.
const FRAME: Duration = Duration::from_millis(33);

type Screen = Terminal<CrosstermBackend<Stdout>>;

/// A run in flight, plus the agent it borrowed.
type Running = tokio::task::JoinHandle<(Agent, RunOutcome)>;

/// Everything the UI holds.
pub struct App {
    pub view: View,
    pub input: Input,
    pub status: Status,
    pub modal: Option<Modal>,
    catalog: Catalog,
    thinking: Option<ThinkingLevel>,
    session: Option<Arc<SessionPlugin>>,
    /// Typed while the agent was busy, sent when the run settles.
    queued: Option<String>,
    handle: AgentHandle,
    quit: bool,
}

impl App {
    #[must_use]
    fn new(harness: &Harness, thinking: Option<ThinkingLevel>) -> Self {
        let mut status = Status::from_model(harness.agent.model());
        status.thinking = thinking.map(|level| level.as_str().to_owned());

        Self {
            view: View::default(),
            input: Input::default(),
            status,
            modal: None,
            catalog: Catalog::new(),
            thinking,
            session: None,
            queued: None,
            handle: harness.agent.handle(),
            quit: false,
        }
    }

    #[cfg(test)]
    fn new_for_test(agent: &Agent) -> Self {
        Self {
            view: View::default(),
            input: Input::default(),
            status: Status::from_model(agent.model()),
            modal: None,
            catalog: Catalog::new(),
            thinking: None,
            session: None,
            queued: None,
            handle: agent.handle(),
            quit: false,
        }
    }

    /// Replay a resumed transcript into the view, so the pane shows the
    /// conversation you are continuing rather than starting blank.
    fn replay(&mut self, transcript: &Transcript) {
        use aphid_core::{ContentRef, Role};

        for message in transcript.iter() {
            match message.role() {
                Role::System => {}
                Role::User => {
                    let text: String = message.content().filter_map(|c| c.text()).collect();
                    if !text.is_empty() {
                        self.view.push_user(text);
                    }
                }
                Role::Assistant => {
                    for content in message.content() {
                        match content {
                            ContentRef::Text(text) => self.view.push_text(text.text()),
                            ContentRef::Thinking(thinking) => {
                                self.view.push_thinking(thinking.text());
                            }
                            ContentRef::ToolCall(call) => {
                                self.view.push_tool_call(
                                    call.id(),
                                    call.name(),
                                    call.arguments_raw(),
                                );
                            }
                            ContentRef::Image(_) => {}
                        }
                    }
                }
                Role::ToolResult => {
                    let Some(meta) = message.tool_result() else {
                        continue;
                    };
                    let text: String = message.content().filter_map(|c| c.text()).collect();
                    self.view.finish_tool(
                        &meta.tool_call_id,
                        &text,
                        meta.is_error,
                        meta.details.clone(),
                    );
                }
            }
        }
    }

    /// Apply one event. Returns true when the screen needs repainting.
    fn apply(&mut self, event: UiEvent) -> bool {
        match event {
            UiEvent::TurnStarted => self.status.running = true,
            UiEvent::Text(text) => self.view.push_text(&text),
            UiEvent::Thinking(text) => self.view.push_thinking(&text),
            UiEvent::ToolCall {
                id,
                name,
                arguments,
            } => self.view.push_tool_call(&id, &name, &arguments),
            UiEvent::ToolProgress { id, chunk } => self.view.push_tool_progress(&id, &chunk),
            UiEvent::ToolResult {
                id,
                text,
                is_error,
                details,
                ..
            } => self.view.finish_tool(&id, &text, is_error, details),
            UiEvent::TurnEnded { usage, error, .. } => {
                self.status.last = Some(usage);
                self.status.total += usage;
                if let Some(error) = error {
                    self.view.push_notice(format!("error: {error}"));
                }
            }
            UiEvent::Notice(text) => self.view.push_notice(text),
            UiEvent::RunEnded(_) => self.status.running = false,
            UiEvent::Confirm {
                tool,
                summary,
                risk,
                reply,
            } => {
                self.modal = Some(Modal::Confirm(Confirm {
                    tool,
                    summary,
                    risk,
                    reply,
                }));
            }
            UiEvent::Key(_) | UiEvent::Resize => {}
        }
        true
    }

    /// The prompt waiting to be sent, if any.
    #[must_use]
    pub fn queued(&self) -> Option<&str> {
        self.queued.as_deref()
    }

    /// Handle a keypress that a modal is claiming.
    fn key_in_modal(&mut self, key: KeyEvent) -> Option<Model> {
        let Some(modal) = &mut self.modal else {
            return None;
        };

        match modal {
            Modal::Models { .. } => match key.code {
                KeyCode::Up => modal.move_selection(-1),
                KeyCode::Down => modal.move_selection(1),
                KeyCode::Enter => {
                    let chosen = modal.selected_model().cloned();
                    self.modal = None;
                    return chosen;
                }
                KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
                _ => {}
            },
            Modal::Confirm(_) => {
                let decision = match key.code {
                    KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(Decision::Allow),
                    KeyCode::Char('a' | 'A') => Some(Decision::AllowAlways),
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(Decision::Deny),
                    _ => None,
                };
                if let Some(decision) = decision
                    && let Some(Modal::Confirm(confirm)) = self.modal.take()
                {
                    confirm.answer(decision);
                }
            }
        }
        None
    }

    fn switch_model(&mut self, agent: &mut Agent, model: Model) {
        let (thinking, note) = clamp_thinking(&model, self.thinking);
        self.thinking = thinking;
        self.status.thinking = thinking.map(|level| level.as_str().to_owned());
        self.status.model = model.id.to_string();
        self.status.context_window = model.context_window;

        self.view
            .push_notice(format!("── switched to {} ──", model.id));
        if let Some(note) = note {
            self.view.push_notice(note);
        }

        // The key belongs to the provider, not to the session: switching to a
        // model from somewhere else has to switch credentials with it, or the
        // next request goes out signed by the wrong provider.
        match api_key(&model) {
            Ok(key) => agent.set_api_key(Some(key)),
            Err(note) => {
                agent.set_api_key(None);
                self.view.push_notice(note);
            }
        }

        agent.set_thinking(thinking);
        agent.set_model(model);
    }

    /// Run a slash command. Returns a prompt when the line was not one.
    fn command(&mut self, agent: &mut Agent, line: &str) -> Option<String> {
        if !line.starts_with('/') {
            return Some(line.to_owned());
        }

        let (name, rest) = line[1..].split_once(' ').unwrap_or((&line[1..], ""));
        let rest = rest.trim();

        match name {
            "quit" | "q" | "exit" => self.quit = true,
            "clear" | "new" => {
                self.view.clear();
                // Keep the system prompt; drop the conversation.
                let transcript = agent.transcript_mut();
                let keep = usize::from(
                    transcript
                        .get(0)
                        .is_some_and(|m| m.role() == aphid_core::Role::System),
                );
                transcript.truncate(keep);
                self.status.last = None;
                self.view.push_notice("── new session ──");
            }
            "model" => {
                if rest.is_empty() {
                    let models = self.catalog.models().to_vec();
                    let selected = self.catalog.position(&self.status.model).unwrap_or(0);
                    self.modal = Some(Modal::Models { models, selected });
                } else {
                    match self.catalog.resolve(rest) {
                        Ok(model) => self.switch_model(agent, model),
                        Err(error) => self.view.push_notice(match error {
                            ResolveError::Unknown { candidates } => {
                                format!("no model `{rest}`. Available: {}", candidates.join(", "))
                            }
                            ResolveError::Ambiguous { matches } => {
                                format!("`{rest}` is ambiguous: {}", matches.join(", "))
                            }
                        }),
                    }
                }
            }
            "think" => match parse_thinking(rest) {
                Ok(level) => {
                    let (level, note) = clamp_thinking(agent.model(), level);
                    self.thinking = level;
                    self.status.thinking = level.map(|level| level.as_str().to_owned());
                    agent.set_thinking(level);
                    self.view.push_notice(note.unwrap_or_else(|| {
                        format!(
                            "thinking {}",
                            level.map_or("off", aphid_core::ThinkingLevel::as_str)
                        )
                    }));
                }
                Err(message) => self.view.push_notice(message),
            },
            "tools" => {
                let names: Vec<&str> = agent.tools().names().collect();
                self.view
                    .push_notice(format!("tools: {}", names.join(", ")));
            }
            "session" => {
                let described = self
                    .session
                    .as_ref()
                    .and_then(|session| Some((session.id()?, session.path()?)))
                    .map_or_else(
                        || "not being saved".to_owned(),
                        |(id, path)| format!("{id} — {}", path.display()),
                    );
                self.view.push_notice(format!("session: {described}"));
            }
            "help" => self.view.push_notice(HELP),
            other => self
                .view
                .push_notice(format!("unknown command `/{other}` — try /help")),
        }

        None
    }
}

const HELP: &str = "\
── commands ──────────────────────────────────────
  /model  [name]  switch model, or open the picker
  /think  <level>  off | minimal | low | medium | high | xhigh | max
  /clear  /new     start a fresh conversation
  /tools           list the registered tools
  /session         where this session is being written
  /help            this list
  /quit            exit

── keys ──────────────────────────────────────────
  Esc         cancels a run
  Ctrl-C      quits
  Ctrl-P      cycles model
  Ctrl-T      shows reasoning
  PageUp/Dn   scroll";

fn parse_thinking(raw: &str) -> Result<Option<ThinkingLevel>, String> {
    Ok(match raw {
        "" | "off" | "none" => None,
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        "max" => Some(ThinkingLevel::Max),
        other => return Err(format!("`{other}` is not a thinking level — try /help")),
    })
}

/// Start the UI and run until the user quits.
///
/// # Errors
///
/// Fails when the terminal cannot be put into raw mode or drawn to.
pub async fn run(
    mut options: HarnessOptions,
    resume: Option<PathBuf>,
    confirm: bool,
) -> std::io::Result<()> {
    // Checked before anything is created, so a piped invocation gets advice
    // rather than a stray session file and an errno.
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the terminal UI needs a terminal — use `aphid -p \"…\"` to run a prompt without one",
        ));
    }

    let (events, receiver) = unbounded_channel();

    options
        .plugins
        .push(Arc::new(UiPlugin::new(events.clone())));
    if confirm {
        options
            .plugins
            .push(Arc::new(Permissions::new(Arc::new(UiConfirmer::new(
                events.clone(),
            )))));
    }

    let workspace = options.workspace.clone();
    let cwd = options.cwd.clone();
    let thinking = options.thinking;
    let model_id = options.model.id.to_string();

    // Scripts print through the app loop, because the UI owns the screen.
    let plugin_files = std::mem::take(&mut options.plugin_files);
    let (host, plugin_problems) = scripts::load(
        &workspace,
        &plugin_files,
        Arc::new(UiSink::new(events.clone())),
    );
    if let Some(backend) = ScriptBackend::install(&host)
        && options.stream_fn.is_none()
    {
        options.stream_fn = Some(backend);
    }
    if !host.is_empty() {
        options.plugins.push(host.clone());
        options.host = Some(host.clone());
    }

    // The session plugin has to be registered before the agent is built.
    let directory = sessions_dir(&workspace);
    let (session, resumed) = session::attach(&directory, &cwd, Some(&model_id), resume.as_deref())?;
    options.plugins.push(session.clone());

    let session_id = session.id();
    let session_path = session.path();
    host.session_start(&SessionInfo {
        id: session_id.as_deref(),
        path: session_path.as_deref(),
        reason: if resumed.is_some() { "resume" } else { "new" },
        restored: 0,
    });

    let mut harness = harness::build(options);
    let mut app = App::new(&harness, thinking);
    app.session = Some(session);
    app.view.watch(host.clone());

    for note in &harness.notes {
        app.view.push_notice(note.clone());
    }
    for diagnostic in &harness.diagnostics {
        app.view.push_notice(format!(
            "skipped skill {}: {}",
            diagnostic.path.display(),
            diagnostic.message
        ));
    }
    for problem in &plugin_problems {
        app.view.push_notice(problem.to_string());
    }
    if let Some(transcript) = resumed {
        app.replay(&transcript);
        let restored = transcript;
        // Splice the loaded conversation in after the freshly built system
        // prompt, so resuming picks up today's project context.
        let target = harness.agent.transcript_mut();
        let keep: Vec<_> = (0..restored.len())
            .filter(|index| {
                restored
                    .get(*index)
                    .is_some_and(|m| m.role() != aphid_core::Role::System)
            })
            .filter_map(|index| restored.id_at(index))
            .collect();
        restored.compact_into(&keep, target);
        app.view
            .push_notice(format!("── resumed {} messages ──", keep.len()));
    }
    app.view.push_notice(format!(
        "aphid · {} · {} — /help for commands",
        harness.agent.model().id,
        workspace.root().display()
    ));

    let mut terminal = setup()?;
    spawn_input_thread(events.clone());
    let result = drive(&mut terminal, &mut app, harness.agent, receiver).await;
    restore(&mut terminal)?;

    // After the terminal is back: a session hook that writes to standard error
    // then lands on a screen that is its own again. `session_end` also flushes
    // every plugin's state, so this is the last thing to run.
    host.session_end(&SessionInfo {
        id: session_id.as_deref(),
        path: session_path.as_deref(),
        reason: "end",
        restored: 0,
    });

    result
}

async fn drive(
    terminal: &mut Screen,
    app: &mut App,
    agent: Agent,
    mut receiver: UnboundedReceiver<UiEvent>,
) -> std::io::Result<()> {
    let mut idle: Option<Agent> = Some(agent);
    let mut running: Option<Running> = None;
    let mut dirty = true;

    while !app.quit {
        if std::mem::take(&mut dirty) {
            terminal.draw(|frame| render(frame, app))?;
        }

        tokio::select! {
            // A finished run hands the agent back.
            finished = async { running.as_mut().expect("only polled while running").await },
                if running.is_some() =>
            {
                running = None;
                app.status.running = false;
                match finished {
                    Ok((agent, _outcome)) => idle = Some(agent),
                    Err(error) => app.view.push_notice(format!("the run panicked: {error}")),
                }
                dirty = true;
            }
            event = receiver.recv() => {
                let Some(event) = event else { break };
                match event {
                    UiEvent::Key(key) => {
                        dirty = true;
                        handle_key(app, key, idle.as_mut());
                    }
                    UiEvent::Resize => dirty = true,
                    other => {
                        dirty = app.apply(other);
                    }
                }
            }
            // A repaint tick, so a streaming reply animates rather than
            // redrawing once per token.
            () = tokio::time::sleep(FRAME), if app.status.running => dirty = true,
        }

        // Anything typed while the agent was busy goes now that it is free.
        if let Some(started) = take_pending(&mut idle, app) {
            running = Some(started);
            dirty = true;
        }
    }

    // Leave nothing running behind us.
    app.handle.cancel();
    if let Some(running) = running {
        running.abort();
    }
    Ok(())
}

/// Hand the idle agent its queued prompt, if there is both.
///
/// Written as a guard plus two takes rather than as a tuple pattern: building
/// `(idle.take(), app.queued.take())` moves both out *before* the match, so a
/// failed pattern drops the agent instead of putting it back.
fn take_pending(idle: &mut Option<Agent>, app: &mut App) -> Option<Running> {
    if idle.is_none() || app.queued.is_none() {
        return None;
    }
    let agent = idle.take().expect("just checked");
    let prompt = app.queued.take().expect("just checked");

    app.status.queued = false;
    app.status.running = true;
    Some(start(agent, prompt))
}

fn start(mut agent: Agent, prompt: String) -> Running {
    tokio::spawn(async move {
        let outcome = agent.prompt(&prompt).await;
        (agent, outcome)
    })
}

fn handle_key(app: &mut App, key: KeyEvent, agent: Option<&mut Agent>) {
    if app.modal.is_some() {
        // The picker's choice can only be applied while the agent is idle; if a
        // run is in flight the switch waits for the next one.
        if let Some(model) = app.key_in_modal(key)
            && let Some(agent) = agent
        {
            app.switch_model(agent, model);
        }
        return;
    }

    match app.input.handle(key) {
        Action::None => {}
        Action::Quit => app.quit = true,
        Action::Cancel => {
            if app.status.running {
                app.handle.cancel();
                app.view.push_notice("── cancelled ──");
            } else {
                app.input.clear();
            }
        }
        Action::ScrollUp => app.view.scroll = app.view.scroll.saturating_add(10),
        Action::ScrollDown => app.view.scroll = app.view.scroll.saturating_sub(10),
        Action::ToggleThinking => app.view.show_thinking = !app.view.show_thinking,
        Action::CycleModel => {
            let next = app.catalog.next_after(&app.status.model.clone());
            if let (Some(model), Some(agent)) = (next, agent) {
                app.switch_model(agent, model);
            }
        }
        Action::Submit(line) => {
            let Some(prompt) = ({
                match agent {
                    Some(agent) => app.command(agent, &line),
                    // Mid-run, only prompts queue — commands need the agent.
                    None => Some(line),
                }
            }) else {
                return;
            };

            app.view.push_user(prompt.clone());
            app.queued = Some(prompt);
            app.status.queued = app.status.running;
        }
    }
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let [transcript, input, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let width = transcript.width as usize;
    let lines = app.view.lines(width);
    let height = transcript.height as usize;

    // Pinned to the bottom unless the user scrolled up.
    let max_scroll = lines.len().saturating_sub(height);
    let scroll = app.view.scroll.min(max_scroll);
    let top = max_scroll - scroll;
    let visible: Vec<Line<'_>> = lines.into_iter().skip(top).take(height).collect();
    frame.render_widget(Paragraph::new(visible), transcript);

    let prompt = if app.status.running { "…" } else { ">" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{prompt} "), Style::default().fg(Color::Cyan)),
            Span::raw(app.input.text().to_owned()),
        ])),
        input,
    );
    frame.render_widget(Paragraph::new(app.status.line()), status);

    if app.modal.is_none() {
        frame.set_cursor_position(Position::new(
            input.x + 2 + app.input.cursor_column() as u16,
            input.y,
        ));
    }
    if let Some(modal) = &app.modal {
        modal.render(frame, frame.area());
    }
}

fn setup() -> std::io::Result<Screen> {
    // Restore the terminal even when something panics, so a crash does not
    // leave the shell in raw mode.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        previous(info);
    }));

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore(terminal: &mut Screen) -> std::io::Result<()> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.backend_mut().execute(cursor::Show)?;
    Ok(())
}

/// The key for a model, from the variable the model itself names.
///
/// The error is a notice rather than a failure: the user may be about to export
/// the variable, and a session that dies on a mistyped `/model` would be worse
/// than one that says what is missing.
fn api_key(model: &Model) -> Result<CompactString, String> {
    let Some(variable) = &model.api_key_env else {
        return Err(format!("{} names no API key variable", model.id));
    };
    match std::env::var(variable.as_str()) {
        Ok(key) if !key.is_empty() => Ok(key.into()),
        _ => Err(format!("{variable} is not set, and {} needs it", model.id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aphid_agent::testing::{Turn, scripted};
    use aphid_core::{Role, providers::deepseek};
    use ratatui::crossterm::event::KeyModifiers;

    fn agent_with(turns: Vec<Turn>) -> Agent {
        let (backend, _script) = scripted(turns);
        Agent::builder()
            .model(deepseek::flash())
            .system("terse")
            .stream_fn(backend)
            .build()
    }

    fn app_for(agent: &Agent) -> App {
        let mut app = App::new_for_test(agent);
        app.status = Status::from_model(agent.model());
        app
    }

    fn type_line(app: &mut App, agent: &mut Option<Agent>, line: &str) {
        for c in line.chars() {
            handle_key(
                app,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                agent.as_mut(),
            );
        }
        handle_key(
            app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            agent.as_mut(),
        );
    }

    #[tokio::test]
    async fn submitting_a_message_actually_starts_a_run() {
        let agent = agent_with(vec![Turn::text("hello back")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        type_line(&mut app, &mut idle, "hello");
        assert_eq!(app.queued(), Some("hello"), "the line should be queued");

        let running = take_pending(&mut idle, &mut app).expect("a run should start");
        assert!(idle.is_none(), "the agent moved into the run");

        let (agent, outcome) = running.await.expect("the run should not panic");
        assert_eq!(outcome.turns, 1);

        // system, user, assistant
        assert_eq!(agent.transcript().len(), 3);
        assert_eq!(agent.transcript().get(1).unwrap().role(), Role::User);
    }

    #[tokio::test]
    async fn an_idle_loop_tick_does_not_lose_the_agent() {
        // The regression: taking the agent out to test a tuple pattern dropped
        // it whenever nothing was queued, so every later submit did nothing.
        let agent = agent_with(vec![Turn::text("late reply")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        for _ in 0..5 {
            assert!(take_pending(&mut idle, &mut app).is_none());
            assert!(idle.is_some(), "the agent must survive an idle tick");
        }

        type_line(&mut app, &mut idle, "still there?");
        let running = take_pending(&mut idle, &mut app).expect("a run should still start");
        let (agent, outcome) = running.await.expect("no panic");
        assert_eq!(outcome.turns, 1);
        assert_eq!(agent.transcript().len(), 3);
    }

    #[tokio::test]
    async fn a_message_typed_mid_run_is_sent_once_the_agent_is_free() {
        let agent = agent_with(vec![Turn::text("first"), Turn::text("second")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        type_line(&mut app, &mut idle, "one");
        let running = take_pending(&mut idle, &mut app).expect("first run");

        // Typed while the agent is away: no agent to hand it to yet.
        app.status.running = true;
        type_line(&mut app, &mut idle, "two");
        assert_eq!(app.queued(), Some("two"));
        assert!(app.status.queued, "the status line should say so");
        assert!(take_pending(&mut idle, &mut app).is_none());

        let (agent, _) = running.await.expect("no panic");
        idle = Some(agent);

        let running = take_pending(&mut idle, &mut app).expect("the queued line goes now");
        assert!(!app.status.queued);
        let (agent, _) = running.await.expect("no panic");

        // system, user, assistant, user, assistant
        assert_eq!(agent.transcript().len(), 5);
    }

    #[tokio::test]
    async fn a_slash_command_is_handled_rather_than_sent() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        type_line(&mut app, &mut idle, "/tools");

        assert!(app.queued().is_none(), "a command is not a prompt");
        assert!(take_pending(&mut idle, &mut app).is_none());
        assert!(idle.is_some());
    }
}
