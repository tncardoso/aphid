//! The thread that owns every call into a script.
//!
//! Before it there were four callers and two of them could be inside the same
//! plugin's engine at the same moment, each doing read-change-write on its
//! state. These say that cannot happen any more.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aphid_agent::exec;
use aphid_plugin::{Capabilities, Job, PluginHost, PluginHub, Report, Silent, explicit};

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

fn hub(host: Arc<PluginHost>) -> (PluginHub, Receiver<Report>) {
    let (sender, reports): (Sender<Report>, _) = channel();
    let sender = Mutex::new(sender);
    let hub = PluginHub::spawn(host, move |report| {
        if let Ok(sender) = sender.lock() {
            let _ = sender.send(report);
        }
    });
    (hub, reports)
}

/// Wait for the thread to have nothing left to do.
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
        r#"
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
        fn on_tick() { enter(); leave(); }
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "right" },
            view: |s| { enter(); leave(); #{ type: "text", text: "panel" } }
        });
        register_command(#{ name: "poke", run: |args| { enter(); leave(); notice("poked") } });
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
        r#"
        fn on_tick() {
            let s = state();
            s.count = if "count" in s { s.count + 1 } else { 1 };
            state(s);
        }
        register_surface(#{
            name: "panel",
            placement: #{ kind: "side", side: "right" },
            // The count lives in the plugin's own state, which is what the
            // tick writes; the panel reads it rather than holding its own.
            view: |s| #{ type: "text", text: "count " + count() }
        });
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
        aphid_plugin::Widget::Text {
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
    let host = host(r#"fn on_run_start(cx) {}"#);
    let (hub, reports) = hub(host);

    hub.send(Job::Surface {
        plugin: "kit".to_owned(),
        name: "nothing".to_owned(),
        event: aphid_plugin::SurfaceEvent::Key {
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
