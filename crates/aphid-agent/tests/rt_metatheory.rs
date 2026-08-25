//! The three properties the composition model rests on.
//!
//! These are not examples. Each is a claim about *every* sequence of mounts and
//! unmounts, checked against a few thousand generated ones. They are the
//! difference between having implemented the model and having implemented
//! something that resembles it — an example test passes on the sequence you
//! thought of, and the ones that break a composition runtime are the ones you
//! did not.
//!
//! The generator is a hand-rolled xorshift rather than a property-testing
//! dependency: the shapes here are small enough that a seed and a loop cover
//! them, and a failure prints the seed to replay.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aphid_agent::rt::{Component, Context, Disposer, Realm, Runtime, Service, Uid};
use serde_json::Value;

// --------------------------------------------------------------- the fixtures

macro_rules! service {
    ($marker:ident, $name:literal) => {
        struct $marker;
        impl Service for $marker {
            const NAME: &'static str = $name;
            type Handle = Arc<&'static str>;
        }
    };
}

service!(Alpha, "alpha");
service!(Beta, "beta");
service!(Gamma, "gamma");

/// A component described by data, so the generator can build any shape.
struct Part {
    name: String,
    inject: Vec<&'static str>,
    provides: Vec<&'static str>,
    live: Arc<AtomicUsize>,
}

impl Component for Part {
    fn name(&self) -> &str {
        &self.name
    }
    fn inject(&self) -> &[&'static str] {
        &self.inject
    }
    fn provides(&self) -> &[&'static str] {
        &self.provides
    }
    fn apply(&self, ctx: &Context) -> Result<(), String> {
        for key in &self.provides {
            match *key {
                "alpha" => ctx.provide::<Alpha>(Arc::new("alpha")),
                "beta" => ctx.provide::<Beta>(Arc::new("beta")),
                _ => ctx.provide::<Gamma>(Arc::new("gamma")),
            };
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

/// One entry of a composition: what to build, and whether it is switched on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    inject: Vec<&'static str>,
    provides: Vec<&'static str>,
    enabled: bool,
}

const KEYS: [&str; 3] = ["alpha", "beta", "gamma"];

fn part(index: usize, entry: &Entry, live: &Arc<AtomicUsize>) -> Arc<dyn Component> {
    Arc::new(Part {
        name: format!("part-{index}"),
        inject: entry.inject.clone(),
        provides: entry.provides.clone(),
        live: Arc::clone(live),
    })
}

/// The observable state of a quiescent composition: which key is bound, and by
/// which entry. Fiber uids differ between two runs that reached the same place,
/// so the entry index is what makes two states comparable.
type Quiescent = Vec<(String, usize)>;

async fn observe(rt: &Runtime, index_of: &dyn Fn(Uid) -> Option<usize>) -> Quiescent {
    let mut state: Quiescent = rt
        .bindings()
        .into_iter()
        .filter_map(|(realm, provider)| {
            let Realm::Root(key) = realm else { return None };
            index_of(provider).map(|index| (key.to_owned(), index))
        })
        .collect();
    state.sort();
    state
}

/// Load a whole composition from nothing.
async fn from_scratch(entries: &[Entry], live: &Arc<AtomicUsize>) -> (Runtime, Vec<Option<Uid>>) {
    let rt = Runtime::new();
    let mut uids = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        if !entry.enabled {
            uids.push(None);
            continue;
        }
        uids.push(rt.mount(part(index, entry, live), Value::Null).ok());
    }
    rt.settle().await;
    (rt, uids)
}

// ----------------------------------------------------------------- the generator

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

/// A composition with no cycle in it. Entry *i* may only inject a key provided
/// by an entry before it, which makes the dependency graph a DAG by
/// construction — a cycle is refused at mount, and generating one would be
/// testing the refusal rather than the properties.
fn compose(rng: &mut Rng, count: usize) -> Vec<Entry> {
    let mut entries = Vec::with_capacity(count);
    let mut offered: Vec<&'static str> = Vec::new();
    for _ in 0..count {
        let mut inject = Vec::new();
        for key in &offered {
            if rng.chance(3) {
                inject.push(*key);
            }
        }
        let mut provides = Vec::new();
        if rng.chance(2) {
            let key = KEYS[rng.below(KEYS.len())];
            if !offered.contains(&key) {
                provides.push(key);
                offered.push(key);
            }
        }
        entries.push(Entry {
            inject,
            provides,
            enabled: !rng.chance(4),
        });
    }
    entries
}

// ------------------------------------------------------------------ Theorem 73

/// Where a composition ends up depends on the configuration it ends at, not on
/// the route it took to get there.
///
/// This is the property the whole idea of reconciliation rests on. Without it,
/// editing a composition file would have to mean tearing everything down and
/// building it again, because no incremental path could be trusted to land in
/// the same place.
#[tokio::test]
async fn the_quiescent_state_is_a_function_of_the_final_composition() {
    for seed in 1..400u64 {
        let mut rng = Rng(seed);
        let count = 2 + rng.below(5);
        let mut entries = compose(&mut rng, count);

        // Take a route: mount everything, then toggle entries at random.
        let live = Arc::new(AtomicUsize::new(0));
        let (rt, mut uids) = from_scratch(&entries, &live).await;
        for _ in 0..(2 + rng.below(6)) {
            let index = rng.below(entries.len());
            entries[index].enabled = !entries[index].enabled;
            match (uids[index], entries[index].enabled) {
                (Some(uid), false) => rt.unmount(uid).await,
                (Some(uid), true) => rt.enable(uid).await,
                (None, true) => {
                    uids[index] = rt
                        .mount(part(index, &entries[index], &live), Value::Null)
                        .ok();
                    rt.settle().await;
                }
                (None, false) => {}
            }
        }
        let taken = observe(&rt, &|uid| uids.iter().position(|slot| *slot == Some(uid))).await;

        // Now load the configuration it ended at, from nothing.
        let fresh_live = Arc::new(AtomicUsize::new(0));
        let (fresh_rt, fresh_uids) = from_scratch(&entries, &fresh_live).await;
        let direct = observe(&fresh_rt, &|uid| {
            fresh_uids.iter().position(|slot| *slot == Some(uid))
        })
        .await;

        assert_eq!(
            taken, direct,
            "seed {seed}: the route changed the destination"
        );
    }
}

// ---------------------------------------------------------------- Corollary 62

/// A fiber that leaves contributes nothing to the state it leaves behind.
///
/// This is what makes rebuilding one entry safe: withdrawing what its fiber
/// installed leaves the fibers around it exactly as they were.
#[tokio::test]
async fn a_departing_fiber_leaves_nothing_of_itself_behind() {
    for seed in 1..400u64 {
        let mut rng = Rng(seed);
        let count = 2 + rng.below(4);
        let entries = compose(&mut rng, count);
        let live = Arc::new(AtomicUsize::new(0));
        let (rt, uids) = from_scratch(&entries, &live).await;

        let index_of = |uid: Uid| uids.iter().position(|slot| *slot == Some(uid));
        let before = observe(&rt, &index_of).await;
        let live_before = live.load(Ordering::SeqCst);

        // Add one more, then take it away again.
        let extra = Entry {
            inject: Vec::new(),
            provides: Vec::new(),
            enabled: true,
        };
        let uid = rt
            .mount(part(999, &extra, &live), Value::Null)
            .expect("an entry with no dependencies always mounts");
        rt.settle().await;
        rt.unmount(uid).await;

        let after = observe(&rt, &index_of).await;
        assert_eq!(before, after, "seed {seed}: a departed fiber left a trace");
        assert_eq!(
            live_before,
            live.load(Ordering::SeqCst),
            "seed {seed}: a departed fiber left an effect standing"
        );
    }
}

// ------------------------------------------------------------------ Theorem 66

/// Every sequence quiesces: nothing is left pending, and nothing spins.
///
/// A fiber only ever waits on dependents that have already stopped being
/// satisfiable, and a dependent that is itself a provider waits the same way
/// for its own, so the graph is walked on demand rather than cycled through.
#[tokio::test]
async fn every_sequence_settles() {
    for seed in 1..400u64 {
        let mut rng = Rng(seed);
        let count = 2 + rng.below(5);
        let entries = compose(&mut rng, count);
        let live = Arc::new(AtomicUsize::new(0));
        let (rt, uids) = from_scratch(&entries, &live).await;

        for _ in 0..(3 + rng.below(8)) {
            let index = rng.below(uids.len());
            if let Some(uid) = uids[index] {
                if rng.chance(2) {
                    rt.unmount(uid).await;
                } else {
                    rt.enable(uid).await;
                }
            }
        }

        // `settle` returning is the claim; a runtime that did not quiesce would
        // not have got here. What is left to check is that it settled into a
        // state with no transition half-finished.
        rt.settle().await;
        for status in rt.roster() {
            assert!(
                !matches!(
                    status.state,
                    aphid_agent::rt::State::Loading | aphid_agent::rt::State::Unloading
                ),
                "seed {seed}: `{}` settled mid-transition as {}",
                status.name,
                status.state
            );
        }

        // And that shutdown reaches nothing-left, with every effect reverted.
        rt.shutdown().await;
        assert_eq!(
            live.load(Ordering::SeqCst),
            0,
            "seed {seed}: effects survived shutdown"
        );
        assert!(
            rt.bindings().is_empty(),
            "seed {seed}: bindings survived shutdown"
        );
    }
}
