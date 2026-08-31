//! The one thread that calls into a script.
//!
//! Rhai runs wherever it is called from, and it was called from four places at
//! once: a slash command and a surface event on the terminal's loop thread, a
//! notice from wherever a notice came from, and `on_tick` on a blocking pool.
//! Two of those could run at the same moment in the same plugin's engine, both
//! reading its state, changing it and writing it back — and one of the two
//! writes was lost.
//!
//! So every call into a script goes through here, and here is one thread. A
//! tick and a panel redraw cannot interleave because there is nowhere for them
//! to interleave. It is the same shape as [`Worker`](super::worker::Worker): a
//! plain thread outside every runtime, jobs in on a channel, results out.
//!
//! Nothing here waits for an answer. A caller that blocked on a script would
//! be back to holding the loop while a plugin thinks; instead the answer is
//! reported and arrives as a message.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use aphid_agent::rt::Bus;

use crate::events::{Notice, Tick};
use crate::registries::Registries;

use super::command::Action;
use super::host::PluginHost;
use super::surface::{Side, SurfaceAction, SurfaceEvent, SurfaceRender};
use super::widget::Widget;

/// What a host was asked to do.
pub enum Job {
    /// Run every plugin's `on_tick`.
    Tick,
    /// Tell the plugins what the user was shown.
    Notice(String),
    /// Run a plugin's slash command.
    Command { name: String, args: String },
    /// Deliver one UI event to a surface.
    Surface {
        plugin: String,
        name: String,
        event: SurfaceEvent,
    },
    /// Re-render the surfaces whose plugin state has moved on.
    Refresh,
    /// Write every plugin's state back to disk.
    Flush,
}

impl Job {
    /// Whether two of these waiting in the queue are worth no more than one.
    ///
    /// A tick that has not run yet is not made more true by asking again, and
    /// a redraw always draws the latest state whenever it happens.
    fn coalesces(&self) -> bool {
        matches!(self, Self::Tick | Self::Refresh)
    }

    fn same_kind(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Tick, Self::Tick) | (Self::Refresh, Self::Refresh)
        )
    }
}

/// One open surface and the widget tree it came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    pub plugin: String,
    pub name: String,
    pub side: Side,
    pub interactive: bool,
    pub widget: Widget,
}

/// What a finished job produced.
pub enum Report {
    Command(Vec<Action>),
    /// A surface handled an event. `None` when no surface by that name is
    /// open any more.
    Surface {
        plugin: String,
        name: String,
        actions: Option<Vec<SurfaceAction>>,
    },
    /// Every open surface, after a redraw.
    Surfaces(Vec<Open>),
}

/// A handle to the script thread.
///
/// The `Sender` is behind a `Mutex` for the same reason the worker's is:
/// `mpsc::Sender` is `Send` but not `Sync`, and this handle is shared.
pub struct PluginHub {
    jobs: Mutex<Option<Sender<Job>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PluginHub {
    /// Start the thread.
    ///
    /// `report` is called on that thread with whatever a job produced. It must
    /// not block — sending on a channel is what it is for.
    #[must_use]
    pub fn spawn(
        host: Arc<PluginHost>,
        bus: Arc<Bus>,
        registries: Arc<Registries>,
        report: impl Fn(Report) + Send + 'static,
    ) -> Self {
        let (jobs, inbox) = channel::<Job>();

        let thread = std::thread::Builder::new()
            .name("aphid-plugin-hub".to_owned())
            .spawn(move || serve(&inbox, &host, &bus, &registries, &report))
            // A host that cannot start its thread still runs: the plugins go
            // quiet rather than the session refusing to open.
            .ok();

        Self {
            jobs: Mutex::new(Some(jobs)),
            thread,
        }
    }

    /// Queue a job. Nothing waits for it.
    pub fn send(&self, job: Job) {
        if let Ok(jobs) = self.jobs.lock()
            && let Some(jobs) = jobs.as_ref()
        {
            let _ = jobs.send(job);
        }
    }

    /// Finish what is queued, then stop.
    ///
    /// Waited for rather than abandoned: whoever calls this is about to run
    /// the session-end hooks, and those must not be the fifth caller racing
    /// this thread on the way out.
    pub fn stop(mut self) {
        // Dropping the sender is what ends the loop, once it has drained.
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.take();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl std::fmt::Debug for PluginHub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PluginHub")
    }
}

fn serve(
    inbox: &Receiver<Job>,
    host: &Arc<PluginHost>,
    bus: &Arc<Bus>,
    registries: &Arc<Registries>,
    report: &impl Fn(Report),
) {
    let mut panels = Panels::default();

    while let Ok(job) = inbox.recv() {
        for job in drain(job, inbox) {
            run(job, host, bus, registries, &mut panels, report);
        }
    }
}

/// Everything waiting right now.
fn drain(first: Job, inbox: &Receiver<Job>) -> Vec<Job> {
    let mut queue = vec![first];
    while let Ok(job) = inbox.try_recv() {
        queue.push(job);
    }
    coalesce(queue)
}

/// Throw away the repeats of the kinds that a repeat adds nothing to.
///
/// A tick that has not run yet is not made more true by asking again, and a
/// redraw always draws the state it finds. A slow tick used to queue behind
/// itself and then run twice over; the flag the host kept for that is what
/// this replaces.
///
/// The **last** of each is what is kept, not the first. A batch can hold a
/// redraw, then a command that changes what a panel shows, then another
/// redraw; keeping the first would draw the panel as it was and then throw
/// away the ask that would have drawn it as it is.
fn coalesce(queue: Vec<Job>) -> Vec<Job> {
    let mut kept: Vec<Job> = Vec::with_capacity(queue.len());
    for job in queue.into_iter().rev() {
        if job.coalesces() && kept.iter().any(|later| later.same_kind(&job)) {
            continue;
        }
        kept.push(job);
    }
    kept.reverse();
    kept
}

fn run(
    job: Job,
    host: &Arc<PluginHost>,
    bus: &Arc<Bus>,
    registries: &Arc<Registries>,
    panels: &mut Panels,
    report: &impl Fn(Report),
) {
    match job {
        Job::Tick => {
            bus.emit(&mut Tick);
            // And the surfaces that asked to hear it, each as a step of its
            // own loop rather than a listener that reaches around it.
            for open in super::surface::registered(registries.surfaces()) {
                if !open.spec.tick {
                    continue;
                }
                let (plugin, name) = (open.plugin.clone(), open.name.clone());
                let actions = host.surface_event(&open.spec, &plugin, SurfaceEvent::Tick);
                if actions.as_ref().is_some_and(|actions| !actions.is_empty()) {
                    report(Report::Surface {
                        plugin,
                        name,
                        actions,
                    });
                }
            }
        }
        Job::Notice(text) => bus.emit(&mut Notice(text)),
        Job::Command { name, args } => {
            if let Some(actions) = host.run_command(registries.commands(), &name, &args) {
                report(Report::Command(actions));
                // A command can change the model a surface projects. Report
                // that projection in the same turn instead of leaving the GUI
                // to discover it on the next quarter-second refresh.
                if let Some(open) = panels.render(host, registries) {
                    report(Report::Surfaces(open));
                }
            }
        }
        Job::Surface {
            plugin,
            name,
            event,
        } => {
            let actions = super::surface::registered(registries.surfaces())
                .into_iter()
                .find(|open| open.plugin == plugin && open.name == name)
                .and_then(|open| host.surface_event(&open.spec, &plugin, event));
            report(Report::Surface {
                plugin,
                name,
                actions,
            });
        }
        // Nothing is said when nothing moved: the tick asks for a redraw
        // every quarter second, and answering it with an unchanged copy of
        // every widget tree would be a copy and a message for nothing.
        Job::Refresh => {
            if let Some(open) = panels.render(host, registries) {
                report(Report::Surfaces(open));
            }
        }
        Job::Flush => host.flush(),
    }
}

/// What the surfaces last came to, kept against each plugin's state version.
///
/// A plugin whose state has not moved is not asked to render again. The
/// version is read on this thread, which is the only thread that changes it,
/// so the check cannot be racing anything.
#[derive(Default)]
struct Panels {
    cache: HashMap<(String, String), Cached>,
    /// Whether anything has been reported yet. The first answer is always
    /// worth giving, even when every surface is closed.
    reported: bool,
}

struct Cached {
    version: u64,
    open: Option<Open>,
}

impl Panels {
    /// What the surfaces come to, or `None` when none of them has moved.
    fn render(
        &mut self,
        host: &Arc<PluginHost>,
        registries: &Arc<Registries>,
    ) -> Option<Vec<Open>> {
        let mut open = Vec::new();
        let mut moved = !self.reported;

        for surface in super::surface::registered(registries.surfaces()) {
            let super::surface::Placement::Side(side) = surface.placement;
            let key = (surface.plugin.clone(), surface.name.clone());
            let version = host.state_version(&surface.plugin).unwrap_or(0);

            let stale = self
                .cache
                .get(&key)
                .is_none_or(|cached| cached.version != version);
            if stale {
                moved = true;
                let drawn = draw(host, &surface, side);
                self.cache.insert(
                    key.clone(),
                    Cached {
                        version,
                        open: drawn,
                    },
                );
            }

            if let Some(cached) = self.cache.get(&key).and_then(|c| c.open.clone()) {
                open.push(cached);
            }
        }

        if !moved {
            return None;
        }
        self.reported = true;
        Some(open)
    }
}

/// Ask one surface to render itself.
fn draw(
    host: &Arc<PluginHost>,
    surface: &super::surface::RegisteredSurface,
    side: Side,
) -> Option<Open> {
    let widget = match host.render_surface(&surface.spec, &surface.plugin)? {
        // Unit closes the surface; nothing to show.
        SurfaceRender::Closed => return None,
        SurfaceRender::Widget(widget) => widget,
        // A surface that failed stays open with its reason in it, rather than
        // vanishing and leaving the reader to wonder where it went.
        SurfaceRender::Failed(error) => Widget::Text {
            id: None,
            text: format!("render failed: {error}"),
        },
    };

    Some(Open {
        plugin: surface.plugin.clone(),
        name: surface.name.clone(),
        side,
        interactive: surface.interactive,
        widget,
    })
}

#[cfg(test)]
mod tests {
    use super::{Job, coalesce};

    fn kinds(jobs: &[Job]) -> Vec<&'static str> {
        jobs.iter()
            .map(|job| match job {
                Job::Tick => "tick",
                Job::Notice(_) => "notice",
                Job::Command { .. } => "command",
                Job::Surface { .. } => "surface",
                Job::Refresh => "refresh",
                Job::Flush => "flush",
            })
            .collect()
    }

    #[test]
    fn a_tick_asked_for_twice_runs_once() {
        let kept = coalesce(vec![Job::Tick, Job::Tick, Job::Tick]);
        assert_eq!(kinds(&kept), ["tick"]);
    }

    #[test]
    fn a_redraw_asked_for_twice_draws_once() {
        let kept = coalesce(vec![Job::Refresh, Job::Refresh]);
        assert_eq!(kinds(&kept), ["refresh"]);
    }

    #[test]
    fn everything_else_is_kept_in_order() {
        // Two notices are two things the user was shown, and two commands are
        // two things they asked for. Neither stands in for the other.
        let kept = coalesce(vec![
            Job::Notice("first".to_owned()),
            Job::Tick,
            Job::Notice("second".to_owned()),
            Job::Tick,
            Job::Command {
                name: "poke".to_owned(),
                args: String::new(),
            },
        ]);
        assert_eq!(kinds(&kept), ["notice", "notice", "tick", "command"]);
    }

    #[test]
    fn a_redraw_keeps_the_place_of_the_last_one_asked_for() {
        // The one that matters is the one after the command: it is what draws
        // the panel as the command left it.
        let kept = coalesce(vec![
            Job::Refresh,
            Job::Command {
                name: "bump".to_owned(),
                args: String::new(),
            },
            Job::Refresh,
        ]);
        assert_eq!(kinds(&kept), ["command", "refresh"]);
    }
}
