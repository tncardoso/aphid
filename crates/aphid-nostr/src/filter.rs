//! Reading a NIP-01 filter the way a relay has to read one.
//!
//! A subscription is answered in two phases: what is already stored, and then
//! what arrives afterwards. The stored phase is a database query and the live
//! phase is a predicate in memory, and **the two must agree about what was
//! asked for**. Anywhere they disagree, a message either arrives twice or never
//! arrives at all, and neither shows up until it is a person saying "it
//! sometimes just does not send".
//!
//! So both phases are built here. [`matches_live`] is the predicate; [`Selector`]
//! is the same filter reduced to what an index can answer, and the store builds
//! its SQL from that rather than from the filter, so the shape of a query is
//! decided once and can be tested without a database.
//!
//! The limits are here for the same reason: a relay that clamps a limit in the
//! query and forgets to clamp it in the predicate has two different filters.

use std::collections::BTreeSet;

use nostr::event::{Event, EventId, Kind};
use nostr::filter::{Filter, MatchEventOptions};
use nostr::key::PublicKey;
use nostr::types::Timestamp;

use crate::Error;

/// The most events one filter may ask for.
pub const MAX_LIMIT: usize = 5_000;

/// What a filter that names no limit gets.
pub const DEFAULT_LIMIT: usize = 500;

/// The most filters one `REQ` may carry.
pub const MAX_FILTERS: usize = 16;

/// The most subscriptions one connection may hold open.
pub const MAX_SUBSCRIPTIONS: usize = 32;

/// Whether a filter can never match anything.
///
/// `{"ids": []}` asked for no ids, not for every id. The `nostr` crate reads an
/// empty list as "no constraint"; every relay reads it as "nothing". This takes
/// the relay's reading, and takes it in **one** place, because the stored phase
/// is SQL — where `IN ()` matches nothing on its own — and the two phases of one
/// subscription must not disagree about what was asked for.
#[must_use]
pub fn is_void(filter: &Filter) -> bool {
    fn empty<T>(set: Option<&BTreeSet<T>>) -> bool {
        set.is_some_and(BTreeSet::is_empty)
    }

    empty(filter.ids.as_ref())
        || empty(filter.authors.as_ref())
        || empty(filter.kinds.as_ref())
        || filter.generic_tags.values().any(BTreeSet::is_empty)
}

/// Whether a live event should be sent to a subscription on this filter.
///
/// `limit` is deliberately not consulted. NIP-01 counts it against stored
/// events only, and a subscription that has drained its limit still follows the
/// group it asked about — a chat that stopped updating after five hundred lines
/// would be worse than one that never started.
#[must_use]
pub fn matches_live(filter: &Filter, event: &Event) -> bool {
    !is_void(filter) && filter.match_event(event, MatchEventOptions::default())
}

/// Whether any filter in a `REQ` matches. A `REQ` is an OR of its filters.
#[must_use]
pub fn any_matches(filters: &[Filter], event: &Event) -> bool {
    filters.iter().any(|filter| matches_live(filter, event))
}

/// A filter reduced to what an index can answer.
///
/// Every list is a `Vec` and not a set: they are already unique, coming out of
/// the filter's `BTreeSet`s, and what happens next is that they are bound to
/// query parameters in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selector {
    pub ids: Vec<EventId>,
    pub authors: Vec<PublicKey>,
    pub kinds: Vec<Kind>,
    /// Single-letter tags, by letter, sorted. Every letter must match and any
    /// value within a letter will do, which is NIP-01's rule stated as a type.
    pub tags: Vec<(char, Vec<String>)>,
    pub since: Option<Timestamp>,
    pub until: Option<Timestamp>,
    /// Always set: [`DEFAULT_LIMIT`] when the filter named none, clamped to
    /// [`MAX_LIMIT`] when it named more.
    pub limit: usize,
    /// True when the filter asked for nothing at all. The store answers with an
    /// empty result rather than building a query that cannot be written.
    pub void: bool,
}

impl Selector {
    /// Reduce one filter.
    ///
    /// # Errors
    ///
    /// Fails when the filter carries a `search`, which this relay does not
    /// implement.
    pub fn from_filter(filter: &Filter) -> Result<Self, Error> {
        if filter.search.is_some() {
            return Err(Error::Search);
        }

        let tags = filter
            .generic_tags
            .iter()
            .map(|(letter, values)| (letter.as_char(), values.iter().cloned().collect()))
            .collect();

        Ok(Self {
            ids: filter.ids.iter().flatten().copied().collect(),
            authors: filter.authors.iter().flatten().copied().collect(),
            kinds: filter.kinds.iter().flatten().copied().collect(),
            tags,
            since: filter.since,
            until: filter.until,
            limit: filter.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT),
            void: is_void(filter),
        })
    }
}
