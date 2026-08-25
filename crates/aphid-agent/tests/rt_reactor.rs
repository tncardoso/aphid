//! The reactor thread: what it serialises, and what it publishes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aphid_agent::rt::{Component, Context, Disposer, Job, Reactor, Service, State};
use serde_json::Value;

struct Clock;
impl Service for Clock {
    const NAME: &'static str = "clock";
    type Handle = Arc<u64>;
}

struct Ticker;
impl Component for Ticker {
    fn name(&self) -> &str {
        "ticker"
    }
    fn provides(&self) -> &[&'static str] {
        &["clock"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        ctx.provide::<Clock>(Arc::new(7));
        Ok(())
    }
}

struct Watcher {
    live: Arc<AtomicUsize>,
}

impl Component for Watcher {
    fn name(&self) -> &str {
        "watcher"
    }
    fn inject(&self) -> &[&'static str] {
        &["clock"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        assert_eq!(*ctx.need::<Clock>(), 7);
        self.live.fetch_add(1, Ordering::SeqCst);
        let live = Arc::clone(&self.live);
        ctx.effect(move || {
            Disposer::sync(move || {
                live.fetch_sub(1, Ordering::SeqCst);
            })
        });
        Ok(())
    }
}

#[tokio::test]
async fn the_reactor_mounts_settles_and_publishes() {
    let published = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&published);
    let reactor = Reactor::spawn(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });

    let live = Arc::new(AtomicUsize::new(0));

    // Mounted in the order that would be wrong if order decided anything.
    let watcher = reactor
        .mount(
            Arc::new(Watcher {
                live: Arc::clone(&live),
            }),
            Value::Null,
        )
        .await
        .expect("mounts");
    let ticker = reactor
        .mount(Arc::new(Ticker), Value::Null)
        .await
        .expect("mounts");

    let snapshot = reactor.snapshot();
    assert_eq!(live.load(Ordering::SeqCst), 1);
    assert_eq!(snapshot.roster.len(), 2);
    assert!(snapshot.active().count() == 2, "{:?}", snapshot.roster);
    assert!(published.load(Ordering::SeqCst) >= 2);

    // Taking the provider away takes the consumer with it, and says so.
    reactor.send(Job::Unmount(ticker));
    reactor.send(Job::Settle);
    settled(&reactor, |s| {
        s.roster.iter().all(|status| status.state != State::Active)
    })
    .await;

    let snapshot = reactor.snapshot();
    let waiting: Vec<_> = snapshot.waiting().map(|status| status.uid).collect();
    assert_eq!(waiting, vec![watcher]);
    assert_eq!(live.load(Ordering::SeqCst), 0);

    reactor.stop();
}

#[tokio::test]
async fn stopping_the_reactor_unloads_everything_it_held() {
    let reactor = Reactor::spawn(|_| {});
    let live = Arc::new(AtomicUsize::new(0));

    reactor
        .mount(Arc::new(Ticker), Value::Null)
        .await
        .expect("mounts");
    reactor
        .mount(
            Arc::new(Watcher {
                live: Arc::clone(&live),
            }),
            Value::Null,
        )
        .await
        .expect("mounts");
    assert_eq!(live.load(Ordering::SeqCst), 1);

    reactor.stop();

    // `stop` joins the thread, which unloads the composition on its way out —
    // so by the time it returns, every effect has been reverted.
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_refused_mount_answers_rather_than_killing_the_thread() {
    struct Bad;
    impl Component for Bad {
        fn name(&self) -> &str {
            "bad"
        }
        fn schema(&self) -> Option<&Value> {
            static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            Some(SCHEMA.get_or_init(|| serde_json::json!({ "type": "object" })))
        }
        fn apply(&self, _ctx: &Context) -> Result<(), String> {
            Ok(())
        }
    }

    let reactor = Reactor::spawn(|_| {});
    let refused = reactor
        .mount(Arc::new(Bad), serde_json::json!("not an object"))
        .await;
    assert!(refused.is_err(), "{refused:?}");

    // Still alive and still serving.
    reactor
        .mount(Arc::new(Ticker), Value::Null)
        .await
        .expect("mounts");
    assert_eq!(reactor.snapshot().active().count(), 1);
    reactor.stop();
}

/// Wait for the reactor to reach a state, without assuming how many turns it
/// takes to get there.
async fn settled(reactor: &Reactor, done: impl Fn(&aphid_agent::rt::Snapshot) -> bool) {
    for _ in 0..200 {
        if done(&reactor.snapshot()) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the reactor never reached the expected state");
}
