//! The app: state, the command set, and the loop that drives both.

use std::collections::VecDeque;
use std::io::Stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aphid_agent::{Agent, AgentHandle, RunOutcome, exec};
use aphid_core::{Model, ThinkingLevel, Transcript};
use aphid_plugin::{Action as PluginAction, ScriptBackend, SessionInfo};
use compact_str::CompactString;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, KeyCode,
    KeyEvent, KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::crossterm::{ExecutableCommand, cursor};
use ratatui::layout::{Constraint, Layout, Margin};
use ratatui::prelude::CrosstermBackend;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::harness::{self, Harness, HarnessOptions};
use crate::model::{Catalog, ResolveError, clamp_thinking};
use crate::plugins::permissions::{Decision, Permissions};
use crate::plugins::scripts;
use crate::session::{self, SessionPlugin, sessions_dir};
use crate::skills::{self, Skill};
use crate::tui::event::{UiConfirmer, UiEvent, UiPlugin, UiSink, spawn_input_thread};
use crate::tui::input::{Action, Input};
use crate::tui::modal::{Confirm, Modal};
use crate::tui::status::Status;
use crate::tui::view::{View, one_line};

/// How often the screen is repainted while something is happening.
const FRAME: Duration = Duration::from_millis(33);

/// How often a plugin's `on_tick` runs.
///
/// Fast enough that a plugin watching something outside the session feels
/// immediate, slow enough that a hook doing real work has finished before the
/// next one is due.
const TICK: Duration = Duration::from_millis(250);

/// How often the process list is redrawn while it is open. It counts in
/// seconds, so this is as often as it can possibly need.
const PS_REFRESH: Duration = Duration::from_millis(250);

/// How many transcript lines one mouse wheel step moves. Smaller than the
/// keyboard page step because a wheel sends many events in quick succession.
const MOUSE_SCROLL_LINES: usize = 3;

type Screen = Terminal<CrosstermBackend<Stdout>>;

/// A run in flight, plus the agent it borrowed.
type Running = tokio::task::JoinHandle<(Agent, RunOutcome)>;

/// Everything the UI holds.
pub struct App {
    pub view: View,
    pub input: Input,
    pub status: Status,
    pub modal: Option<Modal>,
    /// The loaded plugins, for the commands they registered and `/plugins`.
    host: Option<Arc<aphid_plugin::PluginHost>>,
    catalog: Catalog,
    thinking: Option<ThinkingLevel>,
    session: Option<Arc<SessionPlugin>>,
    /// Waiting for the agent, in the order it arrived. A queue and not one slot
    /// because a plugin can send while the user types, and neither should lose.
    queued: VecDeque<String>,
    /// Every command the runtime has started, for `/ps`.
    processes: Arc<exec::Registry>,
    /// The skills the model was told about, for `/skills`.
    skills: Vec<Skill>,
    /// Skill files that did not load, for the same list.
    skill_diagnostics: Vec<skills::Diagnostic>,
    handle: AgentHandle,
    quit: bool,
}

impl App {
    #[must_use]
    fn new(
        harness: &Harness,
        thinking: Option<ThinkingLevel>,
        processes: &Arc<exec::Registry>,
    ) -> Self {
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
            queued: VecDeque::new(),
            processes: Arc::clone(processes),
            skills: harness.skills.clone(),
            skill_diagnostics: harness.diagnostics.clone(),
            handle: harness.agent.handle(),
            host: None,
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
            queued: VecDeque::new(),
            processes: Arc::new(exec::Registry::new()),
            skills: Vec::new(),
            skill_diagnostics: Vec::new(),
            handle: agent.handle(),
            host: None,
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
            UiEvent::TurnStarted => {
                self.status.running = true;
                // Block indices restart with each turn's message buffer.
                self.view.clear_tool_streams();
            }
            UiEvent::Text(text) => self.view.push_text(&text),
            UiEvent::Thinking(text) => self.view.push_thinking(&text),
            UiEvent::ToolStreamStart { block, name } => {
                self.view.begin_tool_stream(block, &name);
            }
            UiEvent::ToolStreamDelta { block, bytes } => {
                self.view.push_tool_stream(block, bytes);
            }
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
                // Runs after every call and result for the turn, so anything
                // still streaming is a call that never arrived.
                self.view.settle_tool_streams();
                self.status.last = Some(usage);
                self.status.total += usage;
                if let Some(error) = error {
                    self.view.push_notice(format!("error: {error}"));
                }
            }
            UiEvent::Notice(text) => self.view.push_notice(text),
            // A plugin's prompt takes the path a typed line takes, minus the
            // command set: the agent is not in hand here, and a plugin has no
            // business running `/quit`.
            UiEvent::Prompt(text) => {
                self.view.push_user(text.clone());
                self.enqueue(text);
            }
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
            UiEvent::Mouse(mouse) => {
                if self.modal.is_none() {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            self.view.scroll = self.view.scroll.saturating_add(MOUSE_SCROLL_LINES);
                        }
                        MouseEventKind::ScrollDown => {
                            self.view.scroll = self.view.scroll.saturating_sub(MOUSE_SCROLL_LINES);
                        }
                        _ => {}
                    }
                }
            }
            UiEvent::Key(_) | UiEvent::Paste(_) | UiEvent::Resize => {}
        }
        true
    }

    /// The next prompt waiting to be sent, if any.
    #[must_use]
    pub fn queued(&self) -> Option<&str> {
        self.queued.front().map(String::as_str)
    }

    /// Put a prompt at the back of the queue.
    fn enqueue(&mut self, prompt: String) {
        self.queued.push_back(prompt);
        self.status.queued = self.status.running;
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
            Modal::Processes { .. } => match key.code {
                KeyCode::Up => modal.move_selection(-1),
                KeyCode::Down => modal.move_selection(1),
                KeyCode::Char('k') => {
                    if let Some(process) = modal.selected_process() {
                        // Asking is all this does; the command's own task is
                        // watching for the answer and does the stopping.
                        self.processes.kill(process.id);
                    }
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
        let Some((name, rest)) = split_command(line) else {
            return Some(line.to_owned());
        };

        if self.command_solo(name, rest) {
            return None;
        }

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
            "plugins" => self.view.push_notice(self.plugin_summary()),
            "skills" => self.view.push_notice(self.skills_summary()),
            // Built-ins win, so a plugin can never take `/quit` away.
            other => self.plugin_command(other, rest),
        }

        None
    }

    /// The commands that need nothing but the UI. `true` when the name was one.
    ///
    /// Kept apart from the rest because these are the only ones that still work
    /// while a run holds the agent — which is exactly when a user wants to ask
    /// what is running.
    fn command_solo(&mut self, name: &str, _rest: &str) -> bool {
        match name {
            "ps" => {
                self.modal = Some(Modal::Processes {
                    registry: Arc::clone(&self.processes),
                    selected: 0,
                });
                true
            }
            _ => false,
        }
    }

    /// Run a plugin's command, or report that nothing owns the name.
    ///
    /// A command reports by returning notices and steers by calling `prompt`,
    /// which arrives as its own event, so there is nothing to hand back here.
    fn plugin_command(&mut self, name: &str, args: &str) {
        let Some(host) = self.host.clone() else {
            self.view
                .push_notice(format!("unknown command `/{name}` — try /help"));
            return;
        };
        let Some(actions) = host.run_command(name, args) else {
            self.view
                .push_notice(format!("unknown command `/{name}` — try /help"));
            return;
        };

        for action in actions {
            match action {
                PluginAction::Notice(text) => self.view.push_notice(text),
            }
        }
    }

    /// What `/plugins` prints.
    fn plugin_summary(&self) -> String {
        let Some(host) = &self.host else {
            return "no plugins are loaded".to_owned();
        };
        if host.is_empty() && host.diagnostics().is_empty() {
            return "no plugins are loaded".to_owned();
        }

        let mut lines = Vec::new();
        for plugin in host.plugins() {
            let mut parts = Vec::new();
            if !plugin.hooks().is_empty() {
                parts.push(plugin.hooks().join(", "));
            }
            let tools = plugin.tools().len();
            if tools > 0 {
                parts.push(format!("{tools} tool(s)"));
            }
            let commands = plugin.commands().len();
            if commands > 0 {
                parts.push(format!("{commands} command(s)"));
            }
            lines.push(format!("  {:<16} {}", plugin.name(), parts.join(" · ")));
        }

        for command in host.commands() {
            lines.push(format!(
                "  /{:<15} {} [{}]",
                command.invocation, command.description, command.plugin
            ));
        }

        for problem in host.diagnostics() {
            lines.push(format!("  ! {problem}"));
        }

        format!("── plugins ──\n{}", lines.join("\n"))
    }

    /// What `/skills` prints.
    fn skills_summary(&self) -> String {
        if self.skills.is_empty() && self.skill_diagnostics.is_empty() {
            return "no skills are loaded".to_owned();
        }

        let mut lines = Vec::new();
        for skill in &self.skills {
            let origin = if skill.project { "project" } else { "global" };
            lines.push(format!(
                "  {:<16} {} [{origin}]",
                skill.name,
                one_line(&skill.description, SKILL_DESCRIPTION)
            ));
        }

        for problem in &self.skill_diagnostics {
            lines.push(format!(
                "  ! {}: {}",
                problem.path.display(),
                problem.message
            ));
        }

        format!("── skills ──\n{}", lines.join("\n"))
    }
}

/// The widest description a `/skills` line carries. A skill may describe itself
/// in up to a kilobyte, which is worth having in the prompt and not on screen.
const SKILL_DESCRIPTION: usize = 60;

const HELP: &str = "\
── commands ──────────────────────────────────────
  /model  [name]  switch model, or open the picker
  /think  <level>  off | minimal | low | medium | high | xhigh | max
  /clear  /new     start a fresh conversation
  /tools           list the registered tools
  /ps              what the runtime is running, and what it just ran
  /session         where this session is being written
  /plugins         list the loaded plugins and their commands
  /skills          list the skills the model can open
  /help            this list
  /quit            exit

── keys ──────────────────────────────────────────
  Esc         cancels a run
  Ctrl-C      quits
  Ctrl-P      cycles model
  Ctrl-T      shows reasoning
  PageUp/Dn   scroll transcript
  Mouse wheel scroll transcript";

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

    // One record of what is running, shared by the tools, the scripts and `/ps`.
    let processes = Arc::clone(&options.processes);

    // Scripts print through the app loop, because the UI owns the screen.
    let plugin_files = std::mem::take(&mut options.plugin_files);
    let (host, plugin_problems) = scripts::load(
        &workspace,
        &plugin_files,
        Arc::new(UiSink::new(events.clone())),
        &processes,
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
    let mut app = App::new(&harness, thinking, &processes);
    app.session = Some(session);
    app.view.watch(host.clone());
    app.host = Some(host.clone());

    if resumed.is_none() {
        app.view.push_logo();
    }

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

    let (mut terminal, kitty) = setup()?;
    spawn_input_thread(events.clone());
    let result = drive(&mut terminal, &mut app, harness.agent, receiver).await;
    restore(&mut terminal, kitty)?;

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

    // Armed only for a session that has a plugin waiting on it, so nothing else
    // pays for a timer it does not use. An `interval` and not a fresh `sleep`
    // per pass: a sleep built inside `select!` restarts whenever any other
    // branch wins, which a busy loop would starve for ever.
    let ticked = app.host.clone().filter(|host| host.any_defines("on_tick"));
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
                    UiEvent::Paste(text) => {
                        dirty = true;
                        // A modal is answered with single keys; a paste into
                        // one would mean nothing.
                        if app.modal.is_none() {
                            app.input.paste(&text);
                        }
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
            // The process list counts elapsed time, which has to keep moving
            // even with an idle agent and nothing else arriving.
            () = tokio::time::sleep(PS_REFRESH),
                if matches!(app.modal, Some(Modal::Processes { .. })) => dirty = true,
            // The plugins' tick. Dispatched off the loop, because a hook that
            // reaches for `exec` or `http` blocks on the plugin worker and the
            // loop that draws the screen must never wait on that.
            _ = ticker.tick(), if ticked.is_some() => {
                let host = ticked.clone().expect("only polled while some");
                tokio::task::spawn_blocking(move || host.tick());
            }
        }

        // Anything typed while the agent was busy goes now that it is free.
        if start_pending(&mut idle, app, &mut running) {
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

/// Start the next queued prompt when there is both an idle agent and no run in
/// flight. The explicit `running` guard is what keeps the queue serial: taking
/// the idle agent while a run was still finishing would leave two runs alive.
fn start_pending(idle: &mut Option<Agent>, app: &mut App, running: &mut Option<Running>) -> bool {
    if running.is_some() || idle.is_none() || app.queued.is_empty() {
        return false;
    }

    let agent = idle.take().expect("just checked");
    let prompt = app.queued.pop_front().expect("just checked");

    app.status.queued = !app.queued.is_empty();
    app.status.running = true;
    *running = Some(start(agent, prompt));
    true
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
                    // Mid-run the agent is away with the run, so only the
                    // commands that do not need it can run; the rest of the
                    // line queues as a prompt, as it always did.
                    None => match split_command(&line) {
                        Some((name, rest)) if app.command_solo(name, rest) => None,
                        _ => Some(line),
                    },
                }
            }) else {
                return;
            };

            app.view.push_user(prompt.clone());
            app.enqueue(prompt);
        }
    }
}

/// Split `/name rest` into its two halves. `None` when the line is a prompt.
fn split_command(line: &str) -> Option<(&str, &str)> {
    let line = line.strip_prefix('/')?;
    let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
    Some((name, rest.trim()))
}

/// The input box grows with content up to this many rows, then scrolls.
const MAX_INPUT_ROWS: u16 = 4;

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let content_height = (app.input.line_count() as u16).clamp(1, MAX_INPUT_ROWS);
    // +2 for the border's top and bottom rows.
    let input_height = content_height + 2;

    let [transcript, input_row, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_height),
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

    app.input.set_prompt(app.status.running);
    frame.render_widget(app.input.textarea(), input_row);
    app.input.sync_scroll(content_height as usize);

    if app.input.line_count() > content_height as usize {
        let mut state =
            ScrollbarState::new(app.input.line_count()).position(app.input.scroll_top());
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_style(Style::default().fg(Color::DarkGray)),
            // Trim the border's top/bottom rows so the thumb only ever
            // covers the content rows it actually represents.
            input_row.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }

    frame.render_widget(Paragraph::new(app.status.line()), status);

    // The textarea draws its own cursor cell during render; there is no
    // manual `set_cursor_position` to do here.
    if let Some(modal) = &app.modal {
        modal.render(frame, frame.area());
    }
}

/// Sets up the terminal, and reports whether the keyboard-enhancement
/// protocol was enabled — needed so `restore` knows whether to pop it, and
/// so Shift+Enter can be told apart from plain Enter in the input box. On
/// terminals that don't support it, Shift+Enter is indistinguishable from
/// plain Enter, so it just submits — a graceful degradation, not a bug.
fn setup() -> std::io::Result<(Screen, bool)> {
    // Restore the terminal even when something panics, so a crash does not
    // leave the shell in raw mode.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(DisableMouseCapture);
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        previous(info);
    }));

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    // Pasted text then arrives whole, instead of as the keys it looks like —
    // one Enter per line, each of which would submit. The legacy Windows
    // console has no such mode and says so; a session there is no worse off
    // than before, so the refusal is not worth failing the start-up over.
    let _ = stdout.execute(EnableBracketedPaste);
    // Mouse reporting is also best-effort: a terminal that cannot report the
    // wheel still works, it just keeps keyboard-only scrolling.
    let _ = stdout.execute(EnableMouseCapture);

    let kitty = supports_keyboard_enhancement().unwrap_or(false);
    if kitty {
        stdout.execute(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ))?;
    }

    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok((terminal, kitty))
}

fn restore(terminal: &mut Screen, kitty: bool) -> std::io::Result<()> {
    if kitty {
        terminal
            .backend_mut()
            .execute(PopKeyboardEnhancementFlags)?;
    }
    let _ = terminal.backend_mut().execute(DisableMouseCapture);
    let _ = terminal.backend_mut().execute(DisableBracketedPaste);
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
    use ratatui::crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};

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

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[tokio::test]
    async fn submitting_a_message_actually_starts_a_run() {
        let agent = agent_with(vec![Turn::text("hello back")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        type_line(&mut app, &mut idle, "hello");
        assert_eq!(app.queued(), Some("hello"), "the line should be queued");

        let mut running = None;
        assert!(
            start_pending(&mut idle, &mut app, &mut running),
            "a run should start"
        );
        assert!(idle.is_none(), "the agent moved into the run");

        let (agent, outcome) = running
            .expect("just started")
            .await
            .expect("the run should not panic");
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

        let mut running = None;
        for _ in 0..5 {
            assert!(!start_pending(&mut idle, &mut app, &mut running));
            assert!(idle.is_some(), "the agent must survive an idle tick");
        }

        type_line(&mut app, &mut idle, "still there?");
        assert!(
            start_pending(&mut idle, &mut app, &mut running),
            "a run should still start"
        );
        let (agent, outcome) = running.expect("just started").await.expect("no panic");
        assert_eq!(outcome.turns, 1);
        assert_eq!(agent.transcript().len(), 3);
    }

    #[tokio::test]
    async fn a_message_typed_mid_run_is_sent_once_the_agent_is_free() {
        let agent = agent_with(vec![Turn::text("first"), Turn::text("second")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        type_line(&mut app, &mut idle, "one");
        let mut running = None;
        assert!(
            start_pending(&mut idle, &mut app, &mut running),
            "first run"
        );

        // Typed while the agent is away: no agent to hand it to yet.
        app.status.running = true;
        type_line(&mut app, &mut idle, "two");
        assert_eq!(app.queued(), Some("two"));
        assert!(app.status.queued, "the status line should say so");
        assert!(!start_pending(&mut idle, &mut app, &mut running));

        let (agent, _) = running.expect("first run").await.expect("no panic");
        idle = Some(agent);

        let mut running = None;
        assert!(
            start_pending(&mut idle, &mut app, &mut running),
            "the queued line goes now"
        );
        assert!(!app.status.queued);
        let (agent, _) = running.expect("just started").await.expect("no panic");

        // system, user, assistant, user, assistant
        assert_eq!(agent.transcript().len(), 5);
    }

    #[tokio::test]
    async fn prompts_from_a_plugin_queue_in_order_behind_a_typed_line() {
        let agent = agent_with(vec![
            Turn::text("first"),
            Turn::text("second"),
            Turn::text("third"),
        ]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        type_line(&mut app, &mut idle, "typed");
        app.status.running = true;
        app.apply(UiEvent::Prompt("from a plugin".to_owned()));
        app.apply(UiEvent::Prompt("and another".to_owned()));

        // Both are in the pane already, as a typed line would be.
        let users: Vec<String> = app
            .view
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                crate::tui::view::Entry::User(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec!["typed", "from a plugin", "and another"]);

        app.status.running = false;
        for expected in ["typed", "from a plugin", "and another"] {
            assert_eq!(app.queued(), Some(expected));
            let mut running = None;
            assert!(
                start_pending(&mut idle, &mut app, &mut running),
                "a run should start"
            );
            let (agent, _) = running.expect("just started").await.expect("no panic");
            idle = Some(agent);
        }
        assert_eq!(app.queued(), None, "the queue is drained in order");
    }

    /// The whole streaming path, from protocol events to the transcript: a
    /// tool-call block must produce a card while it streams, and the announced
    /// call must land in that same card rather than a second one.
    #[tokio::test]
    async fn a_streamed_tool_call_shows_up_before_it_is_announced() {
        use crate::tui::view::{Entry, ToolState};

        let (backend, _script) = scripted(vec![
            Turn::call("c1", "bash", r#"{"command":"cargo test"}"#),
            Turn::text("all green"),
        ]);
        let (events, mut receiver) = unbounded_channel();
        let mut agent = Agent::builder()
            .model(deepseek::flash())
            .stream_fn(backend)
            .plugin(UiPlugin::new(events))
            .tool(aphid_agent::tool_fn(
                "bash",
                "Run a command.",
                serde_json::json!({ "type": "object" }),
                |_args: serde_json::Value, _cx: aphid_agent::ToolCx| async move {
                    aphid_agent::ToolOutcome::text("ok")
                },
            ))
            .build();
        agent.prompt("run the tests").await;

        let mut app = app_for(&agent);
        let mut streaming_seen = false;
        while let Ok(event) = receiver.try_recv() {
            app.apply(event);
            // Between the block opening and the call being announced, the card
            // is on screen with a byte count.
            if let Some(Entry::Tool {
                state: ToolState::Streaming,
                name,
                streamed,
                ..
            }) = app.view.entries().last()
            {
                assert_eq!(name, "bash");
                streaming_seen |= *streamed > 0;
            }
        }

        assert!(streaming_seen, "the card should have counted bytes");
        let tools: Vec<&Entry> = app
            .view
            .entries()
            .iter()
            .filter(|entry| matches!(entry, Entry::Tool { .. }))
            .collect();
        assert_eq!(tools.len(), 1, "the placeholder became the call");
        assert!(
            matches!(
                tools[0],
                Entry::Tool {
                    state: ToolState::Done,
                    ..
                }
            ),
            "{:?}",
            tools[0]
        );
    }

    #[test]
    fn mouse_wheel_scrolls_the_transcript() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);

        app.apply(UiEvent::Mouse(mouse(MouseEventKind::ScrollUp)));
        assert_eq!(app.view.scroll, MOUSE_SCROLL_LINES);

        app.apply(UiEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        assert_eq!(app.view.scroll, 0);
    }

    #[test]
    fn mouse_wheel_down_saturates_at_the_newest_message() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);

        app.apply(UiEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        app.apply(UiEvent::Mouse(mouse(MouseEventKind::ScrollDown)));
        assert_eq!(app.view.scroll, 0);
    }

    #[test]
    fn non_wheel_mouse_events_do_not_scroll() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);

        app.apply(UiEvent::Mouse(mouse(MouseEventKind::Moved)));
        assert_eq!(app.view.scroll, 0);
    }

    #[test]
    fn mouse_wheel_does_not_change_the_input() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        app.input.paste("draft");

        app.apply(UiEvent::Mouse(mouse(MouseEventKind::ScrollUp)));
        app.apply(UiEvent::Mouse(mouse(MouseEventKind::ScrollDown)));

        assert_eq!(app.input.text(), "draft");
    }

    #[tokio::test]
    async fn a_slash_command_is_handled_rather_than_sent() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        type_line(&mut app, &mut idle, "/tools");

        assert!(app.queued().is_none(), "a command is not a prompt");
        let mut running = None;
        assert!(!start_pending(&mut idle, &mut app, &mut running));
        assert!(idle.is_some());
    }

    fn notice_lines(app: &App) -> Vec<String> {
        use crate::tui::view::Entry;
        app.view
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                Entry::Notice(text) => Some(text.clone()),
                _ => None,
            })
            .flat_map(|text| {
                text.lines()
                    .map(std::borrow::ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn skill(name: &str, description: &str, project: bool) -> Skill {
        Skill {
            name: name.to_owned(),
            description: description.to_owned(),
            path: PathBuf::from(format!("/w/.aphid/skills/{name}.md")),
            project,
        }
    }

    #[tokio::test]
    async fn skills_lists_what_loaded_and_what_did_not() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);
        app.skills = vec![
            skill("release", "How to cut a release.", true),
            skill("review", &"a very wordy description ".repeat(20), false),
        ];
        app.skill_diagnostics = vec![skills::Diagnostic {
            path: PathBuf::from("/w/.aphid/skills/broken.md"),
            message: "no `description` in the frontmatter".to_owned(),
        }];

        type_line(&mut app, &mut idle, "/skills");

        let lines = notice_lines(&app);
        assert!(
            lines.iter().any(|line| line.contains("release")
                && line.contains("How to cut a release.")
                && line.ends_with("[project]")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains('…') && line.ends_with("[global]")),
            "a long description is cut to one line: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("  !") && line.contains("broken.md")),
            "{lines:?}"
        );
        assert!(app.queued().is_none(), "a command is not a prompt");
    }

    #[tokio::test]
    async fn skills_says_so_when_there_are_none() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        type_line(&mut app, &mut idle, "/skills");

        assert!(notice_lines(&app).contains(&"no skills are loaded".to_owned()));
    }

    #[tokio::test]
    async fn ps_opens_the_process_list() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        type_line(&mut app, &mut idle, "/ps");

        assert!(matches!(app.modal, Some(Modal::Processes { .. })));
        assert!(app.queued().is_none(), "a command is not a prompt");
    }

    /// The one time a user most wants to know what is running is while
    /// something is running, which is exactly when the agent is away.
    #[tokio::test]
    async fn ps_works_while_a_run_holds_the_agent() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let mut away: Option<Agent> = None;
        app.status.running = true;

        type_line(&mut app, &mut away, "/ps");

        assert!(matches!(app.modal, Some(Modal::Processes { .. })));
        assert_eq!(app.queued(), None, "it must not queue as a prompt");
    }

    #[tokio::test]
    async fn a_command_that_needs_the_agent_still_queues_mid_run() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let mut away: Option<Agent> = None;
        app.status.running = true;

        type_line(&mut app, &mut away, "/tools");

        assert!(app.modal.is_none());
        assert_eq!(app.queued(), Some("/tools"));
    }

    #[tokio::test]
    async fn k_stops_the_selected_process() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let mut idle = Some(agent);

        let processes = Arc::clone(&app.processes);
        let running = tokio::spawn({
            let processes = Arc::clone(&processes);
            async move {
                exec::run(
                    &processes,
                    exec::Spec::new("bash", "sleep 30"),
                    None,
                    Arc::new(|_, _| {}),
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;

        type_line(&mut app, &mut idle, "/ps");
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            idle.as_mut(),
        );

        let status = running.await.expect("the sleep");
        assert_eq!(status, exec::Status::Killed);
    }
}

#[cfg(test)]
mod plugin_tests {
    use std::sync::Arc;

    use aphid_agent::{Agent, exec};
    use aphid_core::providers::deepseek;
    use aphid_plugin::{Capabilities, PluginHost, Silent, explicit};
    use tokio::sync::mpsc::unbounded_channel;

    use super::{App, Status};
    use crate::tui::event::{UiEvent, UiSink};
    use crate::tui::view::Entry;

    /// A workspace with one plugin, removed on drop.
    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new(source: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);

            let root = std::env::temp_dir().join(format!(
                "aphid-app-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let dir = root.join(".aphid").join("plugins");
            std::fs::create_dir_all(&dir).expect("create");
            std::fs::write(dir.join("kit.rhai"), source).expect("write");
            Self(root)
        }

        fn host(&self) -> Arc<PluginHost> {
            self.host_with(Arc::new(Silent))
        }

        fn host_with(&self, sink: Arc<dyn aphid_plugin::Sink>) -> Arc<PluginHost> {
            let file = explicit(&self.0.join(".aphid").join("plugins").join("kit.rhai"))
                .expect("readable");
            let processes = Arc::new(exec::Registry::new());
            let (host, problems) =
                PluginHost::load(&[file], &Capabilities::full(&self.0), sink, &processes);
            assert!(problems.is_empty(), "{problems:?}");
            Arc::new(host)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn app_with(host: Arc<PluginHost>) -> (App, Agent) {
        let (backend, _script) = aphid_agent::testing::scripted([]);
        let agent = Agent::builder()
            .model(deepseek::flash())
            .stream_fn(backend)
            .build();
        let mut app = App::new_for_test(&agent);
        app.status = Status::from_model(agent.model());
        app.host = Some(host);
        (app, agent)
    }

    fn notices(app: &App) -> Vec<String> {
        app.view
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                Entry::Notice(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_plugin_command_prints_and_prompts() {
        let fixture = Fixture::new(
            r#"
            register_command(#{
                name: "greet",
                description: "Say hello.",
                run: |args| {
                    prompt("Say hello to " + args);
                    notice("greeting " + args)
                }
            });
            "#,
        );
        let (events, mut receiver) = unbounded_channel::<UiEvent>();
        let host = fixture.host_with(Arc::new(UiSink::new(events)));
        let (mut app, mut agent) = app_with(host);

        let typed = app.command(&mut agent, "/greet Ana");

        assert_eq!(typed, None, "a command is not itself a prompt");
        assert_eq!(notices(&app), vec!["greeting Ana"]);

        // The prompt took the long way round, as an event the loop applies.
        let event = receiver.try_recv().expect("the prompt was sent");
        app.apply(event);
        assert_eq!(app.queued(), Some("Say hello to Ana"));
    }

    #[test]
    fn a_built_in_command_wins_over_a_plugin_of_the_same_name() {
        let fixture = Fixture::new(
            r#"register_command(#{ name: "help", run: |args| { prompt("hijacked") } });"#,
        );
        let (mut app, mut agent) = app_with(fixture.host());

        let prompt = app.command(&mut agent, "/help");

        assert_eq!(prompt, None, "the built-in ran, not the plugin");
        assert!(
            notices(&app)
                .first()
                .is_some_and(|text| text.contains("commands")),
            "{:?}",
            notices(&app)
        );
    }

    #[test]
    fn an_unknown_command_is_still_reported() {
        let fixture = Fixture::new(r#"fn on_run_start(cx) {}"#);
        let (mut app, mut agent) = app_with(fixture.host());

        assert_eq!(app.command(&mut agent, "/nope"), None);
        assert!(
            notices(&app)[0].contains("unknown command `/nope`"),
            "{:?}",
            notices(&app)
        );
    }

    #[test]
    fn plugins_lists_what_loaded() {
        let fixture = Fixture::new(
            r#"
            fn on_run_start(cx) {}
            register_command(#{ name: "greet", description: "Say hello.", run: |a| { "hi" } });
            "#,
        );
        let (mut app, mut agent) = app_with(fixture.host());

        app.command(&mut agent, "/plugins");

        let summary = notices(&app).remove(0);
        assert!(summary.contains("kit"), "{summary}");
        assert!(summary.contains("on_run_start"), "{summary}");
        assert!(summary.contains("/greet"), "{summary}");
        assert!(summary.contains("Say hello."), "{summary}");
    }
}
