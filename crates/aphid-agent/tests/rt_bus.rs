//! The five dispatch modes, and what each one promises.

use std::sync::{Arc, Mutex};

use aphid_agent::rt::{
    Bailed, Bus, Emitted, Event, Failure, Next, Paralleled, Serialed, Uid, Waterfalled,
};

fn owner() -> Uid {
    // Any two calls give different owners, which is what unsubscribing needs.
    aphid_agent::rt::Runtime::new()
        .mount(Arc::new(Nothing), serde_json::Value::Null)
        .expect("mounts")
}

struct Nothing;
impl aphid_agent::rt::Component for Nothing {
    fn name(&self) -> &str {
        "nothing"
    }
    fn apply(&self, _ctx: &aphid_agent::rt::Context) -> Result<(), String> {
        Ok(())
    }
}

// ------------------------------------------------------------------------ emit

struct Draft {
    text: String,
}
impl Event for Draft {
    const NAME: &'static str = "demo/draft";
}
impl Emitted for Draft {}

#[test]
fn every_listener_runs_and_each_sees_the_last_ones_edits() {
    let bus = Bus::new();
    bus.on::<Draft>(owner(), |draft| draft.text.push_str(" one"));
    bus.on::<Draft>(owner(), |draft| draft.text.push_str(" two"));

    let mut draft = Draft {
        text: "start".to_owned(),
    };
    bus.emit(&mut draft);
    assert_eq!(draft.text, "start one two");
}

#[test]
fn a_listener_leaves_when_its_owner_does() {
    let bus = Bus::new();
    let first = owner();
    bus.on::<Draft>(first, |draft| draft.text.push('a'));
    bus.on::<Draft>(owner(), |draft| draft.text.push('b'));

    bus.unsubscribe::<Draft>(first);

    let mut draft = Draft {
        text: String::new(),
    };
    bus.emit(&mut draft);
    assert_eq!(draft.text, "b");
}

#[test]
fn an_event_nobody_listens_to_costs_one_lookup() {
    let bus = Bus::new();
    assert!(!bus.has_listeners::<Draft>());
    bus.on::<Draft>(owner(), |_| {});
    assert!(bus.has_listeners::<Draft>());
}

// ------------------------------------------------------------------------ bail

struct Call {
    tool: String,
}
impl Event for Call {
    const NAME: &'static str = "demo/call";
    // A guard that raised has not agreed to anything.
    const FAILURE: Failure = Failure::Closed;
}
impl Bailed for Call {
    type Out = String;
}

#[test]
fn the_first_answer_stops_the_rest() {
    let bus = Bus::new();
    let reached = Arc::new(Mutex::new(Vec::new()));

    let log = Arc::clone(&reached);
    bus.on_bail::<Call>(owner(), move |call| {
        log.lock().expect("not poisoned").push("first");
        (call.tool == "rm").then(|| "blocked".to_owned())
    });
    let log = Arc::clone(&reached);
    bus.on_bail::<Call>(owner(), move |_| {
        log.lock().expect("not poisoned").push("second");
        None
    });

    let blocked = bus.bail(&Call {
        tool: "rm".to_owned(),
    });
    assert_eq!(blocked.as_deref(), Some("blocked"));
    assert_eq!(*reached.lock().expect("not poisoned"), ["first"]);

    reached.lock().expect("not poisoned").clear();
    let allowed = bus.bail(&Call {
        tool: "read".to_owned(),
    });
    assert_eq!(allowed, None);
    assert_eq!(*reached.lock().expect("not poisoned"), ["first", "second"]);
}

#[test]
fn the_failure_policy_is_a_property_of_the_event() {
    assert_eq!(Call::FAILURE, Failure::Closed);
    assert_eq!(Draft::FAILURE, Failure::Open);
}

// ------------------------------------------------------------------- waterfall

struct Transform;
impl Event for Transform {
    const NAME: &'static str = "demo/transform";
}
impl Waterfalled for Transform {
    type In = String;
    type Out = String;
}

#[test]
fn a_waterfall_wraps_what_the_rest_returns() {
    let bus = Bus::new();
    bus.on_waterfall::<Transform>(owner(), |input: String, next: Next<'_, Transform>| {
        next.run(input).to_uppercase()
    });
    bus.on_waterfall::<Transform>(owner(), |input: String, next: Next<'_, Transform>| {
        next.run(format!("{input}!"))
    });

    let out = bus.waterfall::<Transform>("hello".to_owned(), &|input| input);
    assert_eq!(out, "HELLO!");
}

#[test]
fn a_waterfall_listener_can_answer_instead_of_continuing() {
    let bus = Bus::new();
    // Outermost: still gets to transform the replacement on the way out.
    bus.on_waterfall::<Transform>(owner(), |input: String, next: Next<'_, Transform>| {
        next.run(input).to_uppercase()
    });
    // Vetoes: the default underneath never runs.
    bus.on_waterfall::<Transform>(owner(), |input: String, next: Next<'_, Transform>| {
        if input.contains("blocked") {
            return "** blocked **".to_owned();
        }
        next.run(input)
    });

    let vetoed = bus.waterfall::<Transform>("blocked words".to_owned(), &|_| {
        panic!("the default must not run when a listener answers instead")
    });
    assert_eq!(vetoed, "** BLOCKED **");
}

#[test]
fn a_waterfall_with_no_listeners_is_the_default() {
    let bus = Bus::new();
    let out = bus.waterfall::<Transform>("plain".to_owned(), &|input| format!("[{input}]"));
    assert_eq!(out, "[plain]");
}

// -------------------------------------------------------------------- parallel

struct Ping;
impl Event for Ping {
    const NAME: &'static str = "demo/ping";
}
impl Paralleled for Ping {}

#[tokio::test]
async fn parallel_listeners_all_run() {
    let bus = Bus::new();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for _ in 0..3 {
        let count = Arc::clone(&count);
        bus.on_parallel::<Ping, _, _>(owner(), move |_| {
            let count = Arc::clone(&count);
            async move {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
    }

    bus.parallel(Ping).await;
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
}

// ---------------------------------------------------------------------- serial

struct Ask {
    question: String,
}
impl Event for Ask {
    const NAME: &'static str = "demo/ask";
}
impl Serialed for Ask {
    type Out = String;
}

#[tokio::test]
async fn serial_stops_at_the_first_answer() {
    let bus = Bus::new();
    let reached = Arc::new(Mutex::new(Vec::new()));

    let log = Arc::clone(&reached);
    bus.on_serial::<Ask, _, _>(owner(), move |ask: Arc<Ask>| {
        let log = Arc::clone(&log);
        async move {
            log.lock().expect("not poisoned").push("first");
            (ask.question == "who").then(|| "me".to_owned())
        }
    });
    let log = Arc::clone(&reached);
    bus.on_serial::<Ask, _, _>(owner(), move |_| {
        let log = Arc::clone(&log);
        async move {
            log.lock().expect("not poisoned").push("second");
            Some("fallback".to_owned())
        }
    });

    let answer = bus
        .serial(Ask {
            question: "who".to_owned(),
        })
        .await;
    assert_eq!(answer.as_deref(), Some("me"));
    assert_eq!(*reached.lock().expect("not poisoned"), ["first"]);
}

// ------------------------------------------------------------- declared names

#[test]
fn subscribing_to_a_name_nobody_emits_is_knowable() {
    let bus = Bus::new();
    assert!(!bus.is_declared("demo/draft"));

    bus.declare(&["demo/draft"]);
    assert!(bus.is_declared("demo/draft"));

    // And it stops being declared when the component that emitted it goes.
    bus.undeclare(&["demo/draft"]);
    assert!(!bus.is_declared("demo/draft"));
}

#[test]
fn two_components_emitting_one_name_both_have_to_go() {
    let bus = Bus::new();
    bus.declare(&["shared/event"]);
    bus.declare(&["shared/event"]);
    bus.undeclare(&["shared/event"]);
    assert!(bus.is_declared("shared/event"), "one is still emitting it");
    bus.undeclare(&["shared/event"]);
    assert!(!bus.is_declared("shared/event"));
}
