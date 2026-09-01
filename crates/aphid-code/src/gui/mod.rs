//! The GPUI desktop front end.

mod markdown;
pub mod theme;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use aphid_agent::rt::Component;
use gpui::{
    App, Application, Bounds, Context, FocusHandle, Focusable, FontWeight, HighlightStyle,
    IntoElement, Render, ScrollHandle, SharedString, StyledText, Window, WindowBounds,
    WindowOptions, div, img, prelude::*, px, rgb, rgba, size,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::events::{Session, SessionEnd, SessionStart};
use crate::harness::{self, HarnessOptions};
use crate::plugins::permissions::{Decision, PermissionGate, Permissions};
use crate::plugins::scripts;
use crate::scripting::{
    Action as PluginAction, Job as PluginJob, Open, PluginHub, Report as PluginReport,
    SurfaceEvent, Widget,
};
use crate::session::{self, Summary, sessions_dir};
use crate::tui::app::{Executor, PluginLoader};
use crate::tui::modal::Modal;
use crate::tui::runtime::{Answers, Hub, channel};
use crate::tui::scrollback::{Entry, ToolState};
use crate::tui::{App as CodeApp, Effect, Msg, UiComponent, UiConfirmer, UiSink};
use crate::{HarnessOptions as Options, Workspace};
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
    composer: String,
    focus: FocusHandle,
    scroll: ScrollHandle,
    expanded_tools: HashSet<usize>,
    expanded_thinking: HashSet<usize>,
    loaded_images: HashSet<String>,
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
        cx: &mut Context<Self>,
    ) -> Self {
        let sessions = session::list_for(&sessions_dir(), workspace.root());
        Self {
            backend,
            config,
            runtime,
            confirm,
            workspace,
            sessions,
            drawer_open: true,
            composer: String::new(),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            expanded_tools: HashSet::new(),
            expanded_thinking: HashSet::new(),
            loaded_images: HashSet::new(),
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
                        view.scroll.scroll_to_bottom();
                    }
                    Err(error) => view.backend.app.scrollback.push_notice(error),
                }
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
        self.scroll.scroll_to_bottom();
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

    fn on_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if key == "enter" && !modifiers.shift {
            self.send(cx);
            cx.stop_propagation();
            return;
        }
        if key == "enter" {
            self.composer.push('\n');
        } else if key == "backspace" {
            self.composer.pop();
        } else if key == "escape" {
            if self.backend.app.status.running {
                self.backend.perform(Effect::Cancel);
            } else {
                self.composer.clear();
            }
        } else if modifiers.platform && key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.composer.push_str(&text);
            }
        } else if let Some(text) = &event.keystroke.key_char
            && !modifiers.platform
            && !modifiers.control
        {
            self.composer.push_str(text);
        } else {
            return;
        }
        window.focus(&self.focus);
        cx.notify();
        cx.stop_propagation();
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let line = self.composer.trim().to_owned();
        if line.is_empty() {
            return;
        }
        self.composer.clear();
        if let Some(command) = line.strip_prefix('!') {
            self.backend
                .perform(Effect::Bang(command.trim().to_owned()));
        } else {
            self.backend.submit(line);
        }
        if self.backend.app.quitting() {
            cx.quit();
            return;
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
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

    fn choose_model(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(Modal::Models { selected, .. }) = &mut self.backend.app.modal {
            *selected = index;
        }
        let cmd = self
            .backend
            .app
            .update(Msg::Key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )));
        for effect in cmd.into_effects() {
            self.backend.perform(effect);
        }
        cx.notify();
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
                row: 0,
                column: 0,
                target,
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
                div()
                    .id("drawer-toggle")
                    .h(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(PANEL_RAISED)))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_drawer(cx)))
                    .child(if open { "‹  APHID" } else { "☰" }),
            );
        if open {
            drawer = drawer.child(
                div()
                    .id("new-chat")
                    .h(px(38.))
                    .px_3()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .bg(rgb(ACCENT))
                    .text_color(rgb(0x0d150d))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.new_chat(cx)))
                    .child("＋ New chat"),
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
                        div()
                            .id(SharedString::from(format!("session-{index}")))
                            .my_1()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(if active { rgb(USER) } else { rgb(PANEL) })
                            .text_color(if active { rgb(ACCENT) } else { rgb(TEXT) })
                            .opacity(if disabled { 0.55 } else { 1.0 })
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_session(Some(path.clone()), cx);
                            }))
                            .child(summary.header.id.clone())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(format!("{} messages", summary.messages)),
                            )
                    })),
            );
        }
        drawer.into_any_element()
    }

    fn render_entry(
        &self,
        index: usize,
        entry: &Entry,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match entry {
            Entry::User(text) => div()
                .w_full()
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
            Entry::Assistant(text) => self.render_markdown(text, cx),
            Entry::Thinking(text) => {
                let expanded = self.expanded_thinking.contains(&index);
                let preview = if expanded {
                    text.clone()
                } else {
                    text.lines().next().unwrap_or("thinking…").to_owned()
                };
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
                        cx.notify();
                    }))
                    .child(if expanded {
                        "▾ Thinking"
                    } else {
                        "▸ Thinking"
                    })
                    .child(div().mt_1().whitespace_normal().child(preview))
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
                let mut card = div()
                    .id(SharedString::from(format!("tool-{index}")))
                    .w_full()
                    .rounded_md()
                    .border_1()
                    .border_color(if *state == ToolState::Failed {
                        rgb(DANGER)
                    } else {
                        rgb(BORDER)
                    })
                    .bg(rgb(PANEL))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded_tools.remove(&index) {
                            this.expanded_tools.insert(index);
                        }
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
                            .child(arguments.clone())
                            .child(div().mt_2().text_color(rgb(MUTED)).child(output.clone())),
                    );
                }
                card.into_any_element()
            }
            Entry::Notice(text) => div()
                .w_full()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(text.clone())
                .into_any_element(),
            Entry::Shell { command, output } => div()
                .w_full()
                .rounded_md()
                .bg(rgb(PANEL))
                .p_3()
                .font_family("monospace")
                .text_sm()
                .child(format!("$ {command}\n{output}"))
                .into_any_element(),
            Entry::Logo => div()
                .text_2xl()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(ACCENT))
                .child("aphid")
                .into_any_element(),
        }
    }

    fn render_markdown(&self, source: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut root = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .text_color(rgb(TEXT));
        for (index, block) in markdown::parse(source).into_iter().enumerate() {
            let element = match block {
                markdown::Block::Text(text) => div()
                    .w_full()
                    .whitespace_normal()
                    .child(text)
                    .into_any_element(),
                markdown::Block::Heading { level, text } => div()
                    .mt_2()
                    .text_size(px(25. - f32::from(level) * 1.8))
                    .font_weight(FontWeight::BOLD)
                    .child(text)
                    .into_any_element(),
                markdown::Block::Code { language, text } => div()
                    .id(SharedString::from(format!("code-{index}")))
                    .w_full()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(0x0c1014))
                    .p_3()
                    .font_family("monospace")
                    .text_sm()
                    .overflow_scroll()
                    .child(highlight_code(&language, &text))
                    .into_any_element(),
                markdown::Block::Quote(text) => div()
                    .border_l_2()
                    .border_color(rgb(ACCENT))
                    .pl_3()
                    .text_color(rgb(MUTED))
                    .child(text)
                    .into_any_element(),
                markdown::Block::ListItem { depth, text } => div()
                    .pl(px(12. * depth as f32))
                    .child(format!("• {text}"))
                    .into_any_element(),
                markdown::Block::Rule => {
                    div().h(px(1.)).w_full().bg(rgb(BORDER)).into_any_element()
                }
                markdown::Block::Table(text) => div()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .p_3()
                    .font_family("monospace")
                    .child(text)
                    .into_any_element(),
                markdown::Block::Image { url, alt } => {
                    if self.loaded_images.contains(&url) {
                        div()
                            .id(SharedString::from(format!("image-{index}-{url}")))
                            .max_w_full()
                            .child(img(url).max_w_full().max_h(px(420.)))
                            .into_any_element()
                    } else {
                        let load = url.clone();
                        div()
                            .id(SharedString::from(format!("image-gate-{index}-{url}")))
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .p_3()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(PANEL_RAISED)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.loaded_images.insert(load.clone());
                                cx.notify();
                            }))
                            .child(format!("Load remote image: {alt}"))
                            .child(div().text_xs().text_color(rgb(MUTED)).child(url))
                            .into_any_element()
                    }
                }
                markdown::Block::Link { url, text } => {
                    let target = url.clone();
                    div()
                        .id(SharedString::from(format!("link-{index}-{url}")))
                        .text_color(rgb(0x75a7e8))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| cx.open_url(&target))
                        .child(text)
                        .into_any_element()
                }
            };
            root = root.child(element);
        }
        root.into_any_element()
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
                            .child(action_button(
                                "Deny",
                                DANGER,
                                cx.listener(|this, _, _, cx| {
                                    this.answer(Decision::Deny, cx);
                                }),
                            ))
                            .child(action_button(
                                "Allow",
                                ACCENT,
                                cx.listener(|this, _, _, cx| {
                                    this.answer(Decision::Allow, cx);
                                }),
                            ))
                            .child(action_button(
                                "Always",
                                ACCENT,
                                cx.listener(|this, _, _, cx| {
                                    this.answer(Decision::AllowAlways, cx);
                                }),
                            )),
                    )
                    .into_any_element(),
                Modal::Models { models, selected } => div()
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
                            div()
                                .id(SharedString::from(format!("model-{index}")))
                                .px_3()
                                .py_2()
                                .my_1()
                                .rounded_md()
                                .bg(if chosen { rgb(USER) } else { rgb(PANEL) })
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.choose_model(index, cx);
                                }))
                                .child(model.id.to_string())
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(MUTED))
                                        .child(format!("{} token context", model.context_window)),
                                )
                        }),
                    ))
                    .child(
                        div()
                            .id("close-models")
                            .mt_3()
                            .text_color(rgb(MUTED))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx)))
                            .child("Cancel"),
                    )
                    .into_any_element(),
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
                                div()
                                    .id(SharedString::from(format!("process-{id}")))
                                    .px_3()
                                    .py_2()
                                    .my_1()
                                    .rounded_md()
                                    .bg(rgb(PANEL))
                                    .child(format!(
                                        "#{} {} · {:?} · {} bytes",
                                        process.id, process.origin, process.status, process.bytes
                                    ))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(MUTED))
                                            .child(process.command.clone()),
                                    )
                                    .when(running, |row| {
                                        row.cursor_pointer().on_click(cx.listener(
                                            move |this, _, _, cx| this.kill_process(id, cx),
                                        ))
                                    })
                            }),
                        ))
                        .child(
                            div()
                                .id("close-processes")
                                .mt_3()
                                .text_color(rgb(MUTED))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx)))
                                .child("Close · click a running process to stop it"),
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
        let entries = self
            .backend
            .app
            .scrollback
            .entries()
            .iter()
            .enumerate()
            .map(|(index, entry)| self.render_entry(index, entry, cx))
            .collect::<Vec<_>>();

        let mut content = div()
            .relative()
            .size_full()
            .flex()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .font_family("system-ui")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(self.render_drawer(cx));
        if let Some(left) = self.render_plugin_side(true, cx) {
            content = content.child(left);
        }

        let composer_text = if self.composer.is_empty() {
            "Ask aphid, or type /help…".to_owned()
        } else {
            self.composer.clone()
        };
        let composer_color = if self.composer.is_empty() {
            rgb(MUTED)
        } else {
            rgb(TEXT)
        };
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
            .child(
                div()
                    .id("transcript")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .w_full()
                            .max_w(px(980.))
                            .mx_auto()
                            .px_6()
                            .py_5()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .children(entries),
                    ),
            )
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
                                .id("composer")
                                .flex_1()
                                .min_h(px(44.))
                                .max_h(px(120.))
                                .overflow_scroll()
                                .whitespace_normal()
                                .text_color(composer_color)
                                .child(composer_text),
                        )
                        .child(
                            div()
                                .id("send")
                                .size(px(38.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .bg(if running { rgb(DANGER) } else { rgb(ACCENT) })
                                .text_color(rgb(0x0d150d))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if this.backend.app.status.running {
                                        this.backend.perform(Effect::Cancel);
                                        cx.notify();
                                    } else {
                                        this.send(cx);
                                    }
                                }))
                                .child(if running { "■" } else { "↑" }),
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

fn action_button(
    label: &'static str,
    color: u32,
    handler: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(label)
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(color))
        .text_color(rgb(0x0d150d))
        .cursor_pointer()
        .on_click(handler)
        .child(label)
}

fn highlight_code(language: &str, code: &str) -> StyledText {
    static SYNTAXES: std::sync::OnceLock<SyntaxSet> = std::sync::OnceLock::new();
    static THEMES: std::sync::OnceLock<ThemeSet> = std::sync::OnceLock::new();
    let syntaxes = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
    let themes = THEMES.get_or_init(ThemeSet::load_defaults);
    let syntax = syntaxes
        .find_syntax_by_token(language)
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, &themes.themes["base16-ocean.dark"]);
    let mut offset = 0usize;
    let mut highlights = Vec::new();
    for line in LinesWithEndings::from(code) {
        if let Ok(ranges) = highlighter.highlight_line(line, syntaxes) {
            for (style, text) in ranges {
                let end = offset + text.len();
                let color = style.foreground;
                let packed = (u32::from(color.r) << 24)
                    | (u32::from(color.g) << 16)
                    | (u32::from(color.b) << 8)
                    | u32::from(color.a);
                highlights.push((offset..end, HighlightStyle::color(rgba(packed).into())));
                offset = end;
            }
        }
    }
    StyledText::new(code.to_owned()).with_highlights(highlights)
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
                    DesktopView::new(backend, config, runtime_handle, confirm, workspace, cx)
                });
                let focus = view.read(cx).focus.clone();
                window.focus(&focus);
                view.update(cx, |view, cx| view.attach_receiver(receiver, cx));
                view
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
