//! The loop: draw, wait, apply, repeat.

use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Interval, MissedTickBehavior, interval};

use super::{Draw, Effects, Hub, Program, Subs, Timer};

/// How many messages one pass applies before it must draw again.
///
/// A reply arriving as two hundred small deltas becomes one frame rather than
/// two hundred, which is what the old repaint throttle was working around. The
/// cap is there so a stream that never stops cannot starve the screen.
const BATCH: usize = 256;

/// Run `model` until it says it is done.
///
/// Draws first and waits second, so the opening screen is up before anything
/// has happened.
///
/// # Errors
///
/// Fails when the backend cannot draw.
pub async fn run<P, E, B>(
    model: &mut P,
    effects: &mut E,
    terminal: &mut Terminal<B>,
    hub: &Hub<P::Msg>,
    inbox: &mut UnboundedReceiver<P::Msg>,
) -> Result<(), B::Error>
where
    P: Draw,
    E: Effects<Program = P>,
    B: Backend,
{
    let mut cache = P::Cache::default();
    let mut timers = Timers::default();

    effects.start(hub);
    timers.retune(model.subs());

    while !model.done() {
        terminal.draw(|frame| model.draw(frame, &mut cache))?;

        // The one message a drawing may send. Applied before the wait, so what
        // the screen laid out is known by the time a key arrives about it.
        if let Some(msg) = P::laid_out(&cache) {
            apply(model, effects, hub, msg);
        }

        // Taken apart so the three timer branches borrow three different
        // slots: `select!` polls them all, and one `&mut Timers` cannot be
        // handed out three times.
        let Timers {
            frame, poll, tick, ..
        } = &mut timers;

        let woken = tokio::select! {
            msg = inbox.recv() => match msg {
                Some(msg) => Some(msg),
                // Every sender is gone, including the one this loop holds, so
                // nothing can ever arrive again.
                None => break,
            },
            () = fire(frame) => model.timer(Timer::Frame),
            () = fire(poll) => model.timer(Timer::Poll),
            () = fire(tick) => model.timer(Timer::Tick),
        };

        if let Some(msg) = woken {
            apply(model, effects, hub, msg);
        }
        // Whatever else is already waiting goes in the same frame.
        for _ in 0..BATCH {
            let Ok(msg) = inbox.try_recv() else { break };
            apply(model, effects, hub, msg);
        }

        timers.retune(model.subs());
    }

    effects.stop();
    Ok(())
}

fn apply<P, E>(model: &mut P, effects: &mut E, hub: &Hub<P::Msg>, msg: P::Msg)
where
    P: Program,
    E: Effects<Program = P>,
{
    for effect in model.update(msg).into_effects() {
        effects.perform(effect, hub);
    }
}

/// The three intervals, kept in step with what the model asks for.
#[derive(Default)]
struct Timers {
    asked: Subs,
    frame: Option<Interval>,
    poll: Option<Interval>,
    tick: Option<Interval>,
    /// How many intervals have been built. A rebuilt interval starts its
    /// period over, so a test asserts that asking twice builds once.
    #[cfg(test)]
    built: usize,
}

impl Timers {
    /// Match the intervals to `want`, rebuilding only the ones that moved.
    ///
    /// An interval that is rebuilt starts its period over. A model that asks
    /// for the same cadence every pass must therefore keep the same interval,
    /// or a fast loop would postpone the tick for ever.
    fn retune(&mut self, want: Subs) {
        if want == self.asked {
            return;
        }
        for timer in [Timer::Frame, Timer::Poll, Timer::Tick] {
            if want.of(timer) == self.asked.of(timer) {
                continue;
            }
            #[cfg(test)]
            {
                self.built += 1;
            }
            *self.slot(timer) = want.of(timer).map(|period| {
                let mut ticker = interval(period);
                // A slow hook must not leave a burst of owed ticks behind it.
                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                ticker
            });
        }
        self.asked = want;
    }

    fn slot(&mut self, timer: Timer) -> &mut Option<Interval> {
        match timer {
            Timer::Frame => &mut self.frame,
            Timer::Poll => &mut self.poll,
            Timer::Tick => &mut self.tick,
        }
    }
}

/// Wait for one timer, or for ever when the model did not ask for it.
///
/// Never-resolving beats a `select!` guard: the branch keeps the same shape
/// whether or not the timer is armed, and an unarmed one simply never wins.
async fn fire(slot: &mut Option<Interval>) {
    match slot {
        Some(ticker) => {
            ticker.tick().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;

    use super::super::{Cmd, Draw, Effects, Hub, Program, Subs, Timer, hub};
    use super::run;

    /// A model with one number in it, so the loop can be watched without any
    /// of what a real app drags in.
    #[derive(Default)]
    struct Counter {
        count: u32,
        ticks: u32,
        /// Set once the loop has drawn something, by way of `laid_out`.
        width: u16,
        stop: bool,
        /// Every draw the loop made, for the batching test.
        draws: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Msg {
        Add(u32),
        Ticked,
        Drawn(u16),
        Stop,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Effect {
        /// Ask the executor to send `Add(n)` back, so a round trip is visible.
        Echo(u32),
    }

    impl Program for Counter {
        type Msg = Msg;
        type Effect = Effect;

        fn update(&mut self, msg: Msg) -> Cmd<Effect> {
            match msg {
                Msg::Add(n) => {
                    self.count += n;
                    Cmd::none()
                }
                Msg::Ticked => {
                    self.ticks += 1;
                    Cmd::none()
                }
                Msg::Drawn(width) => {
                    self.width = width;
                    Cmd::none()
                }
                Msg::Stop => {
                    self.stop = true;
                    Cmd::none()
                }
            }
        }

        fn timer(&self, timer: Timer) -> Option<Msg> {
            (timer == Timer::Tick).then_some(Msg::Ticked)
        }

        fn subs(&self) -> Subs {
            Subs {
                tick: Some(Duration::from_millis(5)),
                ..Subs::default()
            }
        }

        fn done(&self) -> bool {
            self.stop
        }
    }

    #[derive(Default)]
    struct Cache {
        width: u16,
    }

    impl Draw for Counter {
        type Cache = Cache;

        fn draw(&self, frame: &mut ratatui::Frame<'_>, cache: &mut Cache) {
            self.draws
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            cache.width = frame.area().width;
            frame.render_widget(Paragraph::new(self.count.to_string()), frame.area());
        }

        fn laid_out(cache: &Cache) -> Option<Msg> {
            Some(Msg::Drawn(cache.width))
        }
    }

    /// Answers `Echo` by putting the message back on the hub, which is how a
    /// real executor reports a finished task.
    struct Executor {
        started: bool,
        stopped: bool,
    }

    impl Effects for Executor {
        type Program = Counter;

        fn perform(&mut self, effect: Effect, hub: &Hub<Msg>) {
            let Effect::Echo(n) = effect;
            hub.send(Msg::Add(n));
        }

        fn start(&mut self, _hub: &Hub<Msg>) {
            self.started = true;
        }

        fn stop(&mut self) {
            self.stopped = true;
        }
    }

    fn terminal() -> ratatui::Terminal<TestBackend> {
        ratatui::Terminal::new(TestBackend::new(20, 3)).expect("terminal")
    }

    #[tokio::test]
    async fn messages_are_applied_in_order_and_stop_the_loop() {
        let (hub, mut inbox) = hub::channel();
        let mut model = Counter::default();
        let mut effects = Executor {
            started: false,
            stopped: false,
        };

        for n in [1, 2, 3] {
            hub.send(Msg::Add(n));
        }
        hub.send(Msg::Stop);

        run(&mut model, &mut effects, &mut terminal(), &hub, &mut inbox)
            .await
            .expect("the loop");

        assert_eq!(model.count, 6);
        assert!(effects.started && effects.stopped, "both ends were called");
    }

    #[tokio::test]
    async fn a_burst_of_messages_becomes_one_frame() {
        let (hub, mut inbox) = hub::channel();
        let draws = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut model = Counter {
            draws: std::sync::Arc::clone(&draws),
            ..Counter::default()
        };
        let mut effects = Executor {
            started: false,
            stopped: false,
        };

        // All waiting before the loop starts, so they are all drained together.
        for _ in 0..100 {
            hub.send(Msg::Add(1));
        }
        hub.send(Msg::Stop);

        run(&mut model, &mut effects, &mut terminal(), &hub, &mut inbox)
            .await
            .expect("the loop");

        assert_eq!(model.count, 100);
        assert!(
            draws.load(std::sync::atomic::Ordering::Relaxed) <= 2,
            "a hundred deltas are one screen, not a hundred"
        );
    }

    #[tokio::test]
    async fn an_effect_reports_back_as_a_message() {
        let (hub, mut inbox) = hub::channel();
        let mut model = Counter::default();
        let mut effects = Executor {
            started: false,
            stopped: false,
        };

        // The executor turns this into `Add(5)`, which lands on the hub and is
        // applied by the same loop.
        hub.send(Msg::Add(0));
        model.update(Msg::Add(0));
        hub.send(Msg::Stop);
        effects.perform(Effect::Echo(5), &hub);

        run(&mut model, &mut effects, &mut terminal(), &hub, &mut inbox)
            .await
            .expect("the loop");

        assert_eq!(model.count, 5, "the round trip closed");
    }

    #[tokio::test]
    async fn a_drawing_reports_what_it_laid_out() {
        let (hub, mut inbox) = hub::channel();
        let mut model = Counter::default();
        let mut effects = Executor {
            started: false,
            stopped: false,
        };

        hub.send(Msg::Stop);
        run(&mut model, &mut effects, &mut terminal(), &hub, &mut inbox)
            .await
            .expect("the loop");

        assert_eq!(model.width, 20, "the model heard how wide the pane was");
    }

    #[tokio::test]
    async fn a_timer_the_model_asked_for_fires() {
        let (hub, mut inbox) = hub::channel();
        let mut model = Counter::default();
        let mut effects = Executor {
            started: false,
            stopped: false,
        };

        // Nothing is sent: the only thing that can wake the loop is the tick.
        let stopper = hub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            stopper.send(Msg::Stop);
        });

        run(&mut model, &mut effects, &mut terminal(), &hub, &mut inbox)
            .await
            .expect("the loop");

        assert!(model.ticks > 0, "a 5 ms tick fired inside 60 ms");
    }

    #[tokio::test]
    async fn an_unchanged_subscription_keeps_its_interval() {
        let mut timers = super::Timers::default();
        let want = Subs {
            tick: Some(Duration::from_millis(5)),
            ..Subs::default()
        };

        timers.retune(want);
        assert_eq!(timers.built, 1);

        timers.retune(want);
        assert_eq!(
            timers.built, 1,
            "asking for the same cadence must not restart the period"
        );

        timers.retune(Subs::default());
        assert!(timers.tick.is_none(), "no longer asked for, so disarmed");
    }
}
