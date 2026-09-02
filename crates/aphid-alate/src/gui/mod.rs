//! A window on a running alate.
//!
//! `aphid alate attach` gives an alate a face in a terminal. This gives it one
//! on the desktop: a console that drops from the top of the screen, or a column
//! against its edge, that stays up while you work and shows what the agent is
//! doing between prompts.
//!
//! It is a **client of the gateway** and nothing more. It holds no agent, no
//! memory and no session of its own; [`crate::gateway::Client`] and the wire
//! are the whole of what it shares with the daemon, which is the interface the
//! documentation already promises to anything that can write a line of JSON.
//! Closing the window does not stop the alate.
//!
//! There is one window for the machine, not one for each alate. A second
//! `aphid alate gui` finds the first through `$APHID_HOME/gui.sock` and tells
//! it to come forward; naming another alate points the same window at it.

pub mod balloon;
pub mod config;
pub mod control;
pub mod emote;
pub mod model;
pub mod place;
pub mod render;
pub mod tray;
pub mod window;

use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{
    App, Application, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement,
    ListAlignment, ListState, Render, SharedString, Window, div, img, list, prelude::*, px,
    relative, rgb, rgba,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::list::ListItem;
use gpui_component::text::TextView;
use gpui_component::{Disableable, Root};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use aphid_code::gui::theme::{
    self, ACCENT, BACKGROUND, BORDER, DANGER, MUTED, PANEL, PANEL_RAISED, TEXT, USER,
};

use crate::gateway::wire::{Answer, Envelope, Frame as Wire, Request, Risk};
use crate::gateway::{Client, Reader, Writer};
use crate::home::{DEFAULT_NAME, Home};

use balloon::Balloon;
use config::{Config, Familiar, Mode};
use control::{Command, Control, Reply};
use emote::Mood;
use model::{Entry, Link, Model, ToolState};

gpui::actions!(
    alate,
    [
        /// Stop the run in flight, or close what is on top of the transcript.
        Cancel,
        /// Put a line break in the text box instead of sending it.
        NewLine
    ]
);

/// The key context of the window. The text box has a deeper one, so its own
/// bindings are tried first and these are what reaches here.
const CONTEXT: &str = "Alate";

/// How long to wait before the first reconnection, in seconds. It doubles up to
/// [`BACKOFF_MAX`].
const BACKOFF_START: u64 = 1;
const BACKOFF_MAX: u64 = 30;

/// How tall the creature's band is at the foot of the companion.
///
/// The same 300 the desktop companion this follows gave its own pane. It is a
/// fixed height and not a share of the window, so the log above it grows and
/// shrinks with the screen while the creature stays the size it was drawn for.
const PANE_HEIGHT: f32 = 300.;
/// How much of the companion's band the balloon may take, leaving the rest to
/// the alate.
const BAND_BALLOON: f32 = 130.;
/// How much of the console the balloon may take. More, because there the
/// creature and what it says are the whole of what is on screen.
const CONSOLE_BALLOON: f32 = 220.;

/// How many frames to ask the desktop to place the window on.
///
/// Once is not enough and there is no event to wait for: X11 lists a window
/// when the window manager takes it over, which can be after the first frame is
/// drawn. Asking on the first few costs one connection each and settles it.
const PLACE_FRAMES: u8 = 3;

/// What reaches the window from somewhere that is not the keyboard.
enum Msg {
    /// A connection is up, and this is what to send requests on.
    Opened(UnboundedSender<Request>),
    /// The daemon said something.
    Wire(Box<Envelope>),
    /// There is no connection, and why.
    Down(String),
    /// Somebody ran `aphid alate gui …` while this window was already open.
    Control(Command),
}

/// Everything the window draws.
struct AlateView {
    model: Model,
    /// The text box. It owns its cursor, its selection and its marked text,
    /// which is what makes a dead key compose: `Keystroke.key_char` gives the
    /// key and not the character, so nothing built on key events alone types
    /// `ação`.
    composer: Entity<InputState>,
    focus: FocusHandle,
    /// The transcript, measured entry by entry and anchored at the newest.
    entries: ListState,
    /// What each entry looked like when it was last measured.
    fingerprints: Vec<u64>,
    expanded_tools: HashSet<usize>,
    expanded_thinking: HashSet<usize>,
    /// Where requests go while a connection is up.
    outbox: Option<UnboundedSender<Request>>,
    /// Which connection the messages arriving belong to. Pointing the window at
    /// another alate bumps it, and the old connection's messages are dropped.
    generation: u64,
    /// Whether this window has ever been connected. What makes a reconnection
    /// say so, rather than a first connection announcing itself.
    connected_once: bool,
    runtime: tokio::runtime::Handle,
    config: Config,
    config_path: PathBuf,
    /// Whether the transcript and the text box are on screen. Always true in
    /// companion mode, which has only one height.
    expanded: bool,
    /// Whether the session list is on top of the transcript.
    picking: bool,
    /// Whether the tray's menu is on top of it. There is nowhere else for it on
    /// a panel that draws no menus of its own.
    menu: bool,
    /// Why there is no connection, when there is none.
    down: Option<String>,
    /// What carries the text box's events here.
    ///
    /// Kept, because it has to be made again for each window: a subscription
    /// holds the handle of the window it was made in, and one made in a window
    /// that has since closed drops its events without a word.
    typing: Option<gpui::Subscription>,
    /// The alate this window watches, as the control socket reports it to the
    /// next `aphid alate gui`.
    watching: std::sync::Arc<std::sync::Mutex<String>>,
    /// Whether a `aphid alate run` was started from here and has not answered
    /// yet, so the button cannot be pressed twice.
    waking: bool,
    /// How many more frames to ask the desktop to place this window on. Reset
    /// whenever a new window is opened, which is what a mode change is.
    placing: u8,
    /// The expansion the window on screen is currently sized for. `None` until
    /// the first frame, which is what makes the console take its own size even
    /// if nothing has been toggled.
    sized: Option<bool>,
    /// A mode asked for by the control socket, which has no window to work
    /// with. Taken on the next frame, which has.
    mode_wanted: Option<Mode>,
    /// Whether somebody asked for the window to come forward. Also taken on the
    /// next frame: raising a window is something only a window can do.
    raise_wanted: bool,
    /// What the creature is feeling, and what it was feeling before that.
    mood: Mood,
    /// The last thing it said, over its head.
    balloon: Balloon,
    /// The thread that draws it, and the image it last drew.
    body: render::Body,
    /// When the window opened. Everything the shaders animate against is
    /// measured from here rather than from the wall clock.
    opened: std::time::Instant,
    /// The icon in the tray, for as long as there is a window behind it.
    _tray: Option<tray::Tray>,
    /// Where the tray's picks are sent, and what the macOS menu bar is drained
    /// into on each beat.
    orders: UnboundedSender<Msg>,
}

/// What the window is opened with.
///
/// One value rather than a handful of parameters, because every one of them is
/// decided in [`run`] and carried straight through to the view.
struct Start {
    /// The alate to watch.
    instance: String,
    config: Config,
    config_path: PathBuf,
    /// The runtime the connection and the tray run on. GPUI owns the thread.
    runtime: tokio::runtime::Handle,
    /// The name the control socket answers a `Ping` with, shared because the
    /// socket is served on another thread.
    watching: std::sync::Arc<std::sync::Mutex<String>>,
    /// Where everything that is not the keyboard arrives.
    orders: UnboundedSender<Msg>,
}

impl AlateView {
    fn new(start: Start, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Start {
            instance,
            config,
            config_path,
            runtime,
            watching,
            orders,
        } = start;
        let composer = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(1, 6)
                .soft_wrap(true)
                .placeholder("Say something, or /sessions…")
        });
        let expanded = config.mode == Mode::Companion;
        let (width, height) = render::size_of(config.familiar);
        let body = render::Body::start(config.familiar, width, height);
        // A desktop with no tray of either kind is worth saying once, in the
        // window, rather than leaving somebody to wonder where their icon went.
        let mut model = Model::new(&instance);
        let tray = match tray::start(orders.clone(), &runtime) {
            Ok(tray) => Some(tray),
            Err(reason) => {
                model.note(format!("no tray icon: {reason}"));
                None
            }
        };
        Self {
            model,
            composer,
            focus: cx.focus_handle(),
            entries: ListState::new(0, ListAlignment::Bottom, px(600.)),
            fingerprints: Vec::new(),
            expanded_tools: HashSet::new(),
            expanded_thinking: HashSet::new(),
            outbox: None,
            generation: 0,
            connected_once: false,
            runtime,
            config,
            config_path,
            expanded,
            picking: false,
            menu: false,
            down: None,
            watching,
            waking: false,
            placing: PLACE_FRAMES,
            sized: None,
            mode_wanted: None,
            raise_wanted: false,
            typing: None,
            mood: Mood::default(),
            balloon: Balloon::default(),
            body,
            opened: std::time::Instant::now(),
            _tray: tray,
            orders,
        }
    }

    /// Listen to the text box, in this window.
    ///
    /// Called once for each window the view is drawn in. Replacing the stored
    /// subscription drops the one before it, which belonged to a window that is
    /// closing.
    fn watch_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.typing = Some(cx.subscribe_in(&self.composer, window, Self::on_composer));
    }

    /// Open a connection to the alate the model names, and keep it open.
    fn connect(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.outbox = None;

        let Ok(home) = Home::open(&self.model.instance) else {
            self.down = Some(format!("{:?} cannot name an alate", self.model.instance));
            self.model.link = Link::Asleep;
            cx.notify();
            return;
        };
        let socket = home.socket();
        let (sender, receiver) = unbounded_channel();
        self.runtime.spawn(supervise(socket, sender));
        self.drain(generation, receiver, cx);
        cx.notify();
    }

    /// Turn what the connection task sends into calls on this view.
    fn drain(
        &mut self,
        generation: u64,
        mut receiver: UnboundedReceiver<Msg>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |weak, cx| {
            while let Some(msg) = receiver.recv().await {
                let alive = weak.update(cx, |view, cx| view.receive(generation, msg, cx));
                // The window is gone, or this connection is the old one.
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Answer the control socket and the tray, for as long as the window lives.
    ///
    /// Separate from [`Self::drain`], and deliberately not counted by the
    /// connection's generation: pointing the window at another alate opens a
    /// new connection and retires the old one, and a control channel retired
    /// with it would leave the socket and the tray talking to nobody.
    fn attend(&mut self, mut receiver: UnboundedReceiver<Msg>, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            while let Some(msg) = receiver.recv().await {
                let alive = weak.update(cx, |view, cx| match msg {
                    Msg::Control(command) => view.controlled(command, cx),
                    // Nothing else is sent on this channel.
                    _ => true,
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Apply one message. `false` ends the task that is feeding them.
    fn receive(&mut self, generation: u64, msg: Msg, cx: &mut Context<Self>) -> bool {
        if generation != self.generation {
            return false;
        }
        match msg {
            Msg::Opened(outbox) => {
                self.outbox = Some(outbox);
                self.down = None;
                self.waking = false;
                if self.connected_once {
                    // The daemon opens a session for each connection, so what
                    // follows is a new conversation. Saying so beats drawing
                    // the next reply under the last one as though nothing had
                    // happened.
                    self.model.reset();
                    self.model.note("reconnected — this is a new session");
                }
                self.connected_once = true;
                self.mood.awake();
            }
            Msg::Wire(envelope) => {
                self.mood.arrived(&envelope.frame, self.age());
                self.speak(&envelope.frame);
                self.model.arrived(*envelope);
            }
            Msg::Down(reason) => {
                self.outbox = None;
                self.model.link = Link::Asleep;
                self.down = Some(reason);
                self.mood.asleep();
            }
            Msg::Control(command) => return self.controlled(command, cx),
        }
        self.sync_entries();
        cx.notify();
        true
    }

    /// Do what another `aphid alate gui` asked for.
    fn controlled(&mut self, command: Command, cx: &mut Context<Self>) -> bool {
        match command {
            Command::Ping => {}
            Command::Show => {
                self.expanded = true;
                self.raise_wanted = true;
            }
            // The companion has one shape. Collapsing it would leave a
            // full-height column with a bar at the top of it and nothing
            // under that, which is not a smaller window but an empty one.
            Command::Toggle => {
                if self.config.mode == Mode::Console {
                    self.expanded = !self.expanded;
                }
            }
            Command::Mode => self.mode_wanted = Some(self.config.mode.toggled()),
            Command::Instance { name } => self.point_at(name, cx),
            Command::Familiar { name } => self.wear(&name, cx),
            Command::Menu => {
                self.menu = true;
                self.expanded = true;
                self.raise_wanted = true;
            }
            Command::Quit => {
                cx.quit();
                return false;
            }
        }
        cx.notify();
        true
    }

    /// Watch a different alate, on the same window.
    fn point_at(&mut self, name: String, cx: &mut Context<Self>) {
        if name == self.model.instance {
            return;
        }
        self.model = Model::new(&name);
        self.balloon.clear();
        self.connected_once = false;
        self.fingerprints.clear();
        self.entries.reset(0);
        if let Ok(mut watching) = self.watching.lock() {
            name.clone_into(&mut watching);
        }
        self.config.instance = Some(name);
        self.save_config();
        self.connect(cx);
    }

    /// Draw the creature as another familiar.
    ///
    /// A Blade context belongs to the thread that made it, so this is a new
    /// thread and a new context rather than a switch inside one.
    fn wear(&mut self, name: &str, cx: &mut Context<Self>) {
        let familiar = match name {
            "sap" => Familiar::Sap,
            "drift" => Familiar::Drift,
            // A name from a newer aphid, or a typo on the socket. The creature
            // on screen is not worth a complaint.
            _ => return,
        };
        if familiar == self.config.familiar {
            return;
        }
        self.config.familiar = familiar;
        self.save_config();
        // Put the old context down before building another, while there is
        // still an application around for it to be put down under.
        self.body.stop();
        let (width, height) = render::size_of(familiar);
        self.body = render::Body::start(familiar, width, height);
        cx.notify();
    }

    /// Change which window this is.
    ///
    /// A mode is a different window, not a different layout: GPUI fixes a
    /// window's origin when it is created, so the one on screen has to close
    /// and another open. Nothing about the connection is touched — it belongs
    /// to this view, which outlives the window that draws it.
    fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if mode == self.config.mode {
            return;
        }
        self.config.mode = mode;
        self.expanded = mode == Mode::Companion || self.expanded;
        self.save_config();
        self.reopen(window, cx);
    }

    /// Draw this view in a new window, and close the one it was in.
    ///
    /// The new window is opened **before** the old one is closed, so there is
    /// never a moment with none: the view belongs to the application and not to
    /// the window, and so does the connection it holds — which is the whole
    /// reason changing mode does not end the session.
    fn reopen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bounds = window::bounds(self.config.mode, self.expanded, window::screen(cx));
        let options = window::options(self.config.mode, bounds);
        let view = cx.entity();
        let focus = self.composer.focus_handle(cx);
        self.placing = PLACE_FRAMES;
        self.sized = None;

        // Opening a window draws its first frame there and then, and that frame
        // renders this view. So this cannot run while the view is borrowed —
        // and it always would be, since every way of asking for it arrives
        // through a handler that holds it. `Window::defer` hands back an `App`
        // and no view at all, which is exactly what is needed: it runs at the
        // end of the effect cycle, once the view is back in the app.
        window.defer(cx, move |window, cx| {
            if cx
                .open_window(options, {
                    let view = view.clone();
                    move |window, cx| {
                        // The text box moves to the new window, so what listens
                        // to it has to move as well.
                        view.update(cx, |view, cx| view.watch_composer(window, cx));
                        window.focus(&focus);
                        cx.new(|cx| Root::new(view, window, cx))
                    }
                })
                .is_ok()
            {
                // Second, so that there is never a moment with no window: GPUI
                // stops when the last one goes.
                window.remove_window();
            }
        });
    }

    fn save_config(&self) {
        // A window that cannot remember where it was is still a window.
        let _ = self.config.save(&self.config_path);
    }

    /// Start the alate this window is pointed at.
    ///
    /// The documentation says that putting an alate in the background is the
    /// system's job and not the agent's. **The window is the exception**, and
    /// deliberately: it is already a process with a long life, and a companion
    /// that can only tell you to go and open a terminal is not company. The
    /// child is put in a process group of its own, so it is not taken down by
    /// what takes this window down.
    fn wake(&mut self, cx: &mut Context<Self>) {
        if self.waking {
            return;
        }
        let Ok(binary) = std::env::current_exe() else {
            self.down = Some("cannot find the aphid binary to start it with".to_owned());
            return;
        };
        let mut command = std::process::Command::new(binary);
        command
            .arg("alate")
            .arg("run")
            .arg("--name")
            .arg(&self.model.instance)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        match command.spawn() {
            Ok(_) => {
                self.waking = true;
                self.down = Some(format!("starting {}…", self.model.instance));
            }
            Err(error) => self.down = Some(format!("could not start it: {error}")),
        }
        cx.notify();
    }

    /// Ask the daemon for something, if there is one to ask.
    fn ask(&mut self, requests: Vec<Request>) {
        let Some(outbox) = &self.outbox else { return };
        for request in requests {
            let _ = outbox.send(request);
        }
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

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let line = self
            .composer
            .read(cx)
            .value()
            .trim_end_matches('\n')
            .trim()
            .to_owned();
        self.composer.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        if line.is_empty() {
            return;
        }
        let requests = self.model.submit(&line);
        // `/sessions` is worth showing as well as asking for.
        if line.trim() == "/sessions" {
            self.picking = true;
        }
        self.ask(requests);
        self.sync_entries();
        cx.notify();
    }

    /// Stop the run, or put away whatever is over the transcript.
    fn on_cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu {
            self.menu = false;
        } else if self.picking {
            self.picking = false;
        } else if self.model.confirm.is_some() {
            let requests = self.model.answer(Answer::Deny);
            self.ask(requests);
        } else if self.balloon.text().is_some() {
            self.balloon.dismiss();
        } else if self.model.status.running {
            self.ask(vec![Request::Cancel]);
        } else if self.config.mode == Mode::Console && self.expanded {
            // Nothing to stop: the console gets out of the way instead.
            self.expanded = false;
        }
        cx.notify();
    }

    fn on_new_line(&mut self, _: &NewLine, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |state, cx| {
            state.insert("\n", window, cx);
        });
        cx.notify();
    }

    /// Move the balloon along with the reply.
    ///
    /// The same frames the log is built from, read for the one thing the
    /// creature is meant to say out loud: the reply itself. Thinking is not in
    /// it — that is what the face is for — and neither is a tool.
    fn speak(&mut self, frame: &Wire) {
        match frame {
            Wire::TurnStarted => self.balloon.begin(),
            Wire::Text { text } => self.balloon.append(text),
            Wire::RunEnded { error: None, .. } => self.balloon.finish(),
            Wire::RunEnded {
                error: Some(error), ..
            } => self.balloon.show(error.clone()),
            Wire::HistoryStart { .. } => self.balloon.clear(),
            _ => {}
        }
    }

    /// Seconds since the window opened.
    ///
    /// The creature is animated against this and not against a wall clock, so
    /// that nothing it does jumps when the system clock is set.
    fn age(&self) -> f64 {
        self.opened.elapsed().as_secs_f64()
    }

    /// How long until the next frame of the creature is worth drawing.
    ///
    /// Thirty a second while something is happening, ten while nothing is.
    fn beat(&self) -> std::time::Duration {
        if self.model.status.running || self.model.filling() {
            render::FAST
        } else {
            render::SLOW
        }
    }

    /// Repaint on the creature's rhythm, for as long as the window is open.
    fn animate(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            loop {
                let beat = match weak.update(cx, |view, cx| {
                    cx.notify();
                    view.beat()
                }) {
                    Ok(beat) => beat,
                    // The window is gone.
                    Err(_) => return,
                };
                gpui::Timer::after(beat).await;
            }
        })
        .detach();
    }

    /// Put the caret back in the text box, which a clicked button takes.
    fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        self.composer.focus_handle(cx).focus(window);
    }

    fn answer(&mut self, decision: Answer, window: &mut Window, cx: &mut Context<Self>) {
        let requests = self.model.answer(decision);
        self.ask(requests);
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn watch(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let requests = self.model.watch(&id);
        self.ask(requests);
        self.picking = false;
        self.sync_entries();
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// Tell the list what changed since the last frame.
    ///
    /// A transcript grows at the end, but it does not only grow: a tool result
    /// lands in a card drawn several entries ago, and opening a card changes
    /// its height. So each entry carries a fingerprint of what can change its
    /// size, and the ones that moved are spliced — which is what makes the list
    /// measure those again and keep every other height.
    fn sync_entries(&mut self) {
        let fresh: Vec<u64> = self
            .model
            .pane()
            .entries()
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

        // A transcript that shrank is a different conversation: a replay
        // arrived, or the pane was cleared. Nothing measured is worth keeping.
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
}

/// The drawing.
impl AlateView {
    /// The status bar, which is the whole window when the console is collapsed.
    fn render_bar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let status = &self.model.status;
        let state = if self.model.link != Link::Connected {
            "asleep".to_owned()
        } else if self.model.status.running {
            "working".to_owned()
        } else {
            "idle".to_owned()
        };
        let tokens = if status.context_window == 0 {
            String::new()
        } else {
            format!(" · {}/{}", status.context_used(), status.context_window)
        };
        div()
            .h(px(56.))
            .flex_none()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            // Only where there is no pane below. Collapsed, the glyph in the
            // bar is the whole of the creature, as the pill was in the
            // companion this follows; expanded, or in the companion's band, it
            // would be a second smaller copy of what is already on screen.
            .when(self.config.mode == Mode::Console && !self.expanded, |bar| {
                bar.child(self.render_familiar())
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.model.instance.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("{state}{tokens}")),
                    ),
            )
            .child(
                Button::new("sessions")
                    .ghost()
                    .label("⧉")
                    .tooltip("Sessions")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.picking = !this.picking;
                        if this.picking {
                            this.ask(vec![Request::Sessions]);
                        }
                        this.focus_composer(window, cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("mode")
                    .ghost()
                    .label(self.config.mode.label())
                    .tooltip("Console or companion")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_mode(this.config.mode.toggled(), window, cx);
                    })),
            )
            .when(self.config.mode == Mode::Console, |bar| {
                bar.child(
                    Button::new("expand")
                        .ghost()
                        .label(if self.expanded { "▴" } else { "▾" })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.expanded = !this.expanded;
                            this.focus_composer(window, cx);
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    /// The creature.
    ///
    /// With no device to draw on there is a still glyph in its place and, when
    /// the window is open far enough to read one, the reason why. The panel is
    /// an ornament and the gateway client is the function: refusing to open a
    /// window because a shader would not compile has the two the wrong way
    /// round.
    fn render_familiar(&self) -> gpui::AnyElement {
        let size = if self.expanded { 44. } else { 34. };
        let panel = div().w(px(size)).h(px(size)).flex_none().rounded_md();
        if let Some(image) = self.body.image() {
            return panel
                .child(img(image).w(px(size)).h(px(size)))
                .into_any_element();
        }
        let mark = match self.config.familiar {
            Familiar::Sap => "sap",
            Familiar::Drift => "drf",
        };
        // A button so that the reason can be hung on it: the still glyph on its
        // own says the creature is missing and not why.
        panel
            .child(
                Button::new("familiar")
                    .ghost()
                    .w(px(size))
                    .h(px(size))
                    .label(mark)
                    .tooltip(SharedString::from(
                        self.body
                            .trouble()
                            .unwrap_or("the alate, as it is drawn")
                            .to_owned(),
                    )),
            )
            .into_any_element()
    }

    /// The creature, and what it last said.
    ///
    /// Two shapes of the same thing. In the companion it is a band at the foot
    /// of the window, under the log, with a height of its own. In the console
    /// it is the whole of what is on screen: there is no log there, so the
    /// creature and its balloon are what the console is for.
    ///
    /// The balloon is above the creature rather than over it. The companion
    /// this follows drew it into the same GL pass and laid it across the pane;
    /// here they are two elements in a column, so a long answer takes room from
    /// the creature instead of covering its face.
    fn render_pane(&self, band: bool) -> gpui::AnyElement {
        let mut pane = div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .p_2()
            .bg(rgb(BACKGROUND));
        pane = if band {
            pane.h(px(PANE_HEIGHT))
                .flex_none()
                .border_t_1()
                .border_color(rgb(BORDER))
        } else {
            pane.flex_1().min_h_0()
        };

        if let Some(said) = self.balloon.text() {
            let tail = if self.balloon.streaming() { "▍" } else { "" };
            pane = pane.child(
                div()
                    .id("balloon")
                    .flex_none()
                    .max_w(relative(0.9))
                    .max_h(px(if band { BAND_BALLOON } else { CONSOLE_BALLOON }))
                    .overflow_y_scroll()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(rgb(PANEL_RAISED))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .text_color(rgb(TEXT))
                    .whitespace_normal()
                    .child(format!("{said}{tail}")),
            );
        }

        // `img` fits by containing, so the creature takes the height it is
        // given and keeps its shape whatever the window does to the pane —
        // which under a tiling window manager is anything at all.
        pane = if let Some(image) = self.body.image() {
            pane.child(div().flex_1().min_h_0().child(img(image).size_full()))
        } else {
            pane.child(
                div().flex_none().text_xs().text_color(rgb(MUTED)).child(
                    self.body
                        .trouble()
                        .unwrap_or("the alate is not being drawn")
                        .to_owned(),
                ),
            )
        };
        pane.into_any_element()
    }

    fn render_entry(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(entry) = self.model.pane().entries().get(index).cloned() else {
            return div().into_any_element();
        };
        let row = div().w_full().px_3().py_1();
        match entry {
            Entry::User(text) => row
                .flex()
                .justify_end()
                .child(
                    div()
                        .max_w(px(320.))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(rgb(USER))
                        .whitespace_normal()
                        .child(text),
                )
                .into_any_element(),
            Entry::Assistant(text) => row
                .child(
                    TextView::markdown(
                        SharedString::from(format!("assistant-{index}")),
                        text,
                        window,
                        cx,
                    )
                    .selectable(true),
                )
                .into_any_element(),
            Entry::Thinking(text) => {
                let open = self.expanded_thinking.contains(&index);
                let body = if open {
                    text
                } else {
                    text.lines().next().unwrap_or("thinking…").to_owned()
                };
                row.child(
                    div()
                        .id(SharedString::from(format!("thinking-{index}")))
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(PANEL))
                        .text_color(rgb(MUTED))
                        .text_xs()
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded_thinking.remove(&index) {
                                this.expanded_thinking.insert(index);
                            }
                            this.sync_entries();
                            cx.notify();
                        }))
                        .child(if open { "▾ Thinking" } else { "▸ Thinking" })
                        .child(div().mt_1().whitespace_normal().child(body)),
                )
                .into_any_element()
            }
            Entry::Tool(tool) => {
                let open = self.expanded_tools.contains(&index);
                let (mark, color) = match tool.state {
                    ToolState::Streaming => ("◌", MUTED),
                    ToolState::Running => ("●", ACCENT),
                    ToolState::Done => ("✓", ACCENT),
                    ToolState::Failed => ("✗", DANGER),
                };
                // The arguments as a value when they parsed, which is the point
                // of keeping them as one.
                let summary = tool.arguments.as_ref().map_or_else(
                    || tool.raw.clone(),
                    |json| {
                        json.as_object().map_or_else(
                            || json.to_string(),
                            |fields| {
                                fields
                                    .iter()
                                    .map(|(key, value)| format!("{key}: {value}"))
                                    .collect::<Vec<_>>()
                                    .join("  ")
                            },
                        )
                    },
                );
                let head = if tool.state == ToolState::Streaming {
                    format!("{mark} {} · {} bytes", tool.name, tool.streamed)
                } else {
                    format!("{mark} {}", tool.name)
                };
                let mut card = div()
                    .id(SharedString::from(format!("tool-{index}")))
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .text_xs()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded_tools.remove(&index) {
                            this.expanded_tools.insert(index);
                        }
                        this.sync_entries();
                        cx.notify();
                    }))
                    .child(div().text_color(rgb(color)).child(head))
                    .child(
                        div()
                            .text_color(rgb(MUTED))
                            .truncate()
                            .child(summary.clone()),
                    );
                if open && !tool.output.is_empty() {
                    card = card.child(
                        div()
                            .mt_1()
                            .whitespace_normal()
                            .text_color(rgb(if tool.state == ToolState::Failed {
                                DANGER
                            } else {
                                TEXT
                            }))
                            .child(tool.output.clone()),
                    );
                }
                row.child(card).into_any_element()
            }
            Entry::Notice(text) => row
                .child(div().text_xs().text_color(rgb(MUTED)).child(text))
                .into_any_element(),
            Entry::Heartbeat { at, note } => row
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(format!("woke at {at}"))
                        .child(div().whitespace_normal().child(note)),
                )
                .into_any_element(),
            Entry::Session(text) => row
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(format!("── {text} ──")),
                )
                .into_any_element(),
        }
    }

    /// The sessions there are, to pick one from.
    fn render_sessions(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let current = self.model.current().to_owned();
        let rows: Vec<_> = self
            .model
            .sessions
            .live
            .iter()
            .chain(self.model.sessions.stored.iter())
            .cloned()
            .collect();
        let mut panel =
            div()
                .absolute()
                .inset_0()
                .bg(rgba(0x000000cc))
                .p_3()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .mb_2()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("Sessions"))
                        .child(Button::new("close-sessions").ghost().label("✕").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.picking = false;
                                this.focus_composer(window, cx);
                                cx.notify();
                            }),
                        )),
                );
        if rows.is_empty() {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("none yet — the daemon answers in a moment"),
            );
        }
        panel
            .child(
                div()
                    .id("session-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .children(rows.into_iter().enumerate().map(|(index, info)| {
                        let id = info.id.clone();
                        ListItem::new(SharedString::from(format!("session-{index}")))
                            .py_2()
                            .my_1()
                            .rounded_md()
                            .selected(info.id == current)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.watch(id.clone(), window, cx);
                            }))
                            .child(div().child(div().child(info.id.clone())).child(
                                div().text_xs().text_color(rgb(MUTED)).child(format!(
                                    "{} · {}{}",
                                    info.kind,
                                    info.started,
                                    if info.running { " · running" } else { "" }
                                )),
                            ))
                    })),
            )
            .into_any_element()
    }

    /// The tray's menu, in this window.
    ///
    /// A panel that speaks XEmbed adopts a window and knows nothing else about
    /// it — there is no menu on that side to hang items from. So the right
    /// button brings this window forward and opens the menu here, where there
    /// are already buttons to draw it with.
    fn render_menu(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut list = div()
            .id("tray-menu")
            .w_full()
            .max_w(px(320.))
            .max_h(relative(0.9))
            .overflow_y_scroll()
            .rounded_lg()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_RAISED))
            .p_2();
        for row in tray::rows() {
            list = match row {
                tray::Row::Separator => list.child(div().my_1().h(px(1.)).w_full().bg(rgb(BORDER))),
                tray::Row::One(choice) => list.child(self.render_choice(&choice, cx)),
                tray::Row::Group { label, choices } => {
                    let mut group = list.child(
                        div()
                            .mt_2()
                            .px_2()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(label),
                    );
                    for choice in &choices {
                        group = group.child(self.render_choice(choice, cx));
                    }
                    group
                }
            };
        }
        div()
            .absolute()
            .inset_0()
            .bg(rgba(0x000000cc))
            .flex()
            .items_center()
            .justify_center()
            .p_3()
            .child(list)
            .into_any_element()
    }

    /// One thing the menu offers, as a row that sends its command.
    fn render_choice(&self, choice: &tray::Choice, cx: &mut Context<Self>) -> gpui::AnyElement {
        let command = choice.command.clone();
        ListItem::new(SharedString::from(format!("menu-{}", choice.label)))
            .py_2()
            .my_1()
            .rounded_md()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.menu = false;
                // Through the same channel the socket and the icon use, so
                // there is one path into every one of these.
                let _ = this.orders.send(Msg::Control(command.clone()));
                this.focus_composer(window, cx);
                cx.notify();
            }))
            .child(choice.label.clone())
            .into_any_element()
    }

    /// The permission question, over everything else.
    fn render_confirm(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let confirm = self.model.confirm.as_ref()?;
        let risk = match confirm.risk {
            Risk::Read => "reads",
            Risk::Mutate => "changes something",
            Risk::Destructive => "destroys something",
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .bg(rgba(0x000000cc))
                .flex()
                .items_center()
                .justify_center()
                .p_3()
                .child(
                    div()
                        .w_full()
                        .max_w(px(420.))
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(PANEL_RAISED))
                        .p_4()
                        .child(
                            div()
                                .font_weight(FontWeight::BOLD)
                                .child(format!("Allow {}?", confirm.tool)),
                        )
                        .child(
                            div()
                                .my_2()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(format!("it {risk}")),
                        )
                        .child(
                            div()
                                .my_2()
                                .whitespace_normal()
                                .child(confirm.summary.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(Button::new("deny").danger().label("Deny").on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.answer(Answer::Deny, window, cx);
                                    }),
                                ))
                                .child(Button::new("allow").primary().label("Allow").on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.answer(Answer::Allow, window, cx);
                                    }),
                                ))
                                .child(
                                    Button::new("always")
                                        .primary()
                                        .outline()
                                        .label("Always")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.answer(Answer::AllowAlways, window, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// What is drawn instead of a transcript when nothing is listening.
    fn render_asleep(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let reason = self
            .down
            .clone()
            .unwrap_or_else(|| format!("{} is not running", self.model.instance));
        div()
            .flex_1()
            .min_h_0()
            .p_4()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(div().text_color(rgb(MUTED)).child(reason))
            .child(
                Button::new("wake")
                    .primary()
                    .label(if self.waking { "Waking…" } else { "Wake it" })
                    .disabled(self.waking)
                    .on_click(cx.listener(|this, _, _, cx| this.wake(cx))),
            )
            .into_any_element()
    }
}

impl Focusable for AlateView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AlateView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(mode) = self.mode_wanted.take() {
            // Opening and closing windows in the middle of a render pass is not
            // a thing to do, so this runs at the end of the effect cycle.
            cx.defer_in(window, move |this, window, cx| {
                this.set_mode(mode, window, cx)
            });
        }
        if std::mem::take(&mut self.raise_wanted) {
            // `App::activate` raises the application, which on a desktop with
            // no such notion raises nothing. This asks the window manager for
            // the window, which is what a person clicking a tray icon meant.
            cx.activate(true);
            window.activate_window();
        }
        // The console grows downwards from an origin that never moves, so
        // showing the transcript is a resize and not a new window. Driven from
        // what is drawn rather than from the click, so that the button, the
        // `Escape` key and the control socket all arrive here.
        if self.sized != Some(self.expanded) {
            self.sized = Some(self.expanded);
            let size = window::size_of(self.config.mode, self.expanded, window::screen(cx));
            window.resize(size);
        }
        if self.placing > 0 {
            self.placing -= 1;
            let bounds = window::bounds(self.config.mode, self.expanded, window::screen(cx));
            place::place(window, bounds);
        }
        // What the menu bar reported since the last frame. Nothing on Linux,
        // where the menu's own callbacks send.
        tray::drain(&self.orders);
        // Take whatever the drawing thread finished, then ask for the next one.
        // Collecting first is what makes the pair one frame behind at worst,
        // rather than a queue that grows when the GPU is slower than the beat.
        self.body.collect(window, cx);
        let now = self.age();
        let listening =
            !self.model.status.running && self.composer.focus_handle(cx).is_focused(window);
        let showing = self.mood.settled(now, listening);
        self.body.ask(
            now as f32,
            showing,
            self.mood.previous(),
            self.mood.blend(now),
        );

        let mut content = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .text_sm()
            .font_family("system-ui")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_new_line))
            .child(self.render_bar(cx));

        if self.expanded {
            let companion = self.config.mode == Mode::Companion;
            if self.model.link != Link::Connected {
                content = content.child(self.render_asleep(cx));
            } else if companion {
                // The log, then the creature, then what you type — the order
                // the companion this follows put them in, and the reason the
                // creature is beside the text box rather than a screen away.
                content = content
                    .child(
                        list(
                            self.entries.clone(),
                            cx.processor(|this, index: usize, window, cx| {
                                this.render_entry(index, window, cx)
                            }),
                        )
                        .flex_1()
                        .min_h_0(),
                    )
                    .child(self.render_pane(true));
            } else {
                // The console has no log, as the companion's own topbar had
                // none: it is the creature and what it is saying, and the log
                // is a mode away.
                content = content.child(self.render_pane(false));
            }
            let running = self.model.status.running;
            content = content.child(
                div().flex_none().p_2().child(
                    div()
                        .w_full()
                        .rounded_lg()
                        .border_1()
                        .border_color(if running { rgb(0x9a8038) } else { rgb(BORDER) })
                        .bg(rgb(PANEL_RAISED))
                        .p_2()
                        .child(Input::new(&self.composer).appearance(false)),
                ),
            );
        }

        if self.picking {
            content = content.child(self.render_sessions(cx));
        }
        if self.menu {
            content = content.child(self.render_menu(cx));
        }
        if let Some(confirm) = self.render_confirm(cx) {
            content = content.child(confirm);
        }
        content
    }
}

/// What an entry looks like to the list that measures it.
///
/// Only what can change an entry's height goes in: the kind, the length of each
/// text it draws, and whether it is open. Lengths and not the text, because
/// this runs for every entry on every frame that arrives.
fn fingerprint(entry: &Entry, tool_open: bool, thinking_open: bool) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(entry).hash(&mut hasher);
    match entry {
        Entry::User(text) | Entry::Assistant(text) | Entry::Notice(text) | Entry::Session(text) => {
            text.len().hash(&mut hasher);
        }
        Entry::Thinking(text) => {
            text.len().hash(&mut hasher);
            thinking_open.hash(&mut hasher);
        }
        Entry::Heartbeat { at, note } => {
            at.len().hash(&mut hasher);
            note.len().hash(&mut hasher);
        }
        Entry::Tool(tool) => {
            tool.name.len().hash(&mut hasher);
            tool.raw.len().hash(&mut hasher);
            tool.output.len().hash(&mut hasher);
            std::mem::discriminant(&tool.state).hash(&mut hasher);
            tool.streamed.hash(&mut hasher);
            tool.details.is_some().hash(&mut hasher);
            tool_open.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Keep a connection to `socket` up for as long as anybody is listening.
///
/// One connection at a time, reopened with a backoff that gives up on nothing:
/// a window left open overnight finds the alate again when it comes back, and
/// an alate that is not running yet costs one connect attempt every half minute.
async fn supervise(socket: PathBuf, out: UnboundedSender<Msg>) {
    let mut delay = BACKOFF_START;
    loop {
        match Client::connect_as(&socket, Some("gui")).await {
            Ok(client) => {
                delay = BACKOFF_START;
                let (reader, writer) = client.split();
                let (sender, receiver) = unbounded_channel();
                if out.send(Msg::Opened(sender)).is_err() {
                    return;
                }
                let pump = tokio::spawn(pump(writer, receiver));
                let ended = follow(reader, &out).await;
                pump.abort();
                if !ended {
                    return;
                }
                if out.send(Msg::Down("the alate hung up".to_owned())).is_err() {
                    return;
                }
            }
            Err(error) => {
                let reason = if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::ConnectionRefused
                {
                    String::new()
                } else {
                    format!(": {error}")
                };
                if out.send(Msg::Down(format!("not running{reason}"))).is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        delay = (delay * 2).min(BACKOFF_MAX);
    }
}

/// Read envelopes until the connection ends. `false` when the window is gone.
async fn follow(mut reader: Reader, out: &UnboundedSender<Msg>) -> bool {
    loop {
        match reader.recv().await {
            Ok(Some(envelope)) => {
                if out.send(Msg::Wire(Box::new(envelope))).is_err() {
                    return false;
                }
            }
            _ => return true,
        }
    }
}

/// Write requests until the connection ends.
async fn pump(mut writer: Writer, mut requests: UnboundedReceiver<Request>) {
    while let Some(request) = requests.recv().await {
        if writer.send(&request).await.is_err() {
            return;
        }
    }
}

/// Open the window, or bring the one that is open forward.
///
/// `name` is the alate to watch. Without one, the window opens on the alate it
/// was last pointed at, and failing that on the default.
///
/// # Errors
///
/// Fails when there is no home directory, when the runtime cannot start, or
/// when the window cannot be opened.
pub fn run(name: Option<String>) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the runtime: {error}"))?;

    let socket = control::socket_path().map_err(|error| error.to_string())?;
    let config_path = control::config_path().map_err(|error| error.to_string())?;
    let mut config = Config::load(&config_path).unwrap_or_default();
    let instance = name
        .or_else(|| config.instance.clone())
        .unwrap_or_else(|| DEFAULT_NAME.to_owned());

    // One window for the machine. A second run is a remote control for the
    // first, which is what makes `aphid alate gui` safe to bind to a key.
    let control = match runtime.block_on(Control::bind(&socket)) {
        Ok(control) => control,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            return runtime.block_on(hand_over(&socket, &instance));
        }
        Err(error) => return Err(format!("{}: {error}", socket.display())),
    };

    config.instance = Some(instance.clone());
    let handle = runtime.handle().clone();
    // What the socket answers a `Ping` with. The window owns which alate it
    // watches, and the socket is served on another thread, so the name is
    // shared rather than asked for.
    let watching = std::sync::Arc::new(std::sync::Mutex::new(instance));
    let reporter = std::sync::Arc::clone(&watching);
    // One channel for everything that is not the keyboard: the control socket,
    // and the tray, which is another client of the same commands.
    let (commands, orders) = unbounded_channel();
    let picks = commands.clone();
    runtime.spawn(control.serve(move |command| {
        let answer = match &command {
            Command::Ping => Reply::Pong {
                instance: reporter.lock().ok().map(|name| name.clone()),
            },
            _ => Reply::Ok,
        };
        if commands.send(Msg::Control(command)).is_err() {
            return Reply::Refused {
                reason: "the window is closing".to_owned(),
            };
        }
        answer
    }));

    let opened = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let reported = std::sync::Arc::clone(&opened);
    let start = config.clone();

    Application::new().run(move |cx: &mut App| {
        // Before any window: the components read one theme out of a global, so
        // nothing draws in the colors of aphid until this has run.
        theme::init(cx);
        cx.bind_keys([
            gpui::KeyBinding::new("escape", Cancel, Some(CONTEXT)),
            gpui::KeyBinding::new("shift-enter", NewLine, Some(CONTEXT)),
        ]);

        let display = cx
            .active_window()
            .and_then(|window| window.update(cx, |_, window, _| window.bounds()).ok())
            .or_else(|| cx.primary_display().map(|display| display.bounds()))
            .unwrap_or(gpui::Bounds {
                origin: gpui::Point {
                    x: px(0.),
                    y: px(0.),
                },
                size: gpui::Size {
                    width: px(1280.),
                    height: px(800.),
                },
            });
        let bounds = window::bounds(start.mode, start.mode == Mode::Companion, display);

        match cx.open_window(window::options(start.mode, bounds), {
            let start = start.clone();
            move |window, cx| {
                let view = cx.new(|cx| {
                    AlateView::new(
                        Start {
                            instance: start
                                .instance
                                .clone()
                                .unwrap_or_else(|| DEFAULT_NAME.to_owned()),
                            config: start,
                            config_path,
                            runtime: handle,
                            watching,
                            orders: picks,
                        },
                        window,
                        cx,
                    )
                });
                // The text box and not the view: keys go where the focus is,
                // and a view holding it means `Enter` reaches the action
                // bindings instead of the composer, so nothing is ever sent
                // until somebody clicks the box. The actions still fire — the
                // box is inside the element that carries the key context, and
                // dispatch walks up from the focus.
                let focus = view.read(cx).composer.focus_handle(cx);
                window.focus(&focus);
                view.update(cx, |view, cx| {
                    view.watch_composer(window, cx);
                    view.connect(cx);
                    view.attend(orders, cx);
                    view.animate(cx);
                });
                // The first layer has to be a `Root`: the text box reaches for
                // it when it takes focus, and `Root::read` panics when the
                // layer is anything else.
                cx.new(|cx| Root::new(view, window, cx))
            }
        }) {
            Ok(_) => cx.activate(true),
            Err(error) => {
                if let Ok(mut slot) = reported.lock() {
                    *slot = Some(format!("could not open the window: {error}"));
                }
                cx.quit();
            }
        }
    });

    opened
        .lock()
        .map_err(|_| "could not read the window's startup result".to_owned())?
        .take()
        .map_or(Ok(()), Err)
}

/// Tell the window that is already open, and stop.
async fn hand_over(socket: &std::path::Path, instance: &str) -> Result<(), String> {
    let point = control::talk(
        socket,
        &Command::Instance {
            name: instance.to_owned(),
        },
    )
    .await;
    if let Err(error) = point {
        return Err(format!("a window is open but will not answer: {error}"));
    }
    control::talk(socket, &Command::Show)
        .await
        .map(|_| ())
        .map_err(|error| format!("a window is open but will not answer: {error}"))
}

/// Say one thing to the window that is open.
///
/// # Errors
///
/// Fails when no window is open, which is what `aphid alate gui toggle` with
/// nothing running should say.
pub async fn control_one(command: Command) -> Result<(), String> {
    let socket = control::socket_path().map_err(|error| error.to_string())?;
    match control::talk(&socket, &command).await {
        Ok(Reply::Refused { reason }) => Err(reason),
        Ok(_) => Ok(()),
        Err(_) => Err("no window is open. `aphid alate gui` opens one".to_owned()),
    }
}
