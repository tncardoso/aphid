//! Realms: the indirection that lets one key resolve to independent bindings.
//!
//! A key does not name a binding directly. It names a **realm**, and the realm
//! names the binding: `k → ρ(k) → σ(ρ(k))`. Two contexts that map the same key
//! to different realms see independent providers of it, which is what lets one
//! subtree run against a different `shell`, a different `sink`, a different
//! model backend, without either subtree knowing.
//!
//! The indirection is here from the first commit even though most keys never
//! use it. Retrofitting it later would mean touching every read, every write,
//! every notification **and** the guard condition on unloading — it is not a
//! layer that sits on top, it is a condition inside.
//!
//! # Delimiters
//!
//! Each key also carries a delimiter tag, written on a context and inherited by
//! every context derived from it. Two contexts agree on a key's tag exactly
//! when they were derived within one isolate scope for that key — which is how
//! a realm reassignment tells "this binding is mine and moves with me" from
//! "this binding belongs to somebody I merely share a realm with".

use std::collections::HashMap;
use std::sync::Arc;

use compact_str::{CompactString, format_compact};

use super::uid::Uid;

/// Where a key's binding actually lives.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Realm {
    /// The realm a key resolves to when nothing isolated it. Deterministic in
    /// the key, so no allocation and no lazy registration on the read path.
    Root(&'static str),
    /// A realm somebody asked for by name.
    ///
    /// The name is what decides sharing, and it is why this is a name rather
    /// than a generated id: a realm private to one entry is named after that
    /// entry, and a realm two entries mean to share is named by both. Moving an
    /// entry then changes which entries it shares a binding with, rather than
    /// which realm it belongs to.
    Named(&'static str, CompactString),
}

impl Realm {
    /// A realm nobody else will name, for a scope that wants one to itself.
    #[must_use]
    pub fn local(key: &'static str) -> Realm {
        Realm::Named(key, format_compact!("@{}", Uid::fresh().get()))
    }

    /// A realm shared with everyone naming the same thing.
    #[must_use]
    pub fn shared(key: &'static str, name: impl AsRef<str>) -> Realm {
        Realm::Named(key, CompactString::new(name.as_ref()))
    }

    #[must_use]
    pub fn key(&self) -> &'static str {
        match self {
            Realm::Root(key) | Realm::Named(key, _) => key,
        }
    }

    /// The name, for a root realm too — where it is the key itself.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Realm::Root(key) => key,
            Realm::Named(_, name) => name.as_str(),
        }
    }
}

/// One context's realm table, ρ, together with its delimiters.
///
/// Materialised in full rather than chained to a parent: component counts are
/// in the tens, and a flat map keeps every read one lookup instead of a walk.
#[derive(Clone, Default, Debug)]
pub struct Realms {
    entries: HashMap<&'static str, Realm>,
}

impl Realms {
    /// The table a root context carries: every key resolves to its root realm.
    #[must_use]
    pub fn root() -> Arc<Realms> {
        Arc::new(Realms::default())
    }

    /// ρ(k).
    #[must_use]
    pub fn realm(&self, key: &'static str) -> Realm {
        self.entries.get(key).cloned().unwrap_or(Realm::Root(key))
    }

    /// Derive a table that sends `key` to a realm of its own.
    ///
    /// Recovery is implicit: the parent table is untouched, so discarding the
    /// derived context is the whole of the inverse. There is nothing to run.
    #[must_use]
    pub fn isolated(&self, key: &'static str, realm: Realm) -> Arc<Realms> {
        let mut entries = self.entries.clone();
        entries.insert(key, realm);
        Arc::new(Realms { entries })
    }

    /// Whether two tables send a key to the same binding.
    #[must_use]
    pub fn agrees_with(&self, other: &Realms, key: &'static str) -> bool {
        self.realm(key) == other.realm(key)
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries.keys().copied()
    }

    /// Build a table from a whole mapping at once.
    #[must_use]
    pub fn assigned(mapping: impl IntoIterator<Item = (&'static str, Realm)>) -> Arc<Realms> {
        Arc::new(Realms {
            entries: mapping.into_iter().collect(),
        })
    }

    /// The keys where two tables disagree.
    #[must_use]
    pub fn divergence(&self, other: &Realms) -> Vec<&'static str> {
        let mut keys: Vec<&'static str> = self
            .entries
            .keys()
            .chain(other.entries.keys())
            .copied()
            .filter(|key| self.realm(key) != other.realm(key))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }
}
