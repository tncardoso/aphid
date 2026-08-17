//! The app: state, the command set, and the loop that drives both.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aphid_agent::{Agent, AgentHandle, exec};
use aphid_core::{Model, ThinkingLevel, Transcript};
use aphid_plugin::{
    Action as PluginAction, Job as PluginJob, Placement, PluginHub, Report as PluginReport,
    ScriptBackend, SessionInfo, Side, SurfaceAction, SurfaceEvent,
};
use compact_str::CompactString;
use ratatui::Frame;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::harness::{self, Harness, HarnessOptions};
use crate::model::{Catalog, ResolveError, clamp_thinking};
use crate::plugins::permissions::{Decision, Permissions};
use crate::plugins::scripts;
use crate::session::{self, SessionPlugin, sessions_dir};
use crate::skills::{self, Skill};
use crate::tools::Workspace;
use crate::tui::effect::Effect;
use crate::tui::event::{UiConfirmer, UiPlugin, UiSink, spawn_input_thread};
use crate::tui::input::{Action, Input};
use crate::tui::modal::{Confirm, Modal};
use crate::tui::msg::Msg;
use crate::tui::render;
use crate::tui::runtime::{
    self, Answers, Cmd, Draw, Effects, Hub, Program, Subs, Timer, restore, setup,
};
use crate::tui::scrollback::{Scrollback, one_line};
use crate::tui::status::Status;
use crate::tui::surface::{Panes, SurfaceLayer};

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

/// How many transcript lines PageUp and PageDown move.
const PAGE_LINES: usize = 10;

/// Everything the UI holds.
pub struct App {
    pub scrollback: Scrollback,
    pub input: Input,
    pub status: Status,
    pub modal: Option<Modal>,
    /// The plugin surfaces, for focus, events and rendering.
    pub surfaces: SurfaceLayer,
    /// The loaded plugins, for the commands they registered and `/plugins`.
    host: Option<Arc<aphid_plugin::PluginHost>>,
    catalog: Catalog,
    /// The model the agent is pointed at. Held because clamping a thinking
    /// level is a question about the model, and the update must answer it
    /// without reaching for the agent.
    current: Model,
    thinking: Option<ThinkingLevel>,
    /// What the tools are called, for `/tools`. The set does not change during
    /// a session, so a copy is as good as the agent's own.
    tools: Vec<String>,
    /// What `/session` says. Settled when the session opens.
    session_label: String,
    /// The slash commands the plugins registered, so an unknown one can be
    /// told from one that is simply somebody else's.
    plugin_commands: Vec<String>,
    /// Whether any plugin asked to hear about notices.
    plugins_watch_notices: bool,
    /// Whether any plugin wants the background tick.
    plugins_tick: bool,
    /// Whether any plugin has a panel at all.
    plugins_draw: bool,
    session: Option<Arc<SessionPlugin>>,
    /// Waiting for the agent, in the order it arrived. A queue and not one slot
    /// because a plugin can send while the user types, and neither should lose.
    queued: VecDeque<String>,
    /// Every command the runtime has started, for `/ps`.
    processes: Arc<exec::Registry>,
    /// Where `!` commands run.
    workspace: Workspace,
    /// The skills the model was told about, for `/skills`.
    skills: Vec<Skill>,
    /// Skill files that did not load, for the same list.
    skill_diagnostics: Vec<skills::Diagnostic>,
    /// The reply channels for questions on screen. Runtime state, held here
    /// until the executor exists to own it.
    answers: Answers<Decision>,
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
            scrollback: Scrollback::default(),
            input: Input::default(),
            status,
            modal: None,
            surfaces: SurfaceLayer::default(),
            catalog: Catalog::new(),
            current: harness.agent.model().clone(),
            thinking,
            tools: harness
                .agent
                .tools()
                .names()
                .map(ToOwned::to_owned)
                .collect(),
            session_label: "not being saved".to_owned(),
            plugin_commands: Vec::new(),
            plugins_watch_notices: false,
            plugins_tick: false,
            plugins_draw: false,
            host: None,
            session: None,
            queued: VecDeque::new(),
            processes: Arc::clone(processes),
            workspace: harness.workspace.clone(),
            skills: harness.skills.clone(),
            skill_diagnostics: harness.diagnostics.clone(),
            answers: Answers::default(),
            quit: false,
        }
    }

    #[cfg(test)]
    fn new_for_test(agent: &Agent) -> Self {
        Self {
            scrollback: Scrollback::default(),
            input: Input::default(),
            status: Status::from_model(agent.model()),
            modal: None,
            surfaces: SurfaceLayer::default(),
            catalog: Catalog::new(),
            current: agent.model().clone(),
            thinking: None,
            tools: agent.tools().names().map(ToOwned::to_owned).collect(),
            session_label: "not being saved".to_owned(),
            plugin_commands: Vec::new(),
            plugins_watch_notices: false,
            plugins_tick: false,
            plugins_draw: false,
            host: None,
            session: None,
            queued: VecDeque::new(),
            processes: Arc::new(exec::Registry::new()),
            workspace: Workspace::new(std::env::temp_dir()),
            skills: Vec::new(),
            skill_diagnostics: Vec::new(),
            answers: Answers::default(),
            quit: false,
        }
    }

    /// Replay a resumed transcript into the scrollback, so the pane shows the
    /// conversation you are continuing rather than starting blank.
    fn replay(&mut self, transcript: &Transcript) {
        use aphid_core::{ContentRef, Role};

        for message in transcript.iter() {
            match message.role() {
                Role::System => {}
                Role::User => {
                    let text: String = message.content().filter_map(|c| c.text()).collect();
                    if !text.is_empty() {
                        self.scrollback.push_user(text);
                    }
                }
                Role::Assistant => {
                    for content in message.content() {
                        match content {
                            ContentRef::Text(text) => self.scrollback.push_text(text.text()),
                            ContentRef::Thinking(thinking) => {
                                self.scrollback.push_thinking(thinking.text());
                            }
                            ContentRef::ToolCall(call) => {
                                self.scrollback.push_tool_call(
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
                    self.scrollback.finish_tool(
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
    /// Apply one message.
    ///
    /// The only place the model changes, and it changes nothing else: no IO,
    /// no script, no task, no terminal. What it wants done comes back as a
    /// [`Cmd`], which the executor performs and reports on with more messages.
    pub fn update(&mut self, msg: Msg) -> Cmd<Effect> {
        let cmd = self.apply(msg);
        // The input box's border and its scroll window both follow from state
        // the model holds. Settling them here is what lets the drawing take
        // the model by shared reference.
        self.input.set_prompt(self.status.running);
        self.input
            .sync_scroll((self.input.line_count()).clamp(1, MAX_INPUT_ROWS as usize));
        cmd
    }

    fn apply(&mut self, msg: Msg) -> Cmd<Effect> {
        match msg {
            Msg::TurnStarted => {
                self.status.running = true;
                // Block indices restart with each turn's message buffer.
                self.scrollback.clear_tool_streams();
                Cmd::none()
            }
            Msg::Text(text) => {
                self.status.download.note(Instant::now(), text.len() as u64);
                self.scrollback.push_text(&text);
                Cmd::none()
            }
            Msg::Thinking(text) => {
                self.status.download.note(Instant::now(), text.len() as u64);
                self.scrollback.push_thinking(&text);
                Cmd::none()
            }
            Msg::ToolStreamStart { block, name } => {
                self.scrollback.begin_tool_stream(block, &name);
                Cmd::none()
            }
            Msg::ToolStreamDelta { block, bytes } => {
                self.status.download.note(Instant::now(), bytes as u64);
                self.scrollback.push_tool_stream(block, bytes);
                Cmd::none()
            }
            Msg::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.scrollback.push_tool_call(&id, &name, &arguments);
                Cmd::none()
            }
            Msg::ToolProgress { id, chunk } => {
                self.scrollback.push_tool_progress(&id, &chunk);
                Cmd::none()
            }
            Msg::ToolResult {
                id,
                text,
                is_error,
                details,
                ..
            } => {
                self.scrollback.finish_tool(&id, &text, is_error, details);
                Cmd::none()
            }
            Msg::TurnEnded { usage, error, .. } => {
                // The stream is over; a stale reading must not sit on an idle
                // line while `working…` is gone.
                self.status.download.clear();
                // Runs after every call and result for the turn, so anything
                // still streaming is a call that never arrived.
                self.scrollback.settle_tool_streams();
                self.status.last = Some(usage);
                self.status.total += usage;
                match error {
                    Some(error) => self.notice(format!("error: {error}")),
                    None => Cmd::none(),
                }
            }
            Msg::RunEnded { .. } => {
                self.status.download.clear();
                self.status.running = false;
                self.start_queued()
            }
            Msg::RunFailed(reason) => {
                self.status.download.clear();
                self.status.running = false;
                let mut cmd = self.notice(format!("the run failed: {reason}"));
                cmd.extend(self.start_queued());
                cmd
            }
            Msg::Notice(text) => {
                // Straight into the pane: this came from a plugin, so telling
                // the plugins about it would be an echo.
                self.scrollback.push_notice(text);
                Cmd::none()
            }
            // A plugin's prompt takes the path a typed line takes, minus the
            // command set: a plugin has no business running `/quit`.
            Msg::Prompt(text) => {
                self.scrollback.push_user(text.clone());
                self.enqueue(text)
            }
            Msg::Confirm {
                id,
                tool,
                summary,
                risk,
            } => {
                // A question arriving over another modal replaces it: the agent
                // is blocked on this one, and a picker is not.
                self.modal = Some(Modal::Confirm(Confirm {
                    id,
                    tool,
                    summary,
                    risk,
                }));
                Cmd::none()
            }
            Msg::Key(key) => self.keyed(key),
            Msg::Paste(text) => {
                if self.modal.is_some() {
                    return Cmd::none();
                }
                match self.surfaces.focus() {
                    Some((plugin, name)) => Cmd::one(Effect::Surface {
                        plugin,
                        name,
                        event: SurfaceEvent::Paste { text },
                    }),
                    None => {
                        self.input.paste(&text);
                        Cmd::none()
                    }
                }
            }
            Msg::Mouse(mouse) => self.moused(mouse),
            Msg::Resize => Cmd::none(),
            Msg::Frame => {
                self.status.download.prune(Instant::now());
                Cmd::none()
            }
            Msg::Poll => Cmd::one(Effect::SnapshotProcesses),
            Msg::Processes(rows) => {
                if let Some(Modal::Processes { rows: shown, .. }) = &mut self.modal {
                    *shown = rows;
                }
                Cmd::none()
            }
            Msg::BangOutput { command, output } => {
                self.scrollback.push_shell(command, output);
                Cmd::none()
            }
            Msg::LaidOut(laid) => {
                // Idempotent on purpose: it arrives after every frame, and
                // most frames lay out exactly what the last one did.
                self.scrollback.laid_out(laid.viewport);
                self.surfaces.laid_out(laid.hits);
                Cmd::none()
            }
            Msg::Tick => Cmd::batch([Effect::PluginTick, Effect::RefreshSurfaces]),
            Msg::Panes(panes) => {
                self.surfaces.show(panes);
                Cmd::none()
            }
            Msg::SurfaceDone { plugin, actions } => {
                // Nothing to say means the surface is gone; the focus with it.
                if actions.is_empty() {
                    self.surfaces.release_focus();
                }
                let mut cmd = Cmd::none();
                for action in actions {
                    match action {
                        SurfaceAction::Consume => {}
                        SurfaceAction::ReleaseFocus => self.surfaces.release_focus(),
                        SurfaceAction::Notice(text) => {
                            cmd.extend(self.notice(format!("{plugin}: {text}")));
                        }
                        SurfaceAction::Prompt(text) => {
                            self.scrollback.push_user(text.clone());
                            cmd.extend(self.enqueue(text));
                        }
                        // A surface talking to itself. It goes back round the
                        // same way anything else reaches a surface, so there
                        // is no second path for a plugin to be reached by.
                        SurfaceAction::Send { name, payload } => {
                            if let Some((_, surface)) = self.surfaces.focus() {
                                cmd.push(Effect::Surface {
                                    plugin: plugin.clone(),
                                    name: surface,
                                    event: SurfaceEvent::Msg { name, payload },
                                });
                            }
                        }
                    }
                }
                cmd.push(Effect::RefreshSurfaces);
                cmd
            }
        }
    }

    /// The timers this model wants right now.
    #[must_use]
    pub fn wanted_subs(&self) -> Subs {
        Subs {
            frame: self.status.running.then_some(FRAME),
            poll: matches!(self.modal, Some(Modal::Processes { .. })).then_some(PS_REFRESH),
            tick: self.plugins_tick.then_some(TICK),
        }
    }

    #[must_use]
    pub fn quitting(&self) -> bool {
        self.quit
    }

    /// Show a notice, and tell the plugins what was shown.
    fn notice(&mut self, text: impl Into<String>) -> Cmd<Effect> {
        let text = text.into();
        self.scrollback.push_notice(text.clone());
        if self.plugins_watch_notices {
            Cmd::one(Effect::PluginNotice(text))
        } else {
            Cmd::none()
        }
    }

    /// The next prompt waiting to be sent, if any.
    #[must_use]
    pub fn queued(&self) -> Option<&str> {
        self.queued.front().map(String::as_str)
    }

    /// Put a prompt at the back of the queue, and start it when nothing else
    /// is running.
    fn enqueue(&mut self, prompt: String) -> Cmd<Effect> {
        self.queued.push_back(prompt);
        self.start_queued()
    }

    /// Send the next queued prompt, if the agent is free to take it.
    ///
    /// The queue is serial: one run at a time, whether the line was typed or
    /// came from a plugin while the user was typing.
    fn start_queued(&mut self) -> Cmd<Effect> {
        if self.status.running {
            self.status.queued = !self.queued.is_empty();
            return Cmd::none();
        }
        let Some(prompt) = self.queued.pop_front() else {
            self.status.queued = false;
            return Cmd::none();
        };
        self.status.queued = !self.queued.is_empty();
        self.status.running = true;
        Cmd::one(Effect::StartRun(prompt))
    }

    /// Handle a mouse event, routing it to the focused surface or the pane.
    fn moused(&mut self, mouse: MouseEvent) -> Cmd<Effect> {
        if self.modal.is_some() {
            return Cmd::none();
        }

        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                if self.surfaces.focus().is_some() {
                    let button = if up { "up" } else { "down" };
                    return self.to_surface(button, mouse.column, mouse.row, None);
                }
                if up {
                    self.scrollback.scroll_up(MOUSE_SCROLL_LINES);
                } else {
                    self.scrollback.scroll_down(MOUSE_SCROLL_LINES);
                }
                Cmd::none()
            }
            MouseEventKind::Down(_) => match self.surfaces.click(mouse.column, mouse.row) {
                Some((_surface, target)) => {
                    self.to_surface(mouse_button(mouse.kind), mouse.column, mouse.row, target)
                }
                None => Cmd::none(),
            },
            MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                let Some(focus) = self.surfaces.focus() else {
                    return Cmd::none();
                };
                let target = self
                    .surfaces
                    .hit(mouse.column, mouse.row)
                    .filter(|(key, _)| key == &focus)
                    .and_then(|(_, target)| target);
                self.to_surface(mouse_button(mouse.kind), mouse.column, mouse.row, target)
            }
            _ => Cmd::none(),
        }
    }

    /// Send one mouse event to the focused surface.
    #[allow(clippy::wrong_self_convention, reason = "it sends to, not converts")]
    fn to_surface(
        &mut self,
        button: &str,
        column: u16,
        row: u16,
        target: Option<String>,
    ) -> Cmd<Effect> {
        let Some((plugin, name)) = self.surfaces.focus() else {
            return Cmd::none();
        };
        Cmd::one(Effect::Surface {
            plugin,
            name,
            event: SurfaceEvent::Mouse {
                button: button.to_owned(),
                row,
                column,
                target,
            },
        })
    }

    /// Handle a key while a surface has focus.
    fn surface_key(&mut self, key: KeyEvent) -> Cmd<Effect> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if control => {
                self.quit = true;
                Cmd::one(Effect::Quit)
            }
            KeyCode::F(6) => {
                self.surfaces.cycle_focus();
                Cmd::none()
            }
            KeyCode::Esc => {
                self.surfaces.release_focus();
                Cmd::none()
            }
            _ => {
                let Some((plugin, name)) = self.surfaces.focus() else {
                    return Cmd::none();
                };
                Cmd::one(Effect::Surface {
                    plugin,
                    name,
                    event: surface_key_event(key),
                })
            }
        }
    }

    /// Handle a keypress that a modal is claiming.
    fn key_in_modal(&mut self, key: KeyEvent) -> Cmd<Effect> {
        let Some(modal) = &mut self.modal else {
            return Cmd::none();
        };

        match modal {
            Modal::Models { .. } => match key.code {
                KeyCode::Up => modal.move_selection(-1),
                KeyCode::Down => modal.move_selection(1),
                KeyCode::Enter => {
                    let chosen = modal.selected_model().cloned();
                    self.modal = None;
                    if let Some(model) = chosen {
                        return self.switch_model(model);
                    }
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
                        return Cmd::one(Effect::Kill(process.id));
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
                    return Cmd::one(Effect::Answer {
                        id: confirm.id,
                        decision,
                    });
                }
            }
        }
        Cmd::none()
    }

    /// Point the session at another model.
    ///
    /// Everything a reader sees happens here; the agent is told by the effect,
    /// which also fetches the credentials the new provider needs.
    fn switch_model(&mut self, model: Model) -> Cmd<Effect> {
        let (thinking, note) = clamp_thinking(&model, self.thinking);
        self.thinking = thinking;
        self.status.thinking = thinking.map(|level| level.as_str().to_owned());
        self.status.model = model.id.to_string();
        self.status.context_window = model.context_window;
        self.current = model.clone();

        let mut cmd = self.notice(format!("── switched to {} ──", model.id));
        if let Some(note) = note {
            cmd.extend(self.notice(note));
        }
        cmd.push(Effect::SetModel(Box::new(model)));
        cmd
    }

    /// Handle one keypress.
    fn keyed(&mut self, key: KeyEvent) -> Cmd<Effect> {
        if self.modal.is_some() {
            return self.key_in_modal(key);
        }

        if self.surfaces.focus().is_some() {
            return self.surface_key(key);
        }

        if key.code == KeyCode::F(6) && self.surfaces.has_focusable() {
            self.surfaces.focus_first();
            return Cmd::none();
        }

        match self.input.handle(key) {
            Action::None => Cmd::none(),
            Action::Quit => {
                self.quit = true;
                Cmd::one(Effect::Quit)
            }
            Action::Cancel => {
                if self.status.running {
                    let mut cmd = Cmd::one(Effect::Cancel);
                    cmd.extend(self.notice("── cancelled ──"));
                    cmd
                } else {
                    self.input.clear();
                    Cmd::none()
                }
            }
            Action::ScrollUp => {
                self.scrollback.scroll_up(PAGE_LINES);
                Cmd::none()
            }
            Action::ScrollDown => {
                self.scrollback.scroll_down(PAGE_LINES);
                Cmd::none()
            }
            Action::ToggleThinking => {
                self.scrollback.show_thinking = !self.scrollback.show_thinking;
                Cmd::none()
            }
            // No agent involved, so a `!` command works mid-run too, like
            // `/ps`. The registry tracks it, so `/ps` can stop it.
            Action::Bang(command) => Cmd::one(Effect::Bang(command)),
            Action::CycleModel => match self.catalog.next_after(&self.status.model.clone()) {
                Some(model) => self.switch_model(model),
                None => Cmd::none(),
            },
            Action::Submit(line) => self.submit(line),
        }
    }

    /// A finished line: a command, or a prompt for the agent.
    fn submit(&mut self, line: String) -> Cmd<Effect> {
        if let Some((name, rest)) = split_command(&line) {
            let (name, rest) = (name.to_owned(), rest.to_owned());
            return self.command(&name, &rest);
        }
        self.scrollback.push_user(line.clone());
        self.enqueue(line)
    }

    /// Run a slash command.
    fn command(&mut self, name: &str, rest: &str) -> Cmd<Effect> {
        match name {
            "quit" | "q" | "exit" => {
                self.quit = true;
                Cmd::one(Effect::Quit)
            }
            "ps" => {
                self.modal = Some(Modal::Processes {
                    rows: Vec::new(),
                    selected: 0,
                });
                Cmd::one(Effect::SnapshotProcesses)
            }
            "clear" | "new" => {
                self.scrollback.clear();
                self.status.last = None;
                let mut cmd = Cmd::one(Effect::ClearTranscript);
                cmd.extend(self.notice("── new session ──"));
                cmd
            }
            "model" if rest.is_empty() => {
                let models = self.catalog.models().to_vec();
                let selected = self.catalog.position(&self.status.model).unwrap_or(0);
                self.modal = Some(Modal::Models { models, selected });
                Cmd::none()
            }
            "model" => match self.catalog.resolve(rest) {
                Ok(model) => self.switch_model(model),
                Err(ResolveError::Unknown { candidates }) => self.notice(format!(
                    "no model `{rest}`. Available: {}",
                    candidates.join(", ")
                )),
                Err(ResolveError::Ambiguous { matches }) => {
                    self.notice(format!("`{rest}` is ambiguous: {}", matches.join(", ")))
                }
            },
            "think" => match parse_thinking(rest) {
                Ok(level) => {
                    let (level, note) = clamp_thinking(&self.current, level);
                    self.thinking = level;
                    self.status.thinking = level.map(|level| level.as_str().to_owned());
                    let said = note.unwrap_or_else(|| {
                        format!(
                            "thinking {}",
                            level.map_or("off", aphid_core::ThinkingLevel::as_str)
                        )
                    });
                    let mut cmd = Cmd::one(Effect::SetThinking(level));
                    cmd.extend(self.notice(said));
                    cmd
                }
                Err(message) => self.notice(message),
            },
            "tools" => {
                let names = self.tools.join(", ");
                self.notice(format!("tools: {names}"))
            }
            "session" => {
                let described = self.session_label.clone();
                self.notice(format!("session: {described}"))
            }
            "help" => self.notice(HELP),
            "plugins" => {
                let summary = self.plugin_summary();
                self.notice(summary)
            }
            "skills" => {
                let summary = self.skills_summary();
                self.notice(summary)
            }
            // Built-ins win, so a plugin can never take `/quit` away.
            other if self.plugin_commands.iter().any(|known| known == other) => {
                Cmd::one(Effect::PluginCommand {
                    name: other.to_owned(),
                    args: rest.to_owned(),
                })
            }
            other => self.notice(format!("unknown command `/{other}` — try /help")),
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
            let surfaces = plugin.surfaces().len();
            if surfaces > 0 {
                parts.push(format!("{surfaces} surface(s)"));
            }
            lines.push(format!("  {:<16} {}", plugin.name(), parts.join(" · ")));
        }

        for command in host.commands() {
            lines.push(format!(
                "  /{:<15} {} [{}]",
                command.invocation, command.description, command.plugin
            ));
        }

        for surface in host.surfaces() {
            let side = match surface.placement {
                Placement::Side(Side::Left) => "left",
                Placement::Side(Side::Right) => "right",
            };
            lines.push(format!(
                "  {:<17} {side} panel [{}]",
                format!("{}/{}", surface.plugin, surface.name),
                surface.plugin
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
  !cmd             run a shell command; its output goes to the transcript

── keys ──────────────────────────────────────────
  Esc         cancels a run, or returns focus from a panel
  Ctrl-C      quits
  Ctrl-P      cycles model
  Ctrl-T      shows reasoning
  F6          focus a plugin panel
  PageUp/Dn   scroll transcript
  Mouse wheel scroll transcript";

/// Map a crossterm mouse kind to the short name a surface callback sees.
fn mouse_button(kind: MouseEventKind) -> &'static str {
    match kind {
        MouseEventKind::Down(MouseButton::Left)
        | MouseEventKind::Up(MouseButton::Left)
        | MouseEventKind::Drag(MouseButton::Left) => "left",
        MouseEventKind::Down(MouseButton::Right)
        | MouseEventKind::Up(MouseButton::Right)
        | MouseEventKind::Drag(MouseButton::Right) => "right",
        MouseEventKind::Down(MouseButton::Middle)
        | MouseEventKind::Up(MouseButton::Middle)
        | MouseEventKind::Drag(MouseButton::Middle) => "middle",
        _ => "unknown",
    }
}

/// Map a crossterm key to the normalized event a surface callback sees.
fn surface_key_event(key: KeyEvent) -> SurfaceEvent {
    let mut modifiers = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers.push("control".to_owned());
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        modifiers.push("shift".to_owned());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        modifiers.push("alt".to_owned());
    }

    SurfaceEvent::Key {
        code: key_name(key.code),
        modifiers,
    }
}

fn key_name(code: KeyCode) -> String {
    match code {
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

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

    let (events, mut receiver) = runtime::channel();
    // One set of reply channels, shared by whoever asks and whoever answers.
    let answers = Answers::default();

    options
        .plugins
        .push(Arc::new(UiPlugin::new(events.clone())));
    if confirm {
        options
            .plugins
            .push(Arc::new(Permissions::new(Arc::new(UiConfirmer::new(
                events.clone(),
                answers.clone(),
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
    app.answers = answers;
    app.session_label = session.id().zip(session.path()).map_or_else(
        || "not being saved".to_owned(),
        |(id, path)| format!("{id} — {}", path.display()),
    );
    app.session = Some(session);
    app.host = Some(host.clone());
    // Asked once, at load: what the plugins registered does not change during
    // a session, and an update must be able to answer without the host.
    app.plugin_commands = host
        .commands()
        .into_iter()
        .map(|command| command.invocation)
        .collect();
    app.plugins_watch_notices = host.any_defines("on_notify");
    app.plugins_tick = host.any_defines("on_tick") || host.has_surfaces();
    app.plugins_draw = host.has_surfaces();

    if resumed.is_none() {
        app.scrollback.push_logo();
    }

    for note in &harness.notes {
        app.scrollback.push_notice(note.clone());
    }
    for diagnostic in &harness.diagnostics {
        app.scrollback.push_notice(format!(
            "skipped skill {}: {}",
            diagnostic.path.display(),
            diagnostic.message
        ));
    }
    for problem in &plugin_problems {
        app.scrollback.push_notice(problem.to_string());
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
        app.scrollback
            .push_notice(format!("── resumed {} messages ──", keep.len()));
    }
    app.scrollback.push_notice(format!(
        "aphid · {} · {} — /help for commands",
        harness.agent.model().id,
        workspace.root().display()
    ));

    let mut executor = Executor::new(harness.agent, &app, events.clone());
    executor.plugins = Some(spawn_plugin_hub(host.clone(), events.clone()));

    let (mut terminal, kitty) = setup()?;
    spawn_input_thread(&events);
    let result = runtime::run(
        &mut app,
        &mut executor,
        &mut terminal,
        &events,
        &mut receiver,
    )
    .await;
    restore(&mut terminal, kitty)?;
    // Before the session hooks, so they are not racing the script thread on
    // the way out. Its queue is drained first.
    if let Some(plugins) = executor.plugins.take() {
        plugins.stop();
    }

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

/// Everything the model must not own.
///
/// The agent above all: it is parked here between runs and moved into the
/// run's own task while one is in flight, which is why no update can reach for
/// it and why every use of it is an effect.
struct Executor {
    /// The agent, parked between runs.
    ///
    /// Behind a lock because the run's own task is what puts it back: nothing
    /// else can, and `perform` cannot wait for it.
    idle: Arc<Mutex<Option<Agent>>>,
    /// The task watching the run, kept so shutdown can stop watching.
    running: Option<tokio::task::JoinHandle<()>>,
    /// What was asked for while the agent was away, applied when it comes
    /// back. A `/model` chosen mid-run used to be dropped outright, and a
    /// `/clear` was sent to the model as the word `/clear`.
    pending: Vec<Effect>,
    handle: AgentHandle,
    processes: Arc<exec::Registry>,
    workspace: Workspace,
    answers: Answers<Decision>,
    /// The one thread that calls into a script. Nothing here waits on it: a
    /// job goes in and its answer comes back as a message.
    plugins: Option<PluginHub>,
    hub: Hub<Msg>,
}

impl Executor {
    /// Everything the loop will need that the model must not hold.
    fn new(agent: Agent, app: &App, hub: Hub<Msg>) -> Self {
        Self {
            handle: agent.handle(),
            idle: Arc::new(Mutex::new(Some(agent))),
            running: None,
            pending: Vec::new(),
            processes: Arc::clone(&app.processes),
            workspace: app.workspace.clone(),
            answers: app.answers.clone(),
            plugins: None,
            hub,
        }
    }

    /// Do one thing, and report back with messages.
    ///
    /// Reads nothing of the model and writes nothing to it. Everything it
    /// learns goes back on the hub.
    fn perform(&mut self, effect: Effect) {
        match effect {
            Effect::StartRun(prompt) => self.start_run(prompt),
            Effect::Cancel => self.handle.cancel(),
            // The three that need the agent itself. Each is held when it is
            // away and applied the moment it is back, so nothing is lost.
            held @ (Effect::SetModel(_) | Effect::SetThinking(_) | Effect::ClearTranscript) => {
                match self
                    .idle
                    .lock()
                    .ok()
                    .as_deref_mut()
                    .and_then(Option::as_mut)
                {
                    Some(agent) => apply_to_agent(agent, held, &self.hub),
                    None => self.pending.push(held),
                }
            }
            Effect::Bang(command) => {
                let processes = Arc::clone(&self.processes);
                let root = self.workspace.root().to_path_buf();
                let hub = self.hub.clone();
                tokio::spawn(async move {
                    let output = run_bang(&processes, &root, &command).await;
                    hub.send(Msg::BangOutput { command, output });
                });
            }
            Effect::Kill(id) => self.processes.kill(id),
            Effect::SnapshotProcesses => {
                self.hub.send(Msg::Processes(self.processes.snapshot()));
            }
            Effect::Answer { id, decision } => self.answers.answer(id, decision),
            Effect::PluginCommand { name, args } => {
                self.to_plugins(PluginJob::Command { name, args });
            }
            Effect::PluginNotice(text) => self.to_plugins(PluginJob::Notice(text)),
            Effect::Surface {
                plugin,
                name,
                event,
            } => self.to_plugins(PluginJob::Surface {
                plugin,
                name,
                event,
            }),
            Effect::RefreshSurfaces => self.to_plugins(PluginJob::Refresh),
            Effect::PluginTick => self.to_plugins(PluginJob::Tick),
            Effect::Quit => {
                // Releasing the questions first unwinds whoever is blocked on
                // one; cancelling alone would leave it waiting out its timeout.
                self.answers.abandon_all();
                self.handle.cancel();
            }
        }
    }

    /// Queue a job for the script thread. Never waits.
    fn to_plugins(&self, job: PluginJob) {
        if let Some(plugins) = &self.plugins {
            plugins.send(job);
        }
    }

    fn start_run(&mut self, prompt: String) {
        let Some(mut agent) = self.idle.lock().ok().and_then(|mut idle| idle.take()) else {
            // Still away with the last run. The queue is pumped again when it
            // reports back, so the prompt is not lost.
            return;
        };
        // Whatever arrived while the last run held it.
        for held in std::mem::take(&mut self.pending) {
            apply_to_agent(&mut agent, held, &self.hub);
        }

        let work = tokio::spawn(async move {
            let outcome = agent.prompt(&prompt).await;
            (agent, outcome)
        });

        // A task of its own, because `perform` cannot wait and the agent has
        // to come back: without this it goes into the run and stays there, and
        // every later prompt is accepted and never sent.
        let slot = Arc::clone(&self.idle);
        let hub = self.hub.clone();
        self.running = Some(tokio::spawn(async move {
            match work.await {
                Ok((agent, outcome)) => {
                    // Back in the slot *before* the news goes out, so a
                    // `RunEnded` handled on the very next pass finds an idle
                    // agent to start the queue with.
                    if let Ok(mut slot) = slot.lock() {
                        *slot = Some(agent);
                    }
                    hub.send(Msg::RunEnded {
                        stop: outcome.stop,
                        turns: outcome.turns,
                        error: outcome.error,
                    });
                }
                Err(error) => {
                    hub.send(Msg::RunFailed(error.to_string()));
                }
            }
        }));
    }
}

/// Start the thread that calls into the scripts, and turn what it reports
/// into messages.
fn spawn_plugin_hub(host: Arc<aphid_plugin::PluginHost>, hub: Hub<Msg>) -> PluginHub {
    PluginHub::spawn(host, move |report| {
        match report {
            PluginReport::Command(actions) => {
                for action in actions {
                    let PluginAction::Notice(text) = action;
                    hub.send(Msg::Notice(text));
                }
            }
            PluginReport::Surface {
                plugin, actions, ..
            } => {
                hub.send(Msg::SurfaceDone {
                    plugin,
                    // No surface by that name any more: an empty answer, which
                    // the update reads as "it is gone, take the focus back".
                    actions: actions.unwrap_or_default(),
                });
            }
            PluginReport::Surfaces(open) => {
                hub.send(Msg::Panes(Panes::of(open)));
            }
        };
    })
}

/// Do one of the effects that only the agent itself can answer.
fn apply_to_agent(agent: &mut Agent, effect: Effect, hub: &Hub<Msg>) {
    match effect {
        Effect::SetModel(model) => {
            // The key belongs to the provider, not to the session: switching
            // to a model from somewhere else has to switch credentials with
            // it, or the next request goes out signed by the wrong provider.
            match api_key(&model) {
                Ok(key) => agent.set_api_key(Some(key)),
                Err(note) => {
                    agent.set_api_key(None);
                    hub.send(Msg::Notice(note));
                }
            }
            agent.set_model(*model);
        }
        Effect::SetThinking(level) => agent.set_thinking(level),
        Effect::ClearTranscript => {
            // Keep the system prompt; drop the conversation.
            let transcript = agent.transcript_mut();
            let keep = usize::from(
                transcript
                    .get(0)
                    .is_some_and(|m| m.role() == aphid_core::Role::System),
            );
            transcript.truncate(keep);
        }
        other => debug_assert!(false, "not the agent's to do: {other:?}"),
    }
}

impl Program for App {
    type Msg = Msg;
    type Effect = Effect;

    fn update(&mut self, msg: Msg) -> Cmd<Effect> {
        App::update(self, msg)
    }

    fn timer(&self, timer: Timer) -> Option<Msg> {
        Some(match timer {
            Timer::Frame => Msg::Frame,
            Timer::Poll => Msg::Poll,
            Timer::Tick => Msg::Tick,
        })
    }

    fn subs(&self) -> Subs {
        self.wanted_subs()
    }

    fn done(&self) -> bool {
        self.quitting()
    }
}

impl Draw for App {
    type Cache = render::CodeCache;

    fn draw(&self, frame: &mut Frame<'_>, cache: &mut Self::Cache) {
        render::draw(self, frame, cache);
    }

    fn laid_out(cache: &Self::Cache) -> Option<Msg> {
        render::laid_out(cache)
    }
}

impl Effects for Executor {
    type Program = App;

    fn perform(&mut self, effect: Effect, _hub: &Hub<Msg>) {
        Executor::perform(self, effect);
    }

    fn start(&mut self, _hub: &Hub<Msg>) {
        // The panels want drawing before the first frame, not after it.
        self.perform(Effect::RefreshSurfaces);
    }

    fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.abort();
        }
    }
}

/// Split `/name rest` into its two halves. `None` when the line is a prompt.
fn split_command(line: &str) -> Option<(&str, &str)> {
    let line = line.strip_prefix('/')?;
    let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
    Some((name, rest.trim()))
}

/// Run a `!` command to completion in the workspace root, and return its
/// output. The same engine and the same one-line status markers as the `bash`
/// tool; only the tool context is missing, because a `!` command belongs to
/// no tool call.
async fn run_bang(
    processes: &Arc<exec::Registry>,
    root: &std::path::Path,
    command: &str,
) -> String {
    // stdout and stderr are pumped concurrently and appended to one buffer, so
    // the output reads the way it would in a terminal, as in the `bash` tool.
    let collected = Arc::new(Mutex::new(String::new()));
    let sink = {
        let collected = Arc::clone(&collected);
        Arc::new(move |_stream: exec::Stream, line: &str| {
            if let Ok(mut buffer) = collected.lock() {
                buffer.push_str(line);
                buffer.push('\n');
            }
        })
    };

    let spec = exec::Spec::new("tui", command).cwd(Some(root.to_path_buf()));
    let status = exec::run(processes, spec, None, sink).await;

    let mut output = collected.lock().expect("output lock").clone();
    match status {
        exec::Status::Exited(0) => {}
        // A non-zero exit is not an error — plenty of commands report through
        // their status, and the user ran this one on purpose.
        exec::Status::Exited(code) => output.push_str(&format!("\n[exit code {code}]")),
        exec::Status::Signalled => output.push_str("\n[terminated by signal]"),
        exec::Status::TimedOut => output.push_str("\n[timed out]"),
        exec::Status::Cancelled => output.push_str("\n[cancelled]"),
        exec::Status::Killed | exec::Status::Killing => output.push_str("\n[killed]"),
        exec::Status::Failed(error) => output.push_str(&format!("\n[{error}]")),
        exec::Status::Running => {}
    }
    if output.is_empty() {
        output.push_str("[no output]");
    }
    output
}

/// The input box grows with content up to this many rows, then scrolls.
pub(crate) const MAX_INPUT_ROWS: u16 = 4;

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
    use crate::plugins::permissions::{Confirmer, Risk};
    use crate::tui::render::ScrollbackCache;
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

    /// Type a line and press Enter, collecting everything it asked for.
    fn type_line(app: &mut App, line: &str) -> Vec<Effect> {
        let mut effects = Vec::new();
        for c in line.chars() {
            effects.extend(press(app, KeyCode::Char(c)));
        }
        effects.extend(press(app, KeyCode::Enter));
        effects
    }

    fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
        app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .into_effects()
    }

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// The agent, when it is not away with a run.
    fn parked(ex: &Executor) -> Option<std::sync::MutexGuard<'_, Option<Agent>>> {
        let slot = ex.idle.lock().expect("the slot");
        slot.is_some().then_some(slot)
    }

    /// Wait for the run to report back, and apply what it said.
    async fn settle(
        app: &mut App,
        ex: &mut Executor,
        inbox: &mut tokio::sync::mpsc::UnboundedReceiver<Msg>,
    ) -> Msg {
        let msg = tokio::time::timeout(Duration::from_secs(5), inbox.recv())
            .await
            .expect("the run should report back")
            .expect("a message");
        for effect in app.update(msg.clone()).into_effects() {
            ex.perform(effect);
        }
        msg
    }

    /// An executor with nothing but the agent in it, for the few tests that
    /// are about the hand-off rather than about the model.
    fn executor(agent: Agent, hub: Hub<Msg>) -> Executor {
        Executor {
            handle: agent.handle(),
            idle: Arc::new(Mutex::new(Some(agent))),
            running: None,
            pending: Vec::new(),
            processes: Arc::new(exec::Registry::new()),
            workspace: Workspace::new(std::env::temp_dir()),
            answers: Answers::default(),
            plugins: None,
            hub,
        }
    }

    #[test]
    fn a_typed_line_asks_for_a_run_and_nothing_else() {
        let agent = agent_with(vec![Turn::text("hello back")]);
        let mut app = app_for(&agent);

        assert_eq!(
            type_line(&mut app, "hello"),
            [Effect::StartRun("hello".to_owned())]
        );
        assert!(app.status.running, "the status line says so at once");
        assert_eq!(app.queued(), None, "it went, so it is not still waiting");
    }

    #[tokio::test]
    async fn a_run_the_update_asked_for_actually_happens() {
        let agent = agent_with(vec![Turn::text("hello back")]);
        let mut app = app_for(&agent);
        let (hub, mut inbox) = crate::tui::runtime::channel();
        let mut ex = executor(agent, hub);

        for effect in type_line(&mut app, "hello") {
            ex.perform(effect);
        }
        settle(&mut app, &mut ex, &mut inbox).await;

        let parked = parked(&ex).expect("the agent came back");
        let agent = parked.as_ref().expect("the agent");
        // system, user, assistant
        assert_eq!(agent.transcript().len(), 3);
        assert_eq!(agent.transcript().get(1).unwrap().role(), Role::User);
    }

    /// The regression: the agent went into the run's task and nothing ever
    /// took it back out, so the second line typed in a session was accepted,
    /// marked as running, and never sent.
    #[tokio::test]
    async fn a_second_line_is_sent_without_anybody_handing_the_agent_back() {
        let agent = agent_with(vec![Turn::text("first"), Turn::text("second")]);
        let mut app = app_for(&agent);
        let (hub, mut inbox) = crate::tui::runtime::channel();
        let mut ex = executor(agent, hub);

        for effect in type_line(&mut app, "one") {
            ex.perform(effect);
        }

        // Nothing here puts the agent back: the executor has to, and has to
        // say so, or the queue is stuck for the rest of the session.
        let ended = settle(&mut app, &mut ex, &mut inbox).await;
        assert!(matches!(ended, Msg::RunEnded { .. }), "{ended:?}");

        let asked = type_line(&mut app, "two");
        assert_eq!(
            asked,
            [Effect::StartRun("two".to_owned())],
            "the second line asks for a run"
        );
        for effect in asked {
            ex.perform(effect);
        }
        settle(&mut app, &mut ex, &mut inbox).await;

        let parked = parked(&ex).expect("the agent came back a second time");
        let agent = parked.as_ref().expect("the agent");
        // system, user, assistant, user, assistant
        assert_eq!(agent.transcript().len(), 5, "both lines reached the model");
    }

    #[test]
    fn a_line_typed_mid_run_waits_for_the_one_in_flight() {
        let agent = agent_with(vec![Turn::text("first")]);
        let mut app = app_for(&agent);

        assert_eq!(
            type_line(&mut app, "one"),
            [Effect::StartRun("one".to_owned())]
        );

        // The agent is away, so this only queues.
        assert_eq!(type_line(&mut app, "two"), []);
        assert_eq!(app.queued(), Some("two"));
        assert!(app.status.queued, "the status line should say so");

        // And goes the moment the run reports back.
        let cmd = app.update(Msg::RunEnded {
            stop: aphid_core::StopReason::Stop,
            turns: 1,
            error: None,
        });
        assert_eq!(cmd.effects(), [Effect::StartRun("two".to_owned())]);
        assert!(!app.status.queued);
    }

    #[test]
    fn prompts_from_a_plugin_queue_in_order_behind_a_typed_line() {
        let agent = agent_with(vec![Turn::text("first")]);
        let mut app = app_for(&agent);

        type_line(&mut app, "typed");
        assert!(
            app.update(Msg::Prompt("from a plugin".to_owned()))
                .is_empty()
        );
        assert!(app.update(Msg::Prompt("and another".to_owned())).is_empty());

        // All three are in the pane already, as a typed line would be.
        let users: Vec<String> = app
            .scrollback
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                crate::tui::scrollback::Entry::User(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec!["typed", "from a plugin", "and another"]);

        // And they go one at a time, in the order they arrived.
        for expected in ["from a plugin", "and another"] {
            let cmd = app.update(Msg::RunEnded {
                stop: aphid_core::StopReason::Stop,
                turns: 1,
                error: None,
            });
            assert_eq!(cmd.effects(), [Effect::StartRun(expected.to_owned())]);
        }
        assert_eq!(app.queued(), None, "the queue is drained in order");
    }

    /// A reply arrives as hundreds of small deltas, and each one is a message.
    /// Anything the update does at every message is done hundreds of times per
    /// reply, so what does not change must cost nothing.
    #[test]
    fn a_long_stream_does_not_rebuild_what_did_not_change() {
        let agent = agent_with(vec![Turn::text("reply")]);
        let mut app = app_for(&agent);

        app.update(Msg::TurnStarted);
        let built = app.input.borders_built();
        for _ in 0..500 {
            app.update(Msg::Text("a token ".to_owned()));
        }

        assert_eq!(
            app.input.borders_built(),
            built,
            "the input border is the same border for the whole reply"
        );
        assert_eq!(
            app.status.download.bytes(),
            500 * "a token ".len() as u64,
            "and the meter's carried total kept up with every one of them"
        );
    }

    #[test]
    fn streaming_events_feed_the_download_meter() {
        let agent = agent_with(vec![Turn::text("reply")]);
        let mut app = app_for(&agent);

        app.update(Msg::Text("hello".to_owned()));
        app.update(Msg::Thinking("weighing".to_owned()));
        app.update(Msg::ToolStreamDelta {
            block: 0,
            bytes: 40,
        });

        // The meter counts the chunk bytes: prose, reasoning and tool-call
        // argument deltas alike.
        assert_eq!(app.status.download.bytes(), 5 + 8 + 40);
        assert!(app.status.download.rate_kb_s().is_some());
    }

    #[test]
    fn the_download_meter_clears_when_the_turn_ends() {
        let agent = agent_with(vec![Turn::text("reply")]);
        let mut app = app_for(&agent);

        app.update(Msg::Text("hello".to_owned()));
        assert_eq!(app.status.download.bytes(), 5);

        app.update(Msg::TurnEnded {
            usage: aphid_core::Usage::default(),
            stop: aphid_core::StopReason::Stop,
            error: None,
        });
        assert_eq!(app.status.download.bytes(), 0);
        assert!(app.status.download.rate_kb_s().is_none());
    }

    /// The whole streaming path, from protocol events to the transcript: a
    /// tool-call block must produce a card while it streams, and the announced
    /// call must land in that same card rather than a second one.
    #[tokio::test]
    async fn a_streamed_tool_call_shows_up_before_it_is_announced() {
        use crate::tui::scrollback::{Entry, ToolState};

        let (backend, _script) = scripted(vec![
            Turn::call("c1", "bash", r#"{"command":"cargo test"}"#),
            Turn::text("all green"),
        ]);
        let (events, mut receiver) = crate::tui::runtime::channel::<crate::tui::msg::Msg>();
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
            app.update(event);
            // Between the block opening and the call being announced, the card
            // is on screen with a byte count.
            if let Some(Entry::Tool {
                state: ToolState::Streaming,
                name,
                streamed,
                ..
            }) = app.scrollback.entries().last()
            {
                assert_eq!(name, "bash");
                streaming_seen |= *streamed > 0;
            }
        }

        assert!(streaming_seen, "the card should have counted bytes");
        let tools: Vec<&Entry> = app
            .scrollback
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

    /// A transcript long enough to have somewhere to scroll to, already laid
    /// out once: a wheel step means nothing until the pane has a size.
    fn scrollable(app: &mut App, cache: &mut ScrollbackCache) -> usize {
        for number in 0..40 {
            app.scrollback.push_notice(format!("notice {number}"));
        }
        drawn(app, cache)
    }

    /// One frame's worth of laying out, and telling the model about it.
    fn drawn(app: &mut App, cache: &mut ScrollbackCache) -> usize {
        let view = cache.layout(&app.scrollback, 40, 10);
        app.update(Msg::LaidOut(crate::tui::render::Laid {
            viewport: view,
            hits: Vec::new(),
        }));
        view.top
    }

    /// A gated call is a question with a task blocked behind it. These say
    /// that the answer reaches that task, and that no answer ever leaves it
    /// blocked.
    #[test]
    fn answering_a_prompt_reaches_the_call_that_asked() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let (id, waiting) = app.answers.open();

        app.update(Msg::Confirm {
            id,
            tool: "bash".to_owned(),
            summary: "rm -rf build".to_owned(),
            risk: Risk::Destructive,
        });
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));

        let asked = press(&mut app, KeyCode::Char('n'));
        assert_eq!(
            asked,
            [Effect::Answer {
                id,
                decision: Decision::Deny,
            }]
        );
        app.answers.answer(id, Decision::Deny);

        assert_eq!(waiting.try_recv(), Ok(Decision::Deny));
        assert!(app.modal.is_none(), "the question is answered and gone");
    }

    #[test]
    fn a_second_keypress_on_an_answered_prompt_does_nothing() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let (id, waiting) = app.answers.open();

        app.update(Msg::Confirm {
            id,
            tool: "bash".to_owned(),
            summary: "rm -rf build".to_owned(),
            risk: Risk::Destructive,
        });
        for answer in [Decision::Allow, Decision::Deny] {
            let code = match answer {
                Decision::Allow => KeyCode::Char('y'),
                _ => KeyCode::Char('n'),
            };
            for effect in press(&mut app, code) {
                if let Effect::Answer { id, decision } = effect {
                    app.answers.answer(id, decision);
                }
            }
        }

        assert_eq!(waiting.try_recv(), Ok(Decision::Allow));
        assert!(waiting.try_recv().is_err(), "only the first answer counted");
    }

    #[tokio::test]
    async fn a_session_that_quits_refuses_what_it_was_asked() {
        let (events, mut receiver) = crate::tui::runtime::channel::<crate::tui::msg::Msg>();
        let answers = Answers::default();
        let confirmer = UiConfirmer::new(events, answers.clone());

        // Blocks exactly as the agent's task does, on a thread of its own.
        let asked: tokio::task::JoinHandle<Decision> = tokio::task::spawn_blocking(move || {
            confirmer.confirm("bash", "rm -rf /", Risk::Destructive)
        });

        let event = receiver.recv().await.expect("the question");
        assert!(matches!(event, Msg::Confirm { .. }));

        // What the loop does on the way out.
        answers.abandon_all();

        assert_eq!(
            asked.await.expect("the blocked call"),
            Decision::Deny,
            "a question nobody will answer is a refusal, not a wait"
        );
    }

    #[test]
    fn mouse_wheel_scrolls_the_transcript() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let mut cache = ScrollbackCache::default();
        let bottom = scrollable(&mut app, &mut cache);

        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollUp)));
        assert_eq!(drawn(&mut app, &mut cache), bottom - MOUSE_SCROLL_LINES);

        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollDown)));
        assert_eq!(drawn(&mut app, &mut cache), bottom);
    }

    #[test]
    fn mouse_wheel_down_saturates_at_the_newest_message() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let mut cache = ScrollbackCache::default();
        let bottom = scrollable(&mut app, &mut cache);

        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollDown)));
        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollDown)));
        assert_eq!(drawn(&mut app, &mut cache), bottom);
        assert!(!app.scrollback.scrolled());
    }

    #[test]
    fn non_wheel_mouse_events_do_not_scroll() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        scrollable(&mut app, &mut ScrollbackCache::default());

        app.update(Msg::Mouse(mouse(MouseEventKind::Moved)));
        assert!(!app.scrollback.scrolled());
    }

    #[test]
    fn mouse_wheel_does_not_change_the_input() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        app.input.paste("draft");

        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollUp)));
        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollDown)));

        assert_eq!(app.input.text(), "draft");
    }

    #[tokio::test]
    async fn a_slash_command_is_handled_rather_than_sent() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);

        type_line(&mut app, "/tools");

        assert!(app.queued().is_none(), "a command is not a prompt");
    }

    fn notice_lines(app: &App) -> Vec<String> {
        use crate::tui::scrollback::Entry;
        app.scrollback
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
        app.skills = vec![
            skill("release", "How to cut a release.", true),
            skill("review", &"a very wordy description ".repeat(20), false),
        ];
        app.skill_diagnostics = vec![skills::Diagnostic {
            path: PathBuf::from("/w/.aphid/skills/broken.md"),
            message: "no `description` in the frontmatter".to_owned(),
        }];

        type_line(&mut app, "/skills");

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

        type_line(&mut app, "/skills");

        assert!(notice_lines(&app).contains(&"no skills are loaded".to_owned()));
    }

    #[tokio::test]
    async fn ps_opens_the_process_list() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);

        type_line(&mut app, "/ps");

        assert!(matches!(app.modal, Some(Modal::Processes { .. })));
        assert!(app.queued().is_none(), "a command is not a prompt");
    }

    /// The one time a user most wants to know what is running is while
    /// something is running, which is exactly when the agent is away.
    #[tokio::test]
    async fn ps_works_while_a_run_holds_the_agent() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        app.status.running = true;

        type_line(&mut app, "/ps");

        assert!(matches!(app.modal, Some(Modal::Processes { .. })));
        assert_eq!(app.queued(), None, "it must not queue as a prompt");
    }

    /// A command mid-run used to be sent to the model as the literal text of
    /// the command, because the agent it wanted was away. The update answers
    /// out of the model now, so it simply runs.
    #[test]
    fn a_command_works_while_a_run_holds_the_agent() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        app.status.running = true;

        assert!(type_line(&mut app, "/tools").is_empty());

        assert_eq!(app.queued(), None, "it must not be sent as a prompt");
        assert!(
            notice_lines(&app)[0].starts_with("tools:"),
            "{:?}",
            notice_lines(&app)
        );
    }

    /// The commands that do need the agent are held for it rather than lost.
    #[tokio::test]
    async fn a_model_switch_mid_run_is_applied_when_the_agent_returns() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        let (hub, _inbox) = crate::tui::runtime::channel();
        let mut ex = executor(agent, hub);

        // Away with a run.
        let held = ex.idle.lock().expect("the slot").take().expect("the agent");
        app.status.running = true;

        let asked = type_line(&mut app, "/think high");
        assert!(
            matches!(asked.as_slice(), [Effect::SetThinking(_)]),
            "{asked:?}"
        );
        for effect in asked {
            ex.perform(effect);
        }
        assert_eq!(ex.pending.len(), 1, "held for the agent, not dropped");

        // It comes back, and what was waiting is applied before the next run.
        *ex.idle.lock().expect("the slot") = Some(held);
        app.status.running = false;
        for effect in type_line(&mut app, "hello") {
            ex.perform(effect);
        }
        assert!(ex.pending.is_empty(), "the wait is over");
    }

    #[tokio::test]
    async fn k_stops_the_selected_process() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);

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

        // Opening the list asks for a snapshot; the test provides it, as the
        // executor would.
        assert_eq!(type_line(&mut app, "/ps"), [Effect::SnapshotProcesses]);
        app.update(Msg::Processes(processes.snapshot()));

        let asked = press(&mut app, KeyCode::Char('k'));
        let [Effect::Kill(id)] = asked.as_slice() else {
            panic!("k should ask for a kill: {asked:?}");
        };
        processes.kill(*id);

        let status = running.await.expect("the sleep");
        assert_eq!(status, exec::Status::Killed);
    }

    /// A throwaway workspace for `!` commands that must not touch the repo.
    fn temp_workspace() -> Workspace {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let root = std::env::temp_dir().join(format!(
            "aphid-bang-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create");
        Workspace::new(root)
    }

    /// Type a `!` line, run what it asked for, and feed the output back.
    async fn run_bang_line(app: &mut App, line: &str) {
        let asked = type_line(app, line);
        assert_eq!(app.queued(), None, "a bang line is not a prompt");
        let [Effect::Bang(command)] = asked.as_slice() else {
            panic!("a bang line should ask for one command: {asked:?}");
        };

        let processes = Arc::new(exec::Registry::new());
        let output = run_bang(&processes, app.workspace.root(), command).await;
        app.update(Msg::BangOutput {
            command: command.clone(),
            output,
        });
    }

    fn shell_outputs(app: &App) -> Vec<String> {
        use crate::tui::scrollback::Entry;
        app.scrollback
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                Entry::Shell { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_bang_line_runs_the_command_and_prints_its_output() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        app.workspace = temp_workspace();

        run_bang_line(&mut app, "!echo hi").await;

        assert_eq!(shell_outputs(&app), vec!["hi\n"]);
        assert!(!app.status.running, "no agent run was started");
    }

    #[tokio::test]
    async fn a_failing_bang_command_prints_its_exit_code() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        app.workspace = temp_workspace();

        run_bang_line(&mut app, "!exit 3").await;

        let shown = shell_outputs(&app).remove(0);
        assert!(shown.ends_with("[exit code 3]"), "{shown:?}");
    }

    #[tokio::test]
    async fn a_bang_line_works_while_a_run_holds_the_agent() {
        let agent = agent_with(vec![Turn::text("unused")]);
        let mut app = app_for(&agent);
        app.workspace = temp_workspace();
        app.status.running = true;

        assert_eq!(
            type_line(&mut app, "!echo hi"),
            [Effect::Bang("echo hi".to_owned())],
            "a bang line needs no agent, so a run in flight does not stop it"
        );
        assert_eq!(app.queued(), None, "it must not queue as a prompt");
    }
}

#[cfg(test)]
mod plugin_tests {
    use std::sync::Arc;

    use crate::tui::effect::Effect;
    use crate::tui::msg::Msg;

    use aphid_agent::{Agent, exec};
    use aphid_core::providers::deepseek;
    use aphid_plugin::{Capabilities, PluginHost, Silent, explicit};

    use super::{App, Status};
    use crate::tui::event::UiSink;
    use crate::tui::scrollback::Entry;

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
        app.plugin_commands = host
            .commands()
            .into_iter()
            .map(|command| command.invocation)
            .collect();
        app.host = Some(host);
        (app, agent)
    }

    /// Render the panels and hand them to the model, as the script thread
    /// does. Called straight rather than through the thread, so the test does
    /// not have to wait for one.
    fn refresh(app: &mut App, host: &Arc<PluginHost>) {
        let open: Vec<aphid_plugin::Open> = host
            .surfaces()
            .into_iter()
            .filter_map(|surface| {
                let aphid_plugin::Placement::Side(side) = surface.placement;
                let widget = match host.render_surface(&surface.plugin, &surface.name)? {
                    aphid_plugin::SurfaceRender::Widget(widget) => widget,
                    _ => return None,
                };
                Some(aphid_plugin::Open {
                    plugin: surface.plugin,
                    name: surface.name,
                    side,
                    interactive: surface.interactive,
                    widget,
                })
            })
            .collect();
        app.update(Msg::Panes(crate::tui::surface::Panes::of(open)));
    }

    fn notices(app: &App) -> Vec<String> {
        app.scrollback
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
        let (events, mut receiver) = crate::tui::runtime::channel::<crate::tui::msg::Msg>();
        let host = fixture.host_with(Arc::new(UiSink::new(events)));
        let (mut app, _agent) = app_with(Arc::clone(&host));

        // The update names the plugin's command rather than running it; the
        // executor is what reaches into the script.
        let asked = app.command("greet", "Ana").into_effects();
        assert_eq!(
            asked,
            [Effect::PluginCommand {
                name: "greet".to_owned(),
                args: "Ana".to_owned(),
            }]
        );
        for action in host.run_command("greet", "Ana").expect("the command") {
            let aphid_plugin::Action::Notice(text) = action;
            app.update(Msg::Notice(text));
        }
        assert_eq!(notices(&app), vec!["greeting Ana"]);

        // The prompt took the long way round, as a message the loop applies,
        // and goes to the agent like a typed line.
        let event = receiver.try_recv().expect("the prompt was sent");
        assert_eq!(
            app.update(event).into_effects(),
            [Effect::StartRun("Say hello to Ana".to_owned())]
        );
    }

    #[test]
    fn a_focused_surface_gets_keys_and_can_release_focus() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let fixture = Fixture::new(
            r#"
            register_command(#{
                name: "panel",
                description: "Open the panel.",
                run: |args| {
                    let s = surface_state("panel");
                    s.open = true;
                    surface_state("panel", s);
                    notice("panel on")
                }
            });
            register_surface(#{
                name: "panel",
                placement: #{ kind: "side", side: "right" },
                init: || #{ open: false, count: 0 },
                view: |s| {
                    if !s.open { return (); }
                    #{ type: "text", text: "panel" }
                },
                update: |s, msg| {
                    if msg.kind == "key" && msg.code == "down" {
                        s.count += 1;
                        return s;
                    }
                    if msg.kind == "key" && msg.code == "esc" {
                        return "release_focus";
                    }
                    s
                }
            });
            "#,
        );
        let host = fixture.host();
        let (mut app, _agent) = app_with(host.clone());
        refresh(&mut app, &host);
        assert!(!app.surfaces.any_open(), "the panel starts closed");

        let _ = host.run_command("panel", "");
        refresh(&mut app, &host);
        assert!(app.surfaces.any_open(), "the panel is now open");

        app.surfaces.focus_first();
        assert!(app.surfaces.focus().is_some(), "the panel can take focus");

        let asked = app
            .surface_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .into_effects();
        let [
            Effect::Surface {
                plugin,
                name,
                event,
            },
        ] = asked.as_slice()
        else {
            panic!("a key on a focused panel goes to it: {asked:?}");
        };
        host.surface_event(plugin, name, event.clone())
            .expect("the surface took it");

        let count = host.plugins()[0]
            .surface_state("panel")
            .get("count")
            .and_then(|value| value.as_int().ok())
            .expect("the event ran");
        assert_eq!(count, 1);

        app.surface_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            app.surfaces.focus().is_none(),
            "Esc returned focus to the input"
        );
    }

    #[test]
    fn a_built_in_command_wins_over_a_plugin_of_the_same_name() {
        let fixture = Fixture::new(
            r#"register_command(#{ name: "help", run: |args| { prompt("hijacked") } });"#,
        );
        let (mut app, _agent) = app_with(fixture.host());

        assert!(
            app.command("help", "").effects().is_empty(),
            "the built-in ran, not the plugin"
        );
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
        let (mut app, _agent) = app_with(fixture.host());

        assert!(app.command("nope", "").effects().is_empty());
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
        let (mut app, _agent) = app_with(fixture.host());

        app.command("plugins", "");

        let summary = notices(&app).remove(0);
        assert!(summary.contains("kit"), "{summary}");
        assert!(summary.contains("on_run_start"), "{summary}");
        assert!(summary.contains("/greet"), "{summary}");
        assert!(summary.contains("Say hello."), "{summary}");
    }
}
