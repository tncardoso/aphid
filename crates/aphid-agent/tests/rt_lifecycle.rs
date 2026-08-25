//! What a fiber does, and when.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aphid_agent::rt::{Component, Context, Disposer, Runtime, Service, State};
use serde_json::{Value, json};

// A capability with nothing behind it: enough to be provided and injected.
struct Beacon;
impl Service for Beacon {
    const NAME: &'static str = "beacon";
    type Handle = Arc<String>;
}

struct Provider {
    label: &'static str,
}

impl Component for Provider {
    fn name(&self) -> &str {
        self.label
    }
    fn provides(&self) -> &[&'static str] {
        &["beacon"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        ctx.provide::<Beacon>(Arc::new(self.label.to_owned()));
        Ok(())
    }
}

#[derive(Default)]
struct Trace {
    loaded: AtomicUsize,
    unloaded: AtomicUsize,
    seen: std::sync::Mutex<Vec<String>>,
}

struct Consumer {
    trace: Arc<Trace>,
}

impl Component for Consumer {
    fn name(&self) -> &str {
        "consumer"
    }
    fn inject(&self) -> &[&'static str] {
        &["beacon"]
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        self.trace.loaded.fetch_add(1, Ordering::SeqCst);
        let label = ctx.need::<Beacon>();
        if let Ok(mut seen) = self.trace.seen.lock() {
            seen.push(label.to_string());
        }
        let trace = Arc::clone(&self.trace);
        ctx.effect(move || {
            Disposer::sync(move || {
                trace.unloaded.fetch_add(1, Ordering::SeqCst);
            })
        });
        Ok(())
    }
}

#[tokio::test]
async fn a_consumer_waits_for_its_provider_and_then_loads() {
    let rt = Runtime::new();
    let trace = Arc::new(Trace::default());

    let consumer = rt
        .mount(
            Arc::new(Consumer {
                trace: Arc::clone(&trace),
            }),
            Value::Null,
        )
        .expect("mounts");
    rt.settle().await;

    // Nothing provides `beacon` yet, so it is waiting rather than failed.
    assert_eq!(rt.state(consumer), Some(State::Pending));
    assert_eq!(trace.loaded.load(Ordering::SeqCst), 0);

    rt.mount(Arc::new(Provider { label: "first" }), Value::Null)
        .expect("mounts");
    rt.settle().await;

    assert_eq!(rt.state(consumer), Some(State::Active));
    assert_eq!(trace.loaded.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn declaration_order_does_not_decide_load_order() {
    for provider_first in [true, false] {
        let rt = Runtime::new();
        let trace = Arc::new(Trace::default());
        let consumer = Arc::new(Consumer {
            trace: Arc::clone(&trace),
        });
        let provider = Arc::new(Provider { label: "either" });

        if provider_first {
            rt.mount(provider, Value::Null).expect("mounts");
            rt.mount(consumer, Value::Null).expect("mounts");
        } else {
            rt.mount(consumer, Value::Null).expect("mounts");
            rt.mount(provider, Value::Null).expect("mounts");
        }
        rt.settle().await;

        assert_eq!(
            trace.loaded.load(Ordering::SeqCst),
            1,
            "order {provider_first}"
        );
    }
}

#[tokio::test]
async fn losing_a_provider_unloads_the_dependent() {
    let rt = Runtime::new();
    let trace = Arc::new(Trace::default());

    let provider = rt
        .mount(Arc::new(Provider { label: "first" }), Value::Null)
        .expect("mounts");
    let consumer = rt
        .mount(
            Arc::new(Consumer {
                trace: Arc::clone(&trace),
            }),
            Value::Null,
        )
        .expect("mounts");
    rt.settle().await;
    assert_eq!(rt.state(consumer), Some(State::Active));

    rt.unmount(provider).await;

    assert_eq!(rt.state(provider), Some(State::Inactive));
    assert_eq!(rt.state(consumer), Some(State::Pending));
    assert_eq!(trace.unloaded.load(Ordering::SeqCst), 1);

    // And it comes back when the service does.
    rt.enable(provider).await;
    assert_eq!(rt.state(consumer), Some(State::Active));
    assert_eq!(trace.loaded.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_replaced_provider_reloads_the_dependent_against_the_new_one() {
    let rt = Runtime::new();
    let trace = Arc::new(Trace::default());

    let first = rt
        .mount(Arc::new(Provider { label: "first" }), Value::Null)
        .expect("mounts");
    rt.mount(
        Arc::new(Consumer {
            trace: Arc::clone(&trace),
        }),
        Value::Null,
    )
    .expect("mounts");
    rt.settle().await;

    rt.unmount(first).await;
    rt.mount(Arc::new(Provider { label: "second" }), Value::Null)
        .expect("mounts");
    rt.settle().await;

    let seen = trace.seen.lock().expect("not poisoned").clone();
    assert_eq!(seen, vec!["first".to_owned(), "second".to_owned()]);
}

// --------------------------------------------------------------------- effects

struct Registrar {
    log: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl Component for Registrar {
    fn name(&self) -> &str {
        "registrar"
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        for step in ["first", "second", "third"] {
            let log = Arc::clone(&self.log);
            ctx.effect(move || {
                Disposer::sync(move || {
                    if let Ok(mut log) = log.lock() {
                        log.push(step);
                    }
                })
            });
        }
        Ok(())
    }
}

#[tokio::test]
async fn disposers_run_back_to_front() {
    let rt = Runtime::new();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let uid = rt
        .mount(
            Arc::new(Registrar {
                log: Arc::clone(&log),
            }),
            Value::Null,
        )
        .expect("mounts");
    rt.settle().await;
    rt.unmount(uid).await;

    let order = log.lock().expect("not poisoned").clone();
    assert_eq!(order, vec!["third", "second", "first"]);
}

#[tokio::test]
async fn an_effect_reverts_at_most_once() {
    let rt = Runtime::new();
    let count = Arc::new(AtomicUsize::new(0));

    struct Once {
        count: Arc<AtomicUsize>,
    }
    impl Component for Once {
        fn name(&self) -> &str {
            "once"
        }
        fn apply(&self, ctx: &Context) -> Result<(), String> {
            let count = Arc::clone(&self.count);
            let handle = ctx.effect(move || {
                Disposer::sync(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                })
            });
            // Reverting by hand is allowed; the fiber must not revert it again.
            futures_lite_block(handle);
            Ok(())
        }
    }
    fn futures_lite_block(handle: aphid_agent::rt::Handle) {
        // The disposer here is synchronous, so the future is ready on first
        // poll and this never actually blocks.
        let mut future = Box::pin(async move { handle.dispose().await });
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(std::future::Future::poll(future.as_mut(), &mut cx).is_ready());
    }

    let uid = rt
        .mount(
            Arc::new(Once {
                count: Arc::clone(&count),
            }),
            Value::Null,
        )
        .expect("mounts");
    rt.settle().await;
    rt.unmount(uid).await;

    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------- composition

struct Parent;

impl Component for Parent {
    fn name(&self) -> &str {
        "parent"
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        ctx.mount(Arc::new(Provider { label: "child" }), Value::Null)?;
        Ok(())
    }
}

#[tokio::test]
async fn unloading_a_parent_unloads_what_it_mounted() {
    let rt = Runtime::new();
    let parent = rt.mount(Arc::new(Parent), Value::Null).expect("mounts");
    rt.settle().await;

    assert_eq!(rt.roster().len(), 2);
    assert!(
        rt.bindings()
            .iter()
            .any(|(realm, _)| realm.key() == "beacon")
    );

    rt.unmount(parent).await;

    // The child's binding left with it: a departing fiber contributes nothing.
    assert!(rt.bindings().is_empty());
}

// --------------------------------------------------------------------- failure

struct Exploder;

impl Component for Exploder {
    fn name(&self) -> &str {
        "exploder"
    }
    fn apply(&self, _ctx: &Context) -> Result<(), String> {
        Err("apply exploded".to_owned())
    }
}

struct Panicker;

impl Component for Panicker {
    fn name(&self) -> &str {
        "panicker"
    }
    fn apply(&self, _ctx: &Context) -> Result<(), String> {
        panic!("apply panicked");
    }
}

#[tokio::test]
async fn a_component_that_raises_fails_loudly_and_alone() {
    let rt = Runtime::new();
    let bad = rt.mount(Arc::new(Exploder), Value::Null).expect("mounts");
    let good = rt
        .mount(Arc::new(Provider { label: "fine" }), Value::Null)
        .expect("mounts");
    rt.settle().await;

    assert_eq!(rt.state(bad), Some(State::Failed));
    assert_eq!(rt.state(good), Some(State::Active));

    let status = rt
        .roster()
        .into_iter()
        .find(|s| s.uid == bad)
        .expect("listed");
    assert_eq!(status.error.as_deref(), Some("apply exploded"));
}

#[tokio::test]
async fn a_panic_in_one_component_does_not_take_the_rest() {
    let rt = Runtime::new();
    let bad = rt.mount(Arc::new(Panicker), Value::Null).expect("mounts");
    let good = rt
        .mount(Arc::new(Provider { label: "fine" }), Value::Null)
        .expect("mounts");
    rt.settle().await;

    assert_eq!(rt.state(bad), Some(State::Failed));
    assert_eq!(rt.state(good), Some(State::Active));
}

// ----------------------------------------------------------------------- cycle

struct Ping;
impl Component for Ping {
    fn name(&self) -> &str {
        "ping"
    }
    fn inject(&self) -> &[&'static str] {
        &["pong"]
    }
    fn provides(&self) -> &[&'static str] {
        &["ping"]
    }
    fn apply(&self, _ctx: &Context) -> Result<(), String> {
        Ok(())
    }
}

struct Pong;
impl Component for Pong {
    fn name(&self) -> &str {
        "pong"
    }
    fn inject(&self) -> &[&'static str] {
        &["ping"]
    }
    fn provides(&self) -> &[&'static str] {
        &["pong"]
    }
    fn apply(&self, _ctx: &Context) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn a_dependency_cycle_is_refused_at_mount_not_left_silent() {
    let rt = Runtime::new();
    rt.mount(Arc::new(Ping), Value::Null).expect("first mounts");
    let refused = rt.mount(Arc::new(Pong), Value::Null);

    let message = refused.expect_err("a cycle cannot be mounted");
    assert!(
        message.contains("ping") && message.contains("pong"),
        "{message}"
    );
}

// ----------------------------------------------------------------------- config

struct Configured;

impl Component for Configured {
    fn name(&self) -> &str {
        "configured"
    }
    fn schema(&self) -> Option<&Value> {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        Some(SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": { "port": { "type": "integer" } },
                "required": ["port"]
            })
        }))
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        assert!(ctx.config().get("port").is_some());
        Ok(())
    }
}

#[tokio::test]
async fn config_that_fails_the_schema_is_refused_with_the_field_named() {
    let rt = Runtime::new();
    let refused = rt.mount(Arc::new(Configured), json!({ "port": "eighty" }));
    let message = refused.expect_err("bad config is refused");
    assert!(message.contains("port"), "{message}");

    rt.mount(Arc::new(Configured), json!({ "port": 80 }))
        .expect("good config mounts");
    rt.settle().await;
}

// -------------------------------------------------------------------- isolation

struct Isolating;

impl Component for Isolating {
    fn name(&self) -> &str {
        "isolating"
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        // Two children providing the same key, each in a realm of its own.
        ctx.isolate("beacon")
            .mount(Arc::new(Provider { label: "left" }), Value::Null)?;
        ctx.isolate("beacon")
            .mount(Arc::new(Provider { label: "right" }), Value::Null)?;
        Ok(())
    }
}

#[tokio::test]
async fn two_isolated_scopes_bind_the_same_key_independently() {
    let rt = Runtime::new();
    rt.mount(Arc::new(Isolating), Value::Null).expect("mounts");
    rt.settle().await;

    let beacons: Vec<_> = rt
        .bindings()
        .into_iter()
        .filter(|(realm, _)| realm.key() == "beacon")
        .collect();

    // Without isolation the second provider would have overwritten the first.
    assert_eq!(beacons.len(), 2, "{beacons:?}");
    assert_ne!(beacons[0].0, beacons[1].0);
}
