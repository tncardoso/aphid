//! The thread that owns every call into a script.
//!
//! Before it there were four callers and two of them could be inside the same
//! plugin's engine at the same moment, each doing read-change-write on its
//! state. These say that cannot happen any more.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_agent::Silent;
use aphid_agent::exec;
use aphid_code::scripting::{Capabilities, Job, PluginHost, PluginHub, Report, explicit};

/// A host with one plugin in it, written for the test.
fn host(source: &str) -> Arc<PluginHost> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let root = std::env::temp_dir().join(format!(
        "aphid-hub-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let dir = root.join(".aphid").join("plugins");
    std::fs::create_dir_all(&dir).expect("a directory to load from");
    std::fs::write(dir.join("kit.rhai"), source).expect("the plugin");

    let file = explicit(&dir.join("kit.rhai")).expect("readable");
    let (host, diagnostics) = PluginHost::load(
        &[file],
        &Capabilities::full(&root),
        Arc::new(Silent),
        &Arc::new(exec::Registry::new()),
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    Arc::new(host)
}

/// A hub, and the composition its plugins are mounted on.
///
/// The two together because a tick is announced on the bus and answered by
/// whatever subscribed to it — handing the hub an empty bus would be handing it
/// a tick with nowhere to go.
fn hub(host: Arc<PluginHost>) -> (PluginHub, Receiver<Report>) {
    let (sender, reports): (Sender<Report>, _) = channel();
    let sender = Mutex::new(sender);

    let composition = aphid_agent::rt::Composition::new();
    let registries = aphid_code::registries::Registries::for_composition(&composition);
    composition
        .mount(
            Arc::clone(&registries) as Arc<dyn aphid_agent::rt::Component>,
            serde_json::Value::Null,
        )
        .expect("the registry has no dependencies");
    for plugin in host.plugins() {
        let component = Arc::new(aphid_code::scripting::ScriptComponent::new(
            Arc::clone(plugin),
            &composition,
        ));
        composition
            .mount(component, serde_json::Value::Null)
            .expect("a plugin with no dependencies mounts");
    }
    // Nothing here is async, so one drain of the queue loads everything.
    futures_lite_block(composition.runtime.settle());

    let bus = Arc::clone(&composition.bus);
    let hub = PluginHub::spawn(host, bus, registries, move |report| {
        if let Ok(sender) = sender.lock() {
            let _ = sender.send(report);
        }
    });
    (hub, reports)
}

/// Drive a future to completion on the current thread.
fn futures_lite_block<T>(future: impl std::future::Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    loop {
        if let std::task::Poll::Ready(value) = std::future::Future::poll(future.as_mut(), &mut cx) {
            return value;
        }
    }
}

/// Wait for the thread to have nothing left to do.
///
/// A redraw answers only when something moved, so this pokes a plugin's state
/// first to make sure there is an answer to wait for.
fn settle(hub: &PluginHub, reports: &Receiver<Report>) -> Vec<Report> {
    hub.send(Job::Refresh);
    let mut seen = Vec::new();
    loop {
        match reports.recv_timeout(Duration::from_secs(5)) {
            Ok(Report::Surfaces(open)) => {
                seen.push(Report::Surfaces(open));
                return seen;
            }
            Ok(other) => seen.push(other),
            Err(_) => panic!("the script thread never answered"),
        }
    }
}

/// The race this thread exists to close: a tick and a panel redraw both doing
/// read-change-write on the same plugin's state.
///
/// The plugin records how deep it is on the way in and on the way out. One
/// thread means the depth can never be more than one, whatever order the jobs
/// arrive in.
#[test]
fn two_calls_into_one_plugin_never_overlap() {
    let host = host(
        r#"const inject = ["surfaces", "commands"];

        fn enter() {
            let s = state();
            s.depth = if "depth" in s { s.depth + 1 } else { 1 };
            if !("max" in s) { s.max = 0; }
            if s.depth > s.max { s.max = s.depth; }
            state(s);
        }
        fn leave() {
            let s = state();
            s.depth -= 1;
            state(s);
        }

        fn apply(ctx) {
            on("code/tick", || { enter(); leave(); });

            surface(#{
                name: "panel",
                placement: #{ kind: "side", side: "right" },
                view: |s| { enter(); leave(); #{ type: "text", text: "panel" } }
            });

            command(#{ name: "poke", run: |args| { enter(); leave(); notice("poked") } });
        }
"#,
    );
    let (hub, reports) = hub(Arc::clone(&host));

    // Interleave every kind of job that reaches a script.
    for _ in 0..50 {
        hub.send(Job::Tick);
        hub.send(Job::Refresh);
        hub.send(Job::Command {
            name: "poke".to_owned(),
            args: String::new(),
        });
        hub.send(Job::Notice("something happened".to_owned()));
    }
    let _ = settle(&hub, &reports);

    let state = host.state_of("kit").expect("the plugin's state");
    let max = state
        .get("max")
        .and_then(|value| value.as_int().ok())
        .expect("the plugin ran");
    assert_eq!(max, 1, "two calls were inside the plugin at once");

    let depth = state
        .get("depth")
        .and_then(|value| value.as_int().ok())
        .expect("the plugin ran");
    assert_eq!(depth, 0, "every call that went in came back out");
}

/// A state change one job makes is what the next job reads. Before the thread,
/// two callers could read the same state and one write would be lost.
#[test]
fn a_change_one_job_makes_is_what_the_next_one_reads() {
    let host = host(
        r#"const inject = ["surfaces"];

        fn apply(ctx) {
            on("code/tick", || {
                let s = state();
                s.count = if "count" in s { s.count + 1 } else { 1 };
                state(s);
            });

            surface(#{
                name: "panel",
                placement: #{ kind: "side", side: "right" },
                // The count lives in the plugin's own state, which is what the
                // tick writes; the panel reads it rather than holding its own.
                view: |s| #{ type: "text", text: "count " + count() }
            });
        }

        fn count() {
            let s = state();
            if "count" in s { s.count } else { 0 }
        }
"#,
    );
    let (hub, reports) = hub(Arc::clone(&host));

    hub.send(Job::Tick);
    let seen = settle(&hub, &reports);

    let Some(Report::Surfaces(open)) = seen.last() else {
        panic!("a redraw should answer: {}", seen.len());
    };
    assert_eq!(open.len(), 1);
    assert_eq!(
        open[0].widget,
        aphid_code::scripting::Widget::Text {
            id: None,
            text: "count 1".to_owned(),
        },
        "the redraw saw what the tick wrote"
    );
}

/// A surface that no longer exists answers with nothing rather than silently
/// looking like a surface that handled the event.
#[test]
fn an_event_for_a_surface_that_is_gone_says_so() {
    let host = host(r#"fn apply(ctx) {}"#);
    let (hub, reports) = hub(host);

    hub.send(Job::Surface {
        plugin: "kit".to_owned(),
        name: "nothing".to_owned(),
        event: aphid_code::scripting::SurfaceEvent::Key {
            code: "down".to_owned(),
            modifiers: Vec::new(),
        },
    });

    let report = reports
        .recv_timeout(Duration::from_secs(5))
        .expect("an answer");
    let Report::Surface { actions, .. } = report else {
        panic!("the wrong kind of answer");
    };
    assert_eq!(actions, None, "no surface by that name");
}

/// The tick asks for a redraw four times a second. A panel whose plugin has
/// not moved must not answer, or every quarter second is a copy of every
/// widget tree for nothing.
#[test]
fn a_redraw_says_nothing_when_no_panel_moved() {
    let host = host(
        r#"const inject = ["surfaces", "commands"];


fn apply(ctx) {
    surface(#{
                name: "panel",
                placement: #{ kind: "side", side: "right" },
                init: || #{ n: 0 },
                view: |s| #{ type: "text", text: "n " + s.n }
            });

    command(#{
                name: "bump",
                run: |args| {
                    let s = surface_state("panel");
                    s.n += 1;
                    surface_state("panel", s);
                    notice("bumped")
                }
            });
}
"#,
    );
    let (hub, reports) = hub(host);

    // The first answer is always worth giving.
    let _ = settle(&hub, &reports);

    for _ in 0..10 {
        hub.send(Job::Refresh);
    }
    assert!(
        reports.recv_timeout(Duration::from_millis(200)).is_err(),
        "nothing moved, so there is nothing to say"
    );

    hub.send(Job::Command {
        name: "bump".to_owned(),
        args: String::new(),
    });
    let seen = settle(&hub, &reports);
    assert!(
        seen.iter()
            .any(|report| matches!(report, Report::Surfaces(_))),
        "and it speaks up again the moment one does"
    );
}
