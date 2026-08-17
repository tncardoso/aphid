//! The three halves of an app: the pure one, the drawing one, the impure one.

use std::time::Duration;

use ratatui::Frame;

use super::{Cmd, Hub};

/// A repeating wake-up a model asked for.
///
/// Three slots and not a list, because these are the three cadences a terminal
/// wants and a fixed set needs no allocation and no boxed closure. What each
/// one means is the program's own business; the names say how often, not what.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Timer {
    /// The fast one, for something that animates while it happens.
    Frame,
    /// The slow one, for a screen showing elapsed time.
    Poll,
    /// The background one, for work that is not about the screen at all.
    Tick,
}

/// How often a model wants each timer, or `None` for not at all.
///
/// Read after every batch of messages. A timer is rebuilt only when its
/// duration changes, so a model that keeps asking for the same thing keeps the
/// same interval and does not restart it every pass.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Subs {
    pub frame: Option<Duration>,
    pub poll: Option<Duration>,
    pub tick: Option<Duration>,
}

impl Subs {
    #[must_use]
    pub fn of(&self, timer: Timer) -> Option<Duration> {
        match timer {
            Timer::Frame => self.frame,
            Timer::Poll => self.poll,
            Timer::Tick => self.tick,
        }
    }
}

/// The pure half of an app.
///
/// `update` performs no IO, calls no script, spawns no task and touches no
/// terminal. Everything it wants done comes back as a [`Cmd`] for the runtime
/// to interpret. That is what lets a test drive a whole session with no
/// runtime at all.
pub trait Program {
    /// Everything the app reacts to. Plain data: no channels, no handles.
    type Msg;
    /// Everything the app wants done. Plain data, so a test can assert on it.
    type Effect;

    fn update(&mut self, msg: Self::Msg) -> Cmd<Self::Effect>;

    /// The message a fired timer carries. A model that asks for no timers
    /// never sees this.
    fn timer(&self, _timer: Timer) -> Option<Self::Msg> {
        None
    }

    /// The timers this model wants right now.
    fn subs(&self) -> Subs {
        Subs::default()
    }

    /// The runtime stops when this is true.
    fn done(&self) -> bool;
}

/// The half that draws.
///
/// Separate from [`Program`] so a headless driver — an alate daemon — can
/// implement one and not the other.
pub trait Draw: Program {
    /// A scratchpad the runtime owns.
    ///
    /// Everything in it must be derivable from `&self`. It exists only so the
    /// derivation is not repeated at every frame, which means a warm one and a
    /// cold one have to paint the same picture.
    type Cache: Default;

    fn draw(&self, frame: &mut Frame<'_>, cache: &mut Self::Cache);

    /// What the last draw laid out, as a message.
    ///
    /// The one place a drawing may reach back into the model, and it does so
    /// the same way everything else does: by asking. A model that does not
    /// care where things landed leaves this alone.
    fn laid_out(_cache: &Self::Cache) -> Option<Self::Msg> {
        None
    }
}

/// The impure half.
///
/// Owns the agent, the plugin host, the processes and every reply channel —
/// everything a model must not hold. Generic and never `dyn`: one app, one
/// executor, resolved at compile time.
pub trait Effects {
    type Program: Program;

    /// Start one effect.
    ///
    /// **Must not block.** The loop that draws the screen and answers a
    /// permission prompt is the caller; anything slow is spawned or queued and
    /// reports back with messages on `hub`.
    fn perform(
        &mut self,
        effect: <Self::Program as Program>::Effect,
        hub: &Hub<<Self::Program as Program>::Msg>,
    );

    /// Called once, before the first draw.
    fn start(&mut self, _hub: &Hub<<Self::Program as Program>::Msg>) {}

    /// Called once, after the loop ends — however it ended.
    fn stop(&mut self) {}
}
