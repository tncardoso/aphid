//! What the alate is feeling, read off the frames it is already sending.
//!
//! No tool, no frame and no line of the protocol was added for this. The
//! creature is driven by what a client already receives, which is what keeps it
//! an ornament on the window rather than a claim on the daemon.
//!
//! There are two tracks, as in the desktop companion this borrows from. The
//! **base** track follows the run: thinking, talking, finished. The **overlay**
//! track is for the things that interrupt one — a permission question, a tool
//! that failed — and the next `TurnStarted` clears it, because by then the
//! answer has been given and the run has moved on.

use std::time::Duration;

use crate::gateway::wire::Frame;

/// How long the creature looks pleased after a run that worked.
const HAPPY: Duration = Duration::from_secs(2);
/// How long one feeling takes to become the next. The companion this borrows
/// from cut straight from one to the other.
pub const BLEND: Duration = Duration::from_millis(200);

/// One of eight faces.
///
/// The order is the order the shaders branch on, so a new one goes at the end.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(u32)]
pub enum Emote {
    #[default]
    Idle = 0,
    Listening = 1,
    Thinking = 2,
    Talking = 3,
    Happy = 4,
    Sad = 5,
    Surprised = 6,
    Sleeping = 7,
}

impl Emote {
    /// What the shader branches on.
    #[must_use]
    pub fn id(self) -> u32 {
        self as u32
    }
}

/// The two tracks, and when each of them last moved.
#[derive(Clone, Debug)]
pub struct Mood {
    base: Emote,
    /// What a question or a failure put on top, until the next turn starts.
    overlay: Option<Emote>,
    /// When the pleased look runs out, in seconds since the window opened.
    happy_until: Option<f64>,
    /// What was on screen before the current feeling, and when it changed. The
    /// two of them are the crossfade.
    previous: Emote,
    changed: f64,
    /// What [`Mood::settled`] last returned, so a change can be noticed.
    showing: Emote,
}

impl Default for Mood {
    fn default() -> Self {
        Self {
            base: Emote::Idle,
            overlay: None,
            happy_until: None,
            previous: Emote::Idle,
            changed: 0.,
            showing: Emote::Idle,
        }
    }
}

impl Mood {
    /// Read one frame.
    ///
    /// `now` is seconds since the window opened, and not a wall clock: the
    /// creature is animated against the same number the shaders are.
    pub fn arrived(&mut self, frame: &Frame, now: f64) {
        match frame {
            Frame::TurnStarted => {
                // A new turn answers whatever the last one asked, so the
                // overlay goes with it.
                self.overlay = None;
                self.happy_until = None;
                self.base = Emote::Thinking;
            }
            Frame::Text { .. } | Frame::Thinking { .. } => self.base = Emote::Talking,
            Frame::RunEnded { error: None, .. } => {
                self.base = Emote::Happy;
                self.happy_until = Some(now + HAPPY.as_secs_f64());
            }
            Frame::RunEnded { error: Some(_), .. } => {
                self.base = Emote::Sad;
                self.happy_until = None;
            }
            Frame::Confirm { .. } => self.overlay = Some(Emote::Surprised),
            Frame::ToolResult { is_error: true, .. } => self.overlay = Some(Emote::Sad),
            _ => {}
        }
    }

    /// The connection is gone, so the alate is asleep.
    pub fn asleep(&mut self) {
        self.base = Emote::Sleeping;
        self.overlay = None;
        self.happy_until = None;
    }

    /// The connection is back, and nothing has happened on it yet.
    pub fn awake(&mut self) {
        if self.base == Emote::Sleeping {
            self.base = Emote::Idle;
        }
    }

    /// What to draw.
    ///
    /// `listening` is whether the person has the text box focused and nothing
    /// is running — the one input that comes from this side of the socket.
    #[must_use]
    pub fn showing(&self, now: f64, listening: bool) -> Emote {
        if let Some(overlay) = self.overlay {
            return overlay;
        }
        if self.base == Emote::Happy && self.happy_until.is_some_and(|until| now >= until) {
            return if listening {
                Emote::Listening
            } else {
                Emote::Idle
            };
        }
        if self.base == Emote::Idle && listening {
            return Emote::Listening;
        }
        self.base
    }

    /// Take what is showing, and remember it so the crossfade can start.
    ///
    /// Called once for each frame drawn. It is `&mut` because the crossfade is
    /// between what this returned last time and what it returns now, and
    /// nothing else in the window knows when that changed.
    pub fn settled(&mut self, now: f64, listening: bool) -> Emote {
        let showing = self.showing(now, listening);
        if showing != self.showing {
            self.previous = self.showing;
            self.showing = showing;
            self.changed = now;
        }
        showing
    }

    /// What came before, for the shader to fade out of.
    #[must_use]
    pub fn previous(&self) -> Emote {
        self.previous
    }

    /// How far the crossfade has got: 0 at the change, 1 when it is over.
    #[must_use]
    pub fn blend(&self, now: f64) -> f32 {
        let span = BLEND.as_secs_f64();
        let done = (now - self.changed) / span;
        done.clamp(0., 1.) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aphid_core::StopReason;

    fn ended(error: Option<&str>) -> Frame {
        Frame::RunEnded {
            stop: StopReason::Stop,
            turns: 1,
            error: error.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn a_run_thinks_then_talks_then_looks_pleased() {
        let mut mood = Mood::default();
        mood.arrived(&Frame::TurnStarted, 0.);
        assert_eq!(mood.showing(0., false), Emote::Thinking);
        mood.arrived(
            &Frame::Text {
                text: "hello".to_owned(),
            },
            0.5,
        );
        assert_eq!(mood.showing(0.5, false), Emote::Talking);
        mood.arrived(&ended(None), 1.);
        assert_eq!(mood.showing(1., false), Emote::Happy);
    }

    #[test]
    fn the_pleased_look_runs_out_after_two_seconds() {
        let mut mood = Mood::default();
        mood.arrived(&ended(None), 10.);
        assert_eq!(mood.showing(11.9, false), Emote::Happy);
        assert_eq!(mood.showing(12.1, false), Emote::Idle);
        // And into listening rather than idle, if that is where the person is.
        assert_eq!(mood.showing(12.1, true), Emote::Listening);
    }

    #[test]
    fn a_run_that_failed_stays_sad() {
        let mut mood = Mood::default();
        mood.arrived(&ended(Some("no such model")), 0.);
        assert_eq!(mood.showing(60., false), Emote::Sad);
    }

    #[test]
    fn a_question_lays_over_whatever_the_run_was_doing() {
        let mut mood = Mood::default();
        mood.arrived(&Frame::TurnStarted, 0.);
        mood.arrived(
            &Frame::Confirm {
                id: 1,
                tool: "bash".to_owned(),
                summary: "rm -rf /".to_owned(),
                risk: crate::gateway::wire::Risk::Destructive,
            },
            1.,
        );
        assert_eq!(mood.showing(1., false), Emote::Surprised);
        // And the next turn clears it: by then it has been answered.
        mood.arrived(&Frame::TurnStarted, 2.);
        assert_eq!(mood.showing(2., false), Emote::Thinking);
    }

    #[test]
    fn a_tool_that_failed_lays_over_it_too() {
        let mut mood = Mood::default();
        mood.arrived(&Frame::TurnStarted, 0.);
        mood.arrived(
            &Frame::ToolResult {
                id: "c1".to_owned(),
                name: "bash".to_owned(),
                text: "no such file".to_owned(),
                is_error: true,
                details: None,
            },
            1.,
        );
        assert_eq!(mood.showing(1., false), Emote::Sad);
        mood.arrived(&Frame::TurnStarted, 2.);
        assert_eq!(mood.showing(2., false), Emote::Thinking);
    }

    #[test]
    fn a_tool_that_worked_changes_nothing() {
        let mut mood = Mood::default();
        mood.arrived(&Frame::TurnStarted, 0.);
        mood.arrived(
            &Frame::ToolResult {
                id: "c1".to_owned(),
                name: "read".to_owned(),
                text: "fn main() {}".to_owned(),
                is_error: false,
                details: None,
            },
            1.,
        );
        assert_eq!(mood.showing(1., false), Emote::Thinking);
    }

    #[test]
    fn a_lost_connection_puts_it_to_sleep_and_getting_it_back_wakes_it() {
        let mut mood = Mood::default();
        mood.arrived(&Frame::TurnStarted, 0.);
        mood.asleep();
        assert_eq!(mood.showing(0., true), Emote::Sleeping);
        mood.awake();
        assert_eq!(mood.showing(0., false), Emote::Idle);
    }

    #[test]
    fn waking_does_not_overwrite_a_feeling_that_is_not_sleep() {
        let mut mood = Mood::default();
        mood.arrived(&ended(Some("broke")), 0.);
        mood.awake();
        assert_eq!(mood.showing(0., false), Emote::Sad);
    }

    #[test]
    fn one_feeling_fades_into_the_next_over_the_blend() {
        let mut mood = Mood::default();
        assert_eq!(mood.settled(0., false), Emote::Idle);
        mood.arrived(&Frame::TurnStarted, 1.);
        assert_eq!(mood.settled(1., false), Emote::Thinking);
        assert_eq!(mood.previous(), Emote::Idle);
        assert!((mood.blend(1.) - 0.).abs() < f32::EPSILON);
        assert!((mood.blend(1.1) - 0.5).abs() < 0.01);
        assert!((mood.blend(2.) - 1.).abs() < f32::EPSILON);
    }

    #[test]
    fn the_shader_ids_are_the_ones_the_shaders_branch_on() {
        assert_eq!(Emote::Idle.id(), 0);
        assert_eq!(Emote::Sleeping.id(), 7);
    }
}
