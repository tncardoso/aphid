//! The GPUI desktop front end.

pub mod theme;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use aphid_agent::rt::Component;
use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement,
    ListAlignment, ListState, Render, SharedString, Window, WindowBounds, WindowOptions, div, list,
    prelude::*, px, rgb, rgba, size,
};
use gpui_component::Root;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::list::ListItem;
use gpui_component::text::TextView;

use crate::events::{Session, SessionEnd, SessionStart};
use crate::harness::{self, HarnessOptions};
use crate::plugins::permissions::{Decision, PermissionGate, Permissions};
use crate::plugins::scripts;
use crate::scripting::{
    Action as PluginAction, Host, Job as PluginJob, Open, PluginHub, Report as PluginReport,
    SurfaceEvent, Widget,
};
use crate::session::{self, Summary, sessions_dir};
use crate::tui::app::{Executor, PluginLoader};
use crate::tui::modal::Modal;
use crate::tui::runtime::{Answers, Hub, channel};
use crate::tui::scrollback::{Entry, ToolState};
use crate::tui::{App as CodeApp, Effect, Msg, UiComponent, UiConfirmer, UiSink};
use crate::{HarnessOptions as Options, Workspace};

gpui::actions!(
    aphid,
    [
        /// Stop the run in flight.
        CancelRun,
        /// Put a line break in the text box instead of sending it.
        NewLine
    ]
);

/// The key context of the window.
///
/// The text box has a deeper one of its own, so its bindings are tried first
/// and these are what is left: `Escape` reaches here because the box lets it
/// through, and `Shift-Enter` because the box does not bind it at all.
const CONTEXT: &str = "Aphid";
use theme::{ACCENT, BACKGROUND, BORDER, DANGER, MUTED, PANEL, PANEL_RAISED, TEXT, USER};

#[derive(Clone)]
struct GuiConfig {
    workspace: Workspace,
    cwd: PathBuf,
    home: Option<PathBuf>,
    model: aphid_core::Model,
    thinking: Option<aphid_core::ThinkingLevel>,
    system: Option<String>,
    append_system: Option<String>,
    load_context: bool,
    max_turns: u32,
    api_key: Option<compact_str::CompactString>,
    plugin_files: Vec<crate::scripting::PluginFile>,
    processes: Arc<aphid_agent::exec::Registry>,
    stream_fn: Option<aphid_agent::StreamFn>,
}

impl GuiConfig {
    fn capture(options: &HarnessOptions) -> Self {
        Self {
            workspace: options.workspace.clone(),
            cwd: options.cwd.clone(),
            home: options.home.clone(),
            model: options.model.clone(),
            thinking: options.thinking,
            system: options.system.clone(),
            append_system: options.append_system.clone(),
            load_context: options.load_context,
            max_turns: options.max_turns,
            api_key: options.api_key.clone(),
            plugin_files: options.plugin_files.clone(),
            processes: Arc::clone(&options.processes),
            stream_fn: options.stream_fn.clone(),
        }
    }

    fn options(&self) -> HarnessOptions {
        HarnessOptions {
            workspace: self.workspace.clone(),
            cwd: self.cwd.clone(),
            home: self.home.clone(),
            model: self.model.clone(),
            thinking: self.thinking,
            system: self.system.clone(),
            append_system: self.append_system.clone(),
            load_context: self.load_context,
            scope: None,
            max_turns: self.max_turns,
            api_key: self.api_key.clone(),
            composition: aphid_agent::rt::Composition::new(),
            plugin_files: self.plugin_files.clone(),
            host: None,
            processes: Arc::clone(&self.processes),
            stream_fn: self.stream_fn.clone(),
        }
    }
}

struct Backend {
    app: CodeApp,
    executor: Executor,
    composition: aphid_agent::rt::Composition,
    host: Arc<crate::scripting::PluginHost>,
    session_id: Option<String>,
    session_path: Option<PathBuf>,
    surfaces: Vec<Open>,
    stopped: bool,
}

impl Backend {
    fn apply(&mut self, msg: Msg) {
        if let Msg::PluginSurfaces(surfaces) = &msg {
            self.surfaces.clone_from(surfaces);
        }
        let cmd = self.app.update(msg);
        for effect in cmd.into_effects() {
            self.executor.perform(effect);
        }
    }

    fn submit(&mut self, line: String) {
        let cmd = self.app.submit(line);
        for effect in cmd.into_effects() {
            self.executor.perform(effect);
        }
    }

    fn perform(&mut self, effect: Effect) {
        self.executor.perform(effect);
    }

    fn shutdown(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.executor.perform(Effect::Quit);
        if let Some(plugins) = self.executor.plugins.take() {
            plugins.stop();
        }
        self.composition.bus.emit(&mut SessionEnd(Session {
            id: self.session_id.clone(),
            path: self.session_path.clone(),
            reason: "end".to_owned(),
            restored: 0,
        }));
        self.host.flush();
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn bootstrap(
    mut options: HarnessOptions,
    resume: Option<PathBuf>,
    confirm: bool,
) -> Result<(Backend, tokio::sync::mpsc::UnboundedReceiver<Msg>), String> {
    let (events, receiver) = channel();
    let answers = Answers::default();

    options.composition.mount(
        Arc::new(UiComponent::new(events.clone(), &options.composition)),
        serde_json::Value::Null,
    )?;
    if confirm {
        let permissions = Arc::new(Permissions::new(Arc::new(UiConfirmer::new(
            events.clone(),
            answers.clone(),
        ))));
        options.composition.mount(
            Arc::new(PermissionGate::new(None, permissions, &options.composition)),
            serde_json::Value::Null,
        )?;
    }

    let workspace = options.workspace.clone();
    let cwd = options.cwd.clone();
    let thinking = options.thinking;
    let model_id = options.model.id.to_string();
    let processes = Arc::clone(&options.processes);
    let plugin_files = std::mem::take(&mut options.plugin_files);
    let mut notes = Vec::new();
    let (host, plugin_problems) = scripts::load(
        &workspace,
        &plugin_files,
        Arc::new(UiSink::new(events.clone())),
        &processes,
    );
    let registries = crate::registries::Registries::for_composition(&options.composition);
    options
        .composition
        .add(
            Arc::clone(&registries) as Arc<dyn Component>,
            serde_json::Value::Null,
        )
        .await?;

    let mut plugin_loader = aphid_agent::rt::Loader::new(
        &options.composition,
        Arc::new(crate::scripting::Scripts::new(
            host.clone(),
            &options.composition,
        )),
    );
    if !host.is_empty() {
        let rows = match crate::scripting::read(workspace.root()) {
            Ok(rows) => rows,
            Err(error) => {
                notes.push(error);
                Vec::new()
            }
        };
        let report = plugin_loader
            .reconcile(crate::scripting::compose(&plugin_files, &rows))
            .await;
        for (id, error) in &report.failed {
            notes.push(format!("plugin {id}: {error}"));
        }
        options.host = Some(host.clone());
    }

    let (session, resumed) = session::attach(
        &sessions_dir(),
        workspace.root(),
        &cwd,
        Some(&model_id),
        resume.as_deref(),
        Arc::clone(&options.composition.transcript),
    )
    .map_err(|error| error.to_string())?;
    options
        .composition
        .add(session.clone(), serde_json::Value::Null)
        .await?;

    let session_id = session.id();
    let session_path = session.path();
    let composition = options.composition.clone();
    composition.bus.emit(&mut SessionStart(Session {
        id: session_id.clone(),
        path: session_path.clone(),
        reason: if resumed.is_some() { "resume" } else { "new" }.to_owned(),
        restored: 0,
    }));

    let mut harness = harness::build(options);
    let mut app = CodeApp::new(&harness, thinking, &processes);
    app.answers = answers;
    app.session_label = session.id().zip(session.path()).map_or_else(
        || "not being saved".to_owned(),
        |(id, path)| format!("{id} — {}", path.display()),
    );
    app.session = Some(session);
    app.host = Some(host.clone());
    app.composition = Some(composition.clone());
    app.registries = Some(Arc::clone(&registries));
    app.plugin_commands = crate::scripting::registered_commands(registries.commands())
        .into_iter()
        .map(|command| command.invocation)
        .collect();
    app.plugins_watch_notices = composition.bus.has_listeners::<crate::events::Notice>();
    app.plugins_tick =
        composition.bus.has_listeners::<crate::events::Tick>() || !registries.surfaces().is_empty();
    app.plugins_draw = !registries.surfaces().is_empty();

    if resumed.is_none() {
        app.scrollback.push_logo();
    }
    for note in notes.iter().chain(harness.notes.iter()) {
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
    if let Some(restored) = resumed {
        app.replay(&restored);
        let restored_count = session::splice(&mut harness.agent, &restored);
        app.scrollback
            .push_notice(format!("── resumed {restored_count} messages ──"));
    }
    app.scrollback.push_notice(format!(
        "aphid · {} · {} — /help for commands",
        harness.agent.model().id,
        workspace.root().display()
    ));

    let mut executor = Executor::new(harness.agent, &app, events.clone());
    executor.plugins = Some(spawn_plugin_hub(
        host.clone(),
        Arc::clone(&composition.bus),
        Arc::clone(&registries),
        events,
    ));
    if !host.is_empty() {
        executor.loader = Some(Arc::new(tokio::sync::Mutex::new(PluginLoader {
            loader: plugin_loader,
            root: workspace.root().to_path_buf(),
            home: crate::context::home_dir(),
        })));
    }
    if let Some(plugins) = &executor.plugins {
        plugins.send(PluginJob::Refresh);
    }

    Ok((
        Backend {
            app,
            executor,
            composition,
            host,
            session_id,
            session_path,
            surfaces: Vec::new(),
            stopped: false,
        },
        receiver,
    ))
}

fn spawn_plugin_hub(
    host: Arc<crate::scripting::PluginHost>,
    bus: Arc<aphid_agent::rt::Bus>,
    registries: Arc<crate::registries::Registries>,
    hub: Hub<Msg>,
) -> PluginHub {
    PluginHub::spawn(host, bus, registries, move |report| match report {
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
                actions: actions.unwrap_or_default(),
            });
        }
        PluginReport::Surfaces(open) => {
            hub.send(Msg::PluginSurfaces(open));
        }
    })
}

struct DesktopView {
    backend: Backend,
    config: GuiConfig,
    runtime: tokio::runtime::Handle,
    confirm: bool,
    workspace: Workspace,
    sessions: Vec<Summary>,
    drawer_open: bool,
    /// The text box. It owns its own cursor, selection and marked text, which
    /// is what makes a dead key compose: `Keystroke.key_char` gives the key and
    /// not the character, so nothing built on key events alone can type `á`.
    composer: Entity<InputState>,
    focus: FocusHandle,
    /// The transcript, measured item by item and anchored at the newest.
    ///
    /// It holds the heights it measured, so the pane draws the entries that are
    /// on screen and not the whole conversation on every frame.
    entries: ListState,
    /// What each entry looked like when it was last measured. An entry whose
    /// fingerprint moved is spliced, which is what throws its height away.
    fingerprints: Vec<u64>,
    expanded_tools: HashSet<usize>,
    expanded_thinking: HashSet<usize>,
    switching_session: bool,
    session_generation: u64,
}

impl DesktopView {
    fn new(
        backend: Backend,
        config: GuiConfig,
        runtime: tokio::runtime::Handle,
        confirm: bool,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sessions = session::list_for(&sessions_dir(), workspace.root());
        let composer = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(1, 5)
                .soft_wrap(true)
                .placeholder("Ask aphid, or type /help…")
        });
        cx.subscribe_in(&composer, window, Self::on_composer)
            .detach();
        Self {
            backend,
            config,
            runtime,
            confirm,
            workspace,
            sessions,
            drawer_open: true,
            composer,
            focus: cx.focus_handle(),
            // Overdraw of a screen and a half: enough that a scroll of one page
            // has nothing to measure, and not so much that opening a long
            // conversation measures all of it.
            entries: ListState::new(0, ListAlignment::Bottom, px(900.)),
            fingerprints: Vec::new(),
            expanded_tools: HashSet::new(),
            expanded_thinking: HashSet::new(),
            switching_session: false,
            session_generation: 0,
        }
    }

    fn attach_receiver(
        &mut self,
        mut receiver: tokio::sync::mpsc::UnboundedReceiver<Msg>,
        cx: &mut Context<Self>,
    ) {
        let generation = self.session_generation;
        cx.spawn(async move |weak, cx| {
            while let Some(msg) = receiver.recv().await {
                if weak
                    .update(cx, |view, cx| view.receive(generation, msg, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |weak, cx| {
            loop {
                gpui::Timer::after(std::time::Duration::from_millis(250)).await;
                let keep = weak.update(cx, |view, cx| view.tick(generation, cx));
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Tell the list what changed since the last frame.
    ///
    /// A transcript grows at the end, but it does not only grow: a tool result
    /// lands in a card that was drawn several entries ago, and opening a card
    /// changes its height. So each entry carries a fingerprint of the things
    /// that can change its size, and the ones that moved are spliced — which is
    /// what makes the list measure them again and keeps every other height.
    fn sync_entries(&mut self) {
        let entries = self.backend.app.scrollback.entries();
        let fresh: Vec<u64> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                fingerprint(
                    entry,
                    self.expanded_tools.contains(&index),
                    self.expanded_thinking.contains(&index),
                )
            })
            .collect();

        // A transcript that shrank is a different conversation: a session was
        // opened, or the pane was cleared. Nothing measured before is worth
        // keeping.
        if fresh.len() < self.fingerprints.len() {
            self.entries.reset(fresh.len());
            self.fingerprints = fresh;
            return;
        }

        let old = self.fingerprints.len();
        for (index, (new, was)) in fresh.iter().zip(&self.fingerprints).enumerate() {
            if new != was {
                self.entries.splice(index..index + 1, 1);
            }
        }
        if fresh.len() > old {
            self.entries.splice(old..old, fresh.len() - old);
        }
        self.fingerprints = fresh;
    }

    fn open_session(&mut self, resume: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.backend.app.status.running || self.switching_session {
            return;
        }
        self.switching_session = true;
        let work = self
            .runtime
            .spawn(bootstrap(self.config.options(), resume, self.confirm));
        cx.spawn(async move |weak, cx| {
            let result = match work.await {
                Ok(result) => result,
                Err(error) => Err(format!("could not switch sessions: {error}")),
            };
            let _ = weak.update(cx, |view, cx| {
                view.switching_session = false;
                match result {
                    Ok((backend, receiver)) => {
                        view.backend = backend;
                        view.session_generation = view.session_generation.wrapping_add(1);
                        view.attach_receiver(receiver, cx);
                        view.sessions = session::list_for(&sessions_dir(), view.workspace.root());
                    }
                    Err(error) => view.backend.app.scrollback.push_notice(error),
                }
                view.sync_entries();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn receive(&mut self, generation: u64, msg: Msg, cx: &mut Context<Self>) {
        if generation != self.session_generation {
            return;
        }
        let refresh_sessions = matches!(&msg, Msg::RunEnded { .. } | Msg::RunFailed(_));
        self.backend.apply(msg);
        if refresh_sessions {
            self.sessions = session::list_for(&sessions_dir(), self.workspace.root());
        }
        self.sync_entries();
        if self.backend.app.quitting() {
            cx.quit();
            return;
        }
        cx.notify();
    }

    fn tick(&mut self, generation: u64, cx: &mut Context<Self>) -> bool {
        if generation != self.session_generation {
            return false;
        }
        if self.backend.app.plugins_tick {
            self.backend.perform(Effect::PluginTick);
            self.backend.perform(Effect::RefreshSurfaces);
        }
        if matches!(self.backend.app.modal, Some(Modal::Processes { .. })) {
            self.backend.perform(Effect::SnapshotProcesses);
        }
        cx.notify();
        true
    }

    fn toggle_drawer(&mut self, cx: &mut Context<Self>) {
        self.drawer_open = !self.drawer_open;
        cx.notify();
    }

    fn new_chat(&mut self, cx: &mut Context<Self>) {
        self.open_session(None, cx);
    }

    /// What the text box says happened.
    ///
    /// `Enter` in a multi-line box inserts the newline before it reports the
    /// press, so the line is taken with that newline trimmed off. A newline on
    /// purpose is `Shift-Enter`, which the box does not bind and this view does.
    fn on_composer(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { secondary: false } => self.send(window, cx),
            InputEvent::Change => cx.notify(),
            _ => {}
        }
    }

    /// Stop the run, or empty the box when nothing is running.
    fn on_cancel(&mut self, _: &CancelRun, window: &mut Window, cx: &mut Context<Self>) {
        if self.backend.app.status.running {
            self.backend.perform(Effect::Cancel);
        } else {
            self.clear_composer(window, cx);
        }
        cx.notify();
    }

    /// Break the line rather than send it.
    fn on_new_line(&mut self, _: &NewLine, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |state, cx| {
            state.insert("\n", window, cx);
        });
        cx.notify();
    }

    /// The line in the text box, without the newline `Enter` just put there.
    fn composed(&self, cx: &App) -> String {
        self.composer
            .read(cx)
            .value()
            .trim_end_matches('\n')
            .trim()
            .to_owned()
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let line = self.composed(cx);
        self.clear_composer(window, cx);
        if line.is_empty() {
            return;
        }
        if let Some(command) = line.strip_prefix('!') {
            self.backend
                .perform(Effect::Bang(command.trim().to_owned()));
        } else {
            self.backend.submit(line);
        }
        self.sync_entries();
        if self.backend.app.quitting() {
            cx.quit();
            return;
        }
        cx.notify();
    }

    /// Empty the text box.
    ///
    /// Its value is owned by the box, so this asks it rather than assigning to
    /// a string of our own.
    fn clear_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
    }

    fn answer(&mut self, decision: Decision, cx: &mut Context<Self>) {
        let Some(Modal::Confirm(confirm)) = self.backend.app.modal.take() else {
            return;
        };
        self.backend.perform(Effect::Answer {
            id: confirm.id,
            decision,
        });
        cx.notify();
    }

    /// Take the model the pointer landed on.
    ///
    /// The terminal commits a choice with `Enter`, and this used to reach that
    /// path by making a key event that nobody pressed. It names the model
    /// instead, which is what a pointer knows and a keyboard does not.
    fn choose_model(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(Modal::Models { models, .. }) = &self.backend.app.modal else {
            return;
        };
        let Some(model) = models.get(index).cloned() else {
            return;
        };
        self.backend.app.modal = None;
        let cmd = self.backend.app.switch_model(model);
        for effect in cmd.into_effects() {
            self.backend.perform(effect);
        }
        self.sync_entries();
        cx.notify();
    }

    /// Put the caret back in the text box.
    ///
    /// A `Button` takes the focus when it is clicked, which the bare `div` it
    /// replaced never did. So every control that ends an interaction hands the
    /// focus back, and what is typed next lands in the composer instead of in
    /// the button that was pressed.
    fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        self.composer.focus_handle(cx).focus(window);
    }

    fn close_modal(&mut self, cx: &mut Context<Self>) {
        self.backend.app.modal = None;
        cx.notify();
    }

    fn kill_process(&mut self, id: u32, cx: &mut Context<Self>) {
        self.backend.perform(Effect::Kill(id));
        cx.notify();
    }

    fn surface_click(
        &mut self,
        plugin: String,
        name: String,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.backend.perform(Effect::Surface {
            plugin,
            name,
            event: SurfaceEvent::Mouse {
                button: "left".to_owned(),
                // A window has no rows and no columns. Zero is what a graphical
                // host can say honestly; `host` is how a plugin knows to read
                // `target` and not these.
                row: 0,
                column: 0,
                target,
                host: Host::Gui,
            },
        });
        cx.notify();
    }

    fn render_drawer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let open = self.drawer_open;
        let current = self.backend.session_path.as_ref();
        let mut drawer = div()
            .h_full()
            .flex()
            .flex_col()
            .flex_none()
            .w(if open { px(280.) } else { px(52.) })
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .p_2()
            .gap_2()
            .child(
                Button::new("drawer-toggle")
                    .ghost()
                    .w_full()
                    .h(px(36.))
                    .label(if open { "‹  APHID" } else { "☰" })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_drawer(cx))),
            );
        if open {
            drawer = drawer.child(
                Button::new("new-chat")
                    .primary()
                    .w_full()
                    .h(px(38.))
                    .label("＋ New chat")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.new_chat(cx);
                        this.focus_composer(window, cx);
                    })),
            );
            drawer = drawer.child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("SESSIONS"),
            );
            drawer = drawer.child(
                div()
                    .id("session-list")
                    .flex_1()
                    .overflow_scroll()
                    .children(self.sessions.iter().enumerate().map(|(index, summary)| {
                        let active = current.is_some_and(|path| path == &summary.path);
                        let path = summary.path.clone();
                        let disabled = self.backend.app.status.running || self.switching_session;
                        ListItem::new(SharedString::from(format!("session-{index}")))
                            .my_1()
                            .py_2()
                            .rounded_md()
                            .selected(active)
                            // A session cannot be switched mid-run. The row now
                            // refuses the click as well as looking refused; it
                            // used to dim and take it anyway.
                            .disabled(disabled)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_session(Some(path.clone()), cx);
                            }))
                            .child(
                                div().child(summary.header.id.clone()).child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child(format!("{} messages", summary.messages)),
                                ),
                            )
                    })),
            );
        }
        drawer.into_any_element()
    }

    fn render_entry(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(entry) = self.backend.app.scrollback.entries().get(index) else {
            return div().into_any_element();
        };
        // The list draws one entry at a time, so each one carries the padding
        // the transcript used to put around the column as a whole.
        let row = div().w_full().max_w(px(980.)).mx_auto().px_6().py_2();
        match entry {
            Entry::User(text) => row
                .flex()
                .justify_end()
                .child(
                    div()
                        .max_w(px(760.))
                        .px_4()
                        .py_3()
                        .rounded_lg()
                        .bg(rgb(USER))
                        .text_color(rgb(TEXT))
                        .whitespace_normal()
                        .child(text.clone()),
                )
                .into_any_element(),
            Entry::Assistant(text) => row
                .text_color(rgb(TEXT))
                .child(
                    TextView::markdown(
                        SharedString::from(format!("assistant-{index}")),
                        text.clone(),
                        window,
                        cx,
                    )
                    .selectable(true),
                )
                .into_any_element(),
            Entry::Thinking(text) => {
                let expanded = self.expanded_thinking.contains(&index);
                let preview = if expanded {
                    text.clone()
                } else {
                    text.lines().next().unwrap_or("thinking…").to_owned()
                };
                row.child(
                    div()
                        .id(SharedString::from(format!("thinking-{index}")))
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(PANEL))
                        .text_color(rgb(MUTED))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded_thinking.remove(&index) {
                                this.expanded_thinking.insert(index);
                            }
                            this.sync_entries();
                            cx.notify();
                        }))
                        .child(if expanded {
                            "▾ Thinking"
                        } else {
                            "▸ Thinking"
                        })
                        .child(div().mt_1().whitespace_normal().child(preview)),
                )
                .into_any_element()
            }
            Entry::Tool {
                name,
                arguments,
                output,
                state,
                streamed,
                ..
            } => {
                let expanded = self.expanded_tools.contains(&index);
                let state_text = match state {
                    ToolState::Streaming => format!("streaming {streamed} bytes"),
                    ToolState::Running => "running".to_owned(),
                    ToolState::Done => "done".to_owned(),
                    ToolState::Failed => "failed".to_owned(),
                };
                let failed = *state == ToolState::Failed;
                let arguments = arguments.clone();
                let output = output.clone();
                let mut card = div()
                    .id(SharedString::from(format!("tool-{index}")))
                    .w_full()
                    .rounded_md()
                    .border_1()
                    .border_color(if failed { rgb(DANGER) } else { rgb(BORDER) })
                    .bg(rgb(PANEL))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded_tools.remove(&index) {
                            this.expanded_tools.insert(index);
                        }
                        this.sync_entries();
                        cx.notify();
                    }))
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .flex()
                            .justify_between()
                            .child(format!("{} {name}", if expanded { "▾" } else { "▸" }))
                            .child(div().text_xs().text_color(rgb(MUTED)).child(state_text)),
                    );
                if expanded {
                    card = card.child(
                        div()
                            .border_t_1()
                            .border_color(rgb(BORDER))
                            .p_3()
                            .font_family("monospace")
                            .text_sm()
                            .whitespace_normal()
                            .child(arguments)
                            .child(div().mt_2().text_color(rgb(MUTED)).child(output)),
                    );
                }
                row.child(card).into_any_element()
            }
            Entry::Notice(text) => row
                .text_sm()
                .text_color(rgb(MUTED))
                .child(text.clone())
                .into_any_element(),
            Entry::Shell { command, output } => row
                .child(
                    div()
                        .w_full()
                        .rounded_md()
                        .bg(rgb(PANEL))
                        .p_3()
                        .font_family("monospace")
                        .text_sm()
                        .child(format!("$ {command}\n{output}")),
                )
                .into_any_element(),
            Entry::Logo => row
                .text_2xl()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(ACCENT))
                .child("aphid")
                .into_any_element(),
        }
    }

    fn render_surface_widget(
        &self,
        widget: &Widget,
        plugin: &str,
        surface: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match widget {
            Widget::Rows { children } => div()
                .flex()
                .flex_col()
                .gap_2()
                .children(
                    children
                        .iter()
                        .map(|child| self.render_surface_widget(child, plugin, surface, cx)),
                )
                .into_any_element(),
            Widget::Cols { children } => div()
                .flex()
                .gap_2()
                .children(
                    children
                        .iter()
                        .map(|child| self.render_surface_widget(child, plugin, surface, cx)),
                )
                .into_any_element(),
            Widget::Text { text, .. } => div()
                .whitespace_normal()
                .child(text.clone())
                .into_any_element(),
            Widget::List {
                items, selected, ..
            } => div()
                .flex()
                .flex_col()
                .children(items.iter().enumerate().map(|(index, item)| {
                    div()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(if index == *selected {
                            rgb(USER)
                        } else {
                            rgb(PANEL)
                        })
                        .child(item.clone())
                }))
                .into_any_element(),
            Widget::Input {
                text, placeholder, ..
            } => div()
                .border_1()
                .border_color(rgb(BORDER))
                .rounded_md()
                .px_2()
                .py_1()
                .text_color(if text.is_empty() {
                    rgb(MUTED)
                } else {
                    rgb(TEXT)
                })
                .child(if text.is_empty() {
                    placeholder.clone()
                } else {
                    text.clone()
                })
                .into_any_element(),
            Widget::Button { id, label } => {
                let plugin = plugin.to_owned();
                let surface = surface.to_owned();
                let target = id.clone();
                div()
                    .id(SharedString::from(format!(
                        "surface-button-{plugin}-{surface}-{}",
                        id.as_deref().unwrap_or(label)
                    )))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(PANEL_RAISED))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.surface_click(plugin.clone(), surface.clone(), target.clone(), cx);
                    }))
                    .child(label.clone())
                    .into_any_element()
            }
            Widget::Spacer => div().flex_1().into_any_element(),
        }
    }

    fn render_plugin_side(&self, left: bool, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let panels: Vec<&Open> = self
            .backend
            .surfaces
            .iter()
            .filter(|open| {
                matches!(
                    (left, open.side),
                    (true, crate::scripting::Side::Left) | (false, crate::scripting::Side::Right)
                )
            })
            .collect();
        if panels.is_empty() {
            return None;
        }
        Some(
            div()
                .id(if left { "plugin-left" } else { "plugin-right" })
                .w(px(260.))
                .h_full()
                .flex_none()
                .overflow_scroll()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .p_3()
                .children(panels.into_iter().map(|open| {
                    div()
                        .mb_3()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(ACCENT))
                                .child(open.name.clone()),
                        )
                        .child(self.render_surface_widget(
                            &open.widget,
                            &open.plugin,
                            &open.name,
                            cx,
                        ))
                }))
                .into_any_element(),
        )
    }

    fn render_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let modal = self.backend.app.modal.as_ref()?;
        let body =
            match modal {
                Modal::Confirm(confirm) => div()
                    .w(px(520.))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_RAISED))
                    .p_5()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child(format!("Allow {}?", confirm.tool)),
                    )
                    .child(
                        div()
                            .my_3()
                            .text_color(rgb(MUTED))
                            .child(confirm.summary.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("deny").danger().label("Deny").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.answer(Decision::Deny, cx);
                                    this.focus_composer(window, cx);
                                }),
                            ))
                            .child(Button::new("allow").primary().label("Allow").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.answer(Decision::Allow, cx);
                                    this.focus_composer(window, cx);
                                }),
                            ))
                            .child(
                                // The standing answer is the one that is hardest
                                // to take back, so it is the quieter button.
                                Button::new("always")
                                    .primary()
                                    .outline()
                                    .label("Always")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.answer(Decision::AllowAlways, cx);
                                        this.focus_composer(window, cx);
                                    })),
                            ),
                    )
                    .into_any_element(),
                Modal::Models { models, selected } => {
                    div()
                        .w(px(620.))
                        .max_h(px(620.))
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(PANEL_RAISED))
                        .p_4()
                        .child(
                            div()
                                .mb_3()
                                .text_lg()
                                .font_weight(FontWeight::BOLD)
                                .child("Select a model"),
                        )
                        .child(div().id("model-list").overflow_scroll().children(
                            models.iter().enumerate().map(|(index, model)| {
                                let chosen = index == *selected;
                                ListItem::new(SharedString::from(format!("model-{index}")))
                                    .py_2()
                                    .my_1()
                                    .rounded_md()
                                    .selected(chosen)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.choose_model(index, cx);
                                        this.focus_composer(window, cx);
                                    }))
                                    .child(div().child(model.id.to_string()).child(
                                        div().text_xs().text_color(rgb(MUTED)).child(format!(
                                            "{} token context",
                                            model.context_window
                                        )),
                                    ))
                            }),
                        ))
                        .child(
                            Button::new("close-models")
                                .ghost()
                                .mt_3()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_modal(cx);
                                    this.focus_composer(window, cx);
                                })),
                        )
                        .into_any_element()
                }
                Modal::Processes { rows, .. } => {
                    div()
                        .w(px(720.))
                        .max_h(px(620.))
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(PANEL_RAISED))
                        .p_4()
                        .child(
                            div()
                                .mb_3()
                                .text_lg()
                                .font_weight(FontWeight::BOLD)
                                .child("Processes"),
                        )
                        .child(div().id("process-list").overflow_scroll().children(
                            rows.iter().map(|process| {
                                let id = process.id;
                                let running = process.running();
                                ListItem::new(SharedString::from(format!("process-{id}")))
                                    .py_2()
                                    .my_1()
                                    .rounded_md()
                                    // Only a running process can be stopped, so
                                    // only a running row takes a click.
                                    .disabled(!running)
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.kill_process(id, cx)
                                        }),
                                    )
                                    .child(
                                        div()
                                            .child(format!(
                                                "#{} {} · {:?} · {} bytes",
                                                process.id,
                                                process.origin,
                                                process.status,
                                                process.bytes
                                            ))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(MUTED))
                                                    .child(process.command.clone()),
                                            ),
                                    )
                            }),
                        ))
                        .child(
                            Button::new("close-processes")
                                .ghost()
                                .mt_3()
                                .label("Close · click a running process to stop it")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_modal(cx);
                                    this.focus_composer(window, cx);
                                })),
                        )
                        .into_any_element()
                }
            };
        Some(
            div()
                .absolute()
                .inset_0()
                .bg(rgba(0x00000099))
                .flex()
                .items_center()
                .justify_center()
                .child(body)
                .into_any_element(),
        )
    }
}

impl Focusable for DesktopView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DesktopView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = &self.backend.app.status;
        let status_text = format!(
            "{}{} · {}/{} tokens · ${:.4}",
            status.model,
            status
                .thinking
                .as_ref()
                .map_or_else(String::new, |level| format!(" ({level})")),
            status.context_used(),
            status.context_window,
            status.total.cost.total
        );
        let transcript = list(
            self.entries.clone(),
            cx.processor(|this, index: usize, window, cx| this.render_entry(index, window, cx)),
        )
        .flex_1()
        .min_h_0();

        let mut content = div()
            .relative()
            .size_full()
            .flex()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .font_family("system-ui")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_new_line))
            .child(self.render_drawer(cx));
        if let Some(left) = self.render_plugin_side(true, cx) {
            content = content.child(left);
        }

        let running = status.running;
        let main = div()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(48.))
                    .flex_none()
                    .px_5()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.workspace.root().display().to_string()),
                    )
                    .child(div().text_sm().text_color(rgb(MUTED)).child(status_text)),
            )
            .child(transcript)
            .child(
                div().flex_none().px_6().pb_5().child(
                    div()
                        .w_full()
                        .max_w(px(980.))
                        .mx_auto()
                        .rounded_lg()
                        .border_1()
                        .border_color(if running { rgb(0x9a8038) } else { rgb(BORDER) })
                        .bg(rgb(PANEL_RAISED))
                        .p_3()
                        .flex()
                        .gap_3()
                        .items_end()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&self.composer).appearance(false)),
                        )
                        .child(
                            Button::new("send")
                                .map(|button| {
                                    if running {
                                        button.danger()
                                    } else {
                                        button.primary()
                                    }
                                })
                                .w(px(38.))
                                .h(px(38.))
                                .label(if running { "■" } else { "↑" })
                                .tooltip(if running { "Stop the run" } else { "Send" })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if this.backend.app.status.running {
                                        this.backend.perform(Effect::Cancel);
                                        cx.notify();
                                    } else {
                                        this.send(window, cx);
                                    }
                                    this.focus_composer(window, cx);
                                })),
                        ),
                ),
            );
        content = content.child(main);
        if let Some(right) = self.render_plugin_side(false, cx) {
            content = content.child(right);
        }
        if let Some(modal) = self.render_modal(cx) {
            content = content.child(modal);
        }
        content
    }
}

/// What an entry looks like to the list that measures it.
///
/// Only the things that can change an entry's height go in: the kind, the
/// length of each text it draws, and whether it is open. Lengths and not the
/// text itself, because this runs for every entry on every message, and hashing
/// a whole conversation on each streamed chunk would cost more than the
/// measuring it saves.
fn fingerprint(entry: &Entry, tool_open: bool, thinking_open: bool) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(entry).hash(&mut hasher);
    match entry {
        Entry::User(text) | Entry::Assistant(text) | Entry::Notice(text) => {
            text.len().hash(&mut hasher);
        }
        Entry::Thinking(text) => {
            text.len().hash(&mut hasher);
            thinking_open.hash(&mut hasher);
        }
        Entry::Tool {
            name,
            arguments,
            output,
            state,
            streamed,
            details,
        } => {
            name.len().hash(&mut hasher);
            arguments.len().hash(&mut hasher);
            output.len().hash(&mut hasher);
            std::mem::discriminant(state).hash(&mut hasher);
            streamed.hash(&mut hasher);
            details.is_some().hash(&mut hasher);
            tool_open.hash(&mut hasher);
        }
        Entry::Shell { command, output } => {
            command.len().hash(&mut hasher);
            output.len().hash(&mut hasher);
        }
        Entry::Logo => {}
    }
    hasher.finish()
}

/// Start the desktop interface.
///
/// # Errors
///
/// Returns an error when the async runtime, application window, or session
/// cannot start.
pub fn run(options: Options, resume: Option<PathBuf>, confirm: bool) -> Result<(), String> {
    let workspace = options.workspace.clone();
    let config = GuiConfig::capture(&options);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the GUI runtime: {error}"))?;
    let (backend, receiver) = runtime.block_on(bootstrap(options, resume, confirm))?;
    let runtime_handle = runtime.handle().clone();
    let open_error = Arc::new(std::sync::Mutex::new(None::<String>));
    let reported = Arc::clone(&open_error);

    Application::new().run(move |cx: &mut App| {
        // Before any window: the components read one theme out of a global, so
        // nothing draws in the colors of aphid until this has run.
        theme::init(cx);
        cx.bind_keys([
            gpui::KeyBinding::new("escape", CancelRun, Some(CONTEXT)),
            gpui::KeyBinding::new("shift-enter", NewLine, Some(CONTEXT)),
        ]);
        let bounds = Bounds::centered(None, size(px(1240.), px(820.)), cx);
        match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("aphid code".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|cx| {
                    DesktopView::new(
                        backend,
                        config,
                        runtime_handle,
                        confirm,
                        workspace,
                        window,
                        cx,
                    )
                });
                let focus = view.read(cx).focus.clone();
                window.focus(&focus);
                view.update(cx, |view, cx| view.attach_receiver(receiver, cx));
                // The first layer of the window has to be a `Root`. It is not
                // decoration: the text box reaches for it when it takes focus,
                // and `Root::read` panics when the layer is anything else.
                cx.new(|cx| Root::new(view, window, cx))
            },
        ) {
            Ok(_) => cx.activate(true),
            Err(error) => {
                if let Ok(mut slot) = reported.lock() {
                    *slot = Some(format!("could not open the GUI window: {error}"));
                }
                cx.quit();
            }
        }
    });

    open_error
        .lock()
        .map_err(|_| "could not read the GUI startup result".to_owned())?
        .take()
        .map_or(Ok(()), Err)
}
