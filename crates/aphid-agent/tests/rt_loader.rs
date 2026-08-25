//! A composition described as data, and kept in step with it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aphid_agent::rt::{
    Component, Composition, Context, Disposer, Entry, Isolate, Loader, Resolver, Service, State,
};
use serde_json::{Value, json};

struct Shell;
impl Service for Shell {
    const NAME: &'static str = "shell";
    type Handle = Arc<String>;
}

/// A component built from an entry's url and config.
struct Part {
    label: String,
    provides: Vec<&'static str>,
    inject: Vec<&'static str>,
    live: Arc<AtomicUsize>,
}

impl Component for Part {
    fn name(&self) -> &str {
        &self.label
    }
    fn provides(&self) -> &[&'static str] {
        &self.provides
    }
    fn inject(&self) -> &[&'static str] {
        &self.inject
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        if self.provides.contains(&"shell") {
            ctx.provide::<Shell>(Arc::new(self.label.clone()));
        }
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

/// `url` is `provider` or `consumer`; `config.label` names the instance.
struct Parts {
    live: Arc<AtomicUsize>,
    builds: Arc<AtomicUsize>,
}

impl Resolver for Parts {
    fn resolve(&self, entry: &Entry) -> Result<Arc<dyn Component>, String> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        let label = entry
            .config
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or(&entry.id)
            .to_owned();
        match entry.url.as_str() {
            "provider" => Ok(Arc::new(Part {
                label,
                provides: vec!["shell"],
                inject: Vec::new(),
                live: Arc::clone(&self.live),
            })),
            "consumer" => Ok(Arc::new(Part {
                label,
                provides: Vec::new(),
                inject: vec!["shell"],
                live: Arc::clone(&self.live),
            })),
            other => Err(format!("nothing answers to `{other}`")),
        }
    }
}

fn fixture() -> (Composition, Loader, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let composition = Composition::new();
    let live = Arc::new(AtomicUsize::new(0));
    let builds = Arc::new(AtomicUsize::new(0));
    let loader = Loader::new(
        &composition,
        Arc::new(Parts {
            live: Arc::clone(&live),
            builds: Arc::clone(&builds),
        }),
    );
    (composition, loader, live, builds)
}

#[tokio::test]
async fn entries_load_whatever_order_they_are_listed_in() {
    for reversed in [false, true] {
        let (composition, mut loader, live, _) = fixture();
        let mut entries = vec![Entry::new("p", "provider"), Entry::new("c", "consumer")];
        if reversed {
            entries.reverse();
        }

        let report = loader.reconcile(entries).await;
        assert_eq!(report.mounted.len(), 2, "{report:?}");
        assert_eq!(live.load(Ordering::SeqCst), 2);
        for (_, uid) in loader.loaded() {
            assert_eq!(composition.runtime.state(uid), Some(State::Active));
        }
    }
}

#[tokio::test]
async fn an_entry_that_leaves_the_list_is_unloaded() {
    let (_composition, mut loader, live, _) = fixture();
    loader
        .reconcile(vec![
            Entry::new("p", "provider"),
            Entry::new("c", "consumer"),
        ])
        .await;
    assert_eq!(live.load(Ordering::SeqCst), 2);

    let report = loader.reconcile(vec![Entry::new("p", "provider")]).await;
    assert_eq!(report.unmounted, ["c"]);
    assert_eq!(live.load(Ordering::SeqCst), 1);
    assert_eq!(loader.loaded().len(), 1);
}

#[tokio::test]
async fn disabling_an_entry_keeps_it_without_running_it() {
    let (composition, mut loader, live, builds) = fixture();
    loader.reconcile(vec![Entry::new("p", "provider")]).await;
    let built = builds.load(Ordering::SeqCst);

    let mut off = Entry::new("p", "provider");
    off.disabled = true;
    loader.reconcile(vec![off]).await;

    let uid = loader.loaded()[0].1;
    assert_eq!(composition.runtime.state(uid), Some(State::Inactive));
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(
        builds.load(Ordering::SeqCst),
        built,
        "switching one off does not rebuild it"
    );

    loader.reconcile(vec![Entry::new("p", "provider")]).await;
    assert_eq!(composition.runtime.state(uid), Some(State::Active));
    assert_eq!(live.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_changed_url_rebuilds_and_a_changed_config_does_too() {
    let (_composition, mut loader, _, builds) = fixture();
    loader.reconcile(vec![Entry::new("p", "provider")]).await;
    assert_eq!(builds.load(Ordering::SeqCst), 1);

    // Same id, different component.
    let report = loader.reconcile(vec![Entry::new("p", "consumer")]).await;
    assert_eq!(report.reloaded, ["p"]);
    assert_eq!(builds.load(Ordering::SeqCst), 2);

    // Same component, different configuration.
    let mut configured = Entry::new("p", "consumer");
    configured.config = json!({ "label": "renamed" });
    let report = loader.reconcile(vec![configured]).await;
    assert_eq!(report.reloaded, ["p"]);
    assert_eq!(builds.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn nothing_changed_means_nothing_happened() {
    let (_composition, mut loader, _, builds) = fixture();
    let entries = vec![Entry::new("p", "provider"), Entry::new("c", "consumer")];
    loader.reconcile(entries.clone()).await;
    let built = builds.load(Ordering::SeqCst);

    let report = loader.reconcile(entries).await;
    assert!(report.is_quiet(), "{report:?}");
    assert_eq!(builds.load(Ordering::SeqCst), built);
}

#[tokio::test]
async fn a_changed_isolate_reassigns_rather_than_rebuilds() {
    let (composition, mut loader, _, builds) = fixture();
    loader
        .reconcile(vec![
            Entry::new("p", "provider"),
            Entry::new("c", "consumer"),
        ])
        .await;
    let built = builds.load(Ordering::SeqCst);
    let consumer = loader
        .loaded()
        .into_iter()
        .find(|(id, _)| *id == "c")
        .expect("listed")
        .1;
    assert_eq!(composition.runtime.state(consumer), Some(State::Active));

    // The consumer moves to a realm of its own, where nothing provides `shell`.
    let mut moved = Entry::new("c", "consumer");
    moved.isolate = BTreeMap::from([("shell".to_owned(), Isolate::Local)]);
    let report = loader
        .reconcile(vec![Entry::new("p", "provider"), moved])
        .await;

    assert_eq!(report.reloaded, ["c"]);
    assert_eq!(
        builds.load(Ordering::SeqCst),
        built,
        "moving realms does not rebuild the component"
    );
    assert_eq!(composition.runtime.state(consumer), Some(State::Pending));
}

#[tokio::test]
async fn one_entry_that_cannot_be_resolved_does_not_stop_the_others() {
    let (_composition, mut loader, live, _) = fixture();
    let report = loader
        .reconcile(vec![
            Entry::new("p", "provider"),
            Entry::new("bad", "no-such-thing"),
            Entry::new("c", "consumer"),
        ])
        .await;

    assert_eq!(report.mounted.len(), 2);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "bad");
    assert!(report.failed[0].1.contains("no-such-thing"));
    assert_eq!(live.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_route_through_the_list_lands_where_the_list_does() {
    // The same property the metatheory test checks over generated routes,
    // stated once at the level an operator sees it.
    let final_entries = vec![Entry::new("p", "provider"), Entry::new("c", "consumer")];

    let (taken, mut loader, live_taken, _) = fixture();
    loader.reconcile(vec![Entry::new("c", "consumer")]).await;
    loader
        .reconcile(vec![Entry::new("bad", "no-such-thing")])
        .await;
    loader.reconcile(final_entries.clone()).await;

    let (direct, mut fresh, live_direct, _) = fixture();
    fresh.reconcile(final_entries).await;

    assert_eq!(
        live_taken.load(Ordering::SeqCst),
        live_direct.load(Ordering::SeqCst)
    );
    assert_eq!(
        taken.runtime.bindings().len(),
        direct.runtime.bindings().len()
    );
    assert_eq!(
        loader
            .loaded()
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        fresh.loaded().iter().map(|(id, _)| *id).collect::<Vec<_>>()
    );
}
