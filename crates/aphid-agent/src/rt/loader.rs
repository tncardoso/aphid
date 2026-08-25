//! A composition described as data.
//!
//! The imperative side — mount this, unload that — is what a component uses.
//! An operator wants the other thing: to say what the system *should be* and
//! have the runtime work out the difference. That is an [`Entry`] per fiber and
//! a [`Loader`] that reconciles.
//!
//! # Why reconciling incrementally is safe
//!
//! Because where a composition ends up is a function of the configuration it
//! ends at, not of the route it took to get there. Whatever the loader mounts
//! and unmounts on the way, and in whatever order, the system settles where
//! loading the final configuration from nothing would have left it. That is
//! not a hope about this code; it is the property
//! `crates/aphid-agent/tests/rt_metatheory.rs` checks against a few thousand
//! generated routes.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::Value;

use super::component::Component;
use super::composition::Composition;
use super::isolate::{Realm, Realms};
use super::uid::Uid;

/// How an entry wants a service key resolved.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Isolate {
    /// A realm of its own, named after the entry, carried wherever it moves.
    Local,
    /// A realm shared with every entry naming the same thing. Moving such an
    /// entry changes who it shares a binding with rather than which realm it
    /// is in.
    Shared(String),
}

/// One fiber, described.
///
/// What an entry records is exactly what supports a fiber, which is what lets
/// it be a faithful specification rather than a summary: `disabled` says
/// whether it should run, the tree says who its parent is, and `url` selects
/// the component, which declares what it needs and what it offers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// Stable identity. The reconciliation key — an edit to an entry is told
    /// from a removal plus an addition by this and nothing else.
    pub id: String,
    /// What to instantiate. A path, a package name, whatever the resolver
    /// understands.
    pub url: String,
    pub config: Value,
    /// Administratively off. Distinct from a fiber that is merely waiting.
    pub disabled: bool,
    /// Per-key realm assignment.
    pub isolate: BTreeMap<String, Isolate>,
}

impl Entry {
    #[must_use]
    pub fn new(id: impl Into<String>, url: impl Into<String>) -> Entry {
        Entry {
            id: id.into(),
            url: url.into(),
            config: Value::Null,
            disabled: false,
            isolate: BTreeMap::new(),
        }
    }

    /// The realm table this entry asks for.
    fn realms(&self) -> Arc<Realms> {
        Realms::assigned(self.isolate.iter().map(|(key, isolate)| {
            // Leaked because a coeffect key is `&'static str` and this one came
            // from a file. There are tens of them and they outlive the process
            // only in the sense that the process ends first.
            let key: &'static str = Box::leak(key.clone().into_boxed_str());
            let realm = match isolate {
                Isolate::Local => Realm::shared(key, format!("entry:{}", self.id)),
                Isolate::Shared(name) => Realm::shared(key, name),
            };
            (key, realm)
        }))
    }
}

/// Turns an entry's `url` into something mountable.
///
/// The loader knows about identity, ordering and reconciliation; it knows
/// nothing about what a plugin is. That is the host's business.
pub trait Resolver: Send + Sync + 'static {
    /// # Errors
    ///
    /// A message for the operator: a path that does not exist, a file that does
    /// not compile, a name nothing answers to.
    fn resolve(&self, entry: &Entry) -> Result<Arc<dyn Component>, String>;
}

/// What a reconciliation did, and what it could not do.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Report {
    pub mounted: Vec<String>,
    pub unmounted: Vec<String>,
    pub reloaded: Vec<String>,
    /// Entries that could not be brought up, and why. The others still are:
    /// one bad entry is worth saying out loud, not worth refusing to start
    /// over.
    pub failed: Vec<(String, String)>,
}

impl Report {
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.mounted.is_empty()
            && self.unmounted.is_empty()
            && self.reloaded.is_empty()
            && self.failed.is_empty()
    }
}

/// Keeps a composition in step with a list of entries.
pub struct Loader {
    composition: Composition,
    resolver: Arc<dyn Resolver>,
    live: HashMap<String, Live>,
}

struct Live {
    entry: Entry,
    uid: Uid,
}

impl Loader {
    #[must_use]
    pub fn new(composition: &Composition, resolver: Arc<dyn Resolver>) -> Loader {
        Loader {
            composition: composition.clone(),
            resolver,
            live: HashMap::new(),
        }
    }

    /// What is loaded right now, by entry id.
    #[must_use]
    pub fn loaded(&self) -> Vec<(&str, Uid)> {
        let mut loaded: Vec<(&str, Uid)> = self
            .live
            .iter()
            .map(|(id, live)| (id.as_str(), live.uid))
            .collect();
        loaded.sort_by_key(|(id, _)| *id);
        loaded
    }

    /// Bring the composition to `entries`.
    ///
    /// Each field gets the least disruptive treatment that is correct for it:
    /// `url` rebuilds because the component itself changed, `disabled` unloads
    /// and reloads, `isolate` reassigns realms without rebuilding, and `config`
    /// rebuilds because a component receives its configuration when it applies.
    pub async fn reconcile(&mut self, entries: Vec<Entry>) -> Report {
        let mut report = Report::default();
        let wanted: HashMap<&str, &Entry> = entries
            .iter()
            .map(|entry| (entry.id.as_str(), entry))
            .collect();

        // Gone entirely.
        let departed: Vec<String> = self
            .live
            .keys()
            .filter(|id| !wanted.contains_key(id.as_str()))
            .cloned()
            .collect();
        for id in departed {
            if let Some(live) = self.live.remove(&id) {
                self.composition.runtime.unmount(live.uid).await;
                report.unmounted.push(id);
            }
        }

        for entry in &entries {
            match self.live.get(&entry.id) {
                None => self.mount(entry, &mut report).await,
                Some(live) if live.entry == *entry => {}
                Some(_) => self.update(entry, &mut report).await,
            }
        }

        report
    }

    async fn mount(&mut self, entry: &Entry, report: &mut Report) {
        if entry.disabled {
            return;
        }
        let component = match self.resolver.resolve(entry) {
            Ok(component) => component,
            Err(error) => {
                report.failed.push((entry.id.clone(), error));
                return;
            }
        };
        match self
            .composition
            .runtime
            .mount_in(component, entry.config.clone(), entry.realms())
        {
            Ok(uid) => {
                self.composition.runtime.settle().await;
                self.live.insert(
                    entry.id.clone(),
                    Live {
                        entry: entry.clone(),
                        uid,
                    },
                );
                report.mounted.push(entry.id.clone());
            }
            Err(error) => report.failed.push((entry.id.clone(), error)),
        }
    }

    async fn update(&mut self, entry: &Entry, report: &mut Report) {
        let Some(live) = self.live.get(&entry.id) else {
            return;
        };
        let uid = live.uid;
        let previous = live.entry.clone();

        // Off, or turned back on: no rebuilding either way.
        if previous.disabled != entry.disabled
            && previous.url == entry.url
            && previous.config == entry.config
            && previous.isolate == entry.isolate
        {
            if entry.disabled {
                self.composition.runtime.unmount(uid).await;
                report.unmounted.push(entry.id.clone());
            } else {
                self.composition.runtime.enable(uid).await;
                report.mounted.push(entry.id.clone());
            }
            self.live.insert(
                entry.id.clone(),
                Live {
                    entry: entry.clone(),
                    uid,
                },
            );
            return;
        }

        // Only the realms moved: reassign rather than rebuild, so nothing this
        // entry installed is torn down and put back identical.
        if previous.url == entry.url
            && previous.config == entry.config
            && previous.disabled == entry.disabled
        {
            self.composition.runtime.reassign(uid, entry.realms()).await;
            self.live.insert(
                entry.id.clone(),
                Live {
                    entry: entry.clone(),
                    uid,
                },
            );
            report.reloaded.push(entry.id.clone());
            return;
        }

        // The component or its configuration changed, so this is a different
        // fiber wearing the same name.
        self.composition.runtime.unmount(uid).await;
        self.live.remove(&entry.id);
        self.mount(entry, report).await;
        if report.mounted.last().is_some_and(|id| *id == entry.id) {
            report.mounted.pop();
            report.reloaded.push(entry.id.clone());
        }
    }
}

impl std::fmt::Debug for Loader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loader")
            .field("loaded", &self.loaded().len())
            .finish()
    }
}
