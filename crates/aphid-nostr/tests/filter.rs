//! What a filter asked for.
//!
//! These are the rules the store's SQL has to agree with. Each one is written
//! here first because it is cheaper to argue with a predicate than with a query
//! plan.

use aphid_nostr::filter::{self, DEFAULT_LIMIT, MAX_LIMIT, Selector};
use aphid_nostr::nostr::event::{Event, EventBuilder, FinalizeEvent, Kind, Tag};
use aphid_nostr::nostr::filter::{Filter, SingleLetterTag};
use aphid_nostr::nostr::key::Keys;
use aphid_nostr::nostr::types::Timestamp;

fn said(keys: &Keys, kind: u16, tags: Vec<Tag>, at: u64) -> Event {
    EventBuilder::new(Kind::from_u16(kind), "hello")
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(at))
        .finalize(keys)
        .expect("a freshly built event signs")
}

fn h(group: &str) -> Tag {
    Tag::custom("h", [group.to_owned()])
}

fn letter(character: char) -> SingleLetterTag {
    SingleLetterTag::from_char(character).expect("a letter")
}

#[test]
fn an_empty_list_matches_nothing() {
    let keys = Keys::generate();
    let event = said(&keys, 9, vec![h("general")], 100);

    for filter in [
        Filter::new().ids(Vec::new()),
        Filter::new().authors(Vec::new()),
        Filter::new().kinds(Vec::new()),
    ] {
        assert!(
            filter::is_void(&filter),
            "an empty list asked for nothing: {filter:?}"
        );
        assert!(
            !filter::matches_live(&filter, &event),
            "and nothing is what it gets: {filter:?}"
        );
        let selector = Selector::from_filter(&filter).expect("reduces");
        assert!(selector.void);
    }
}

#[test]
fn a_filter_that_names_nothing_matches_everything() {
    let keys = Keys::generate();
    let event = said(&keys, 9, vec![h("general")], 100);
    assert!(!filter::is_void(&Filter::new()));
    assert!(filter::matches_live(&Filter::new(), &event));
}

#[test]
fn letters_are_anded_and_values_are_ored() {
    let keys = Keys::generate();
    let other = Keys::generate();
    let mentioned = Tag::public_key(other.public_key());
    let event = said(&keys, 9, vec![h("general"), mentioned], 100);

    // Both letters are on the event, so both are satisfied.
    let both = Filter::new()
        .custom_tags(letter('h'), ["general".to_owned()])
        .custom_tags(letter('p'), [other.public_key().to_hex()]);
    assert!(filter::matches_live(&both, &event));

    // One letter is right and the other is not: an AND fails.
    let wrong = Filter::new()
        .custom_tags(letter('h'), ["general".to_owned()])
        .custom_tags(letter('p'), [keys.public_key().to_hex()]);
    assert!(!filter::matches_live(&wrong, &event));

    // Two values for one letter: an OR passes on either.
    let either = Filter::new().custom_tags(letter('h'), ["build".to_owned(), "general".to_owned()]);
    assert!(filter::matches_live(&either, &event));
}

#[test]
fn since_and_until_are_inclusive() {
    let keys = Keys::generate();
    let event = said(&keys, 9, vec![h("general")], 100);

    assert!(filter::matches_live(
        &Filter::new().since(Timestamp::from_secs(100)),
        &event
    ));
    assert!(filter::matches_live(
        &Filter::new().until(Timestamp::from_secs(100)),
        &event
    ));
    assert!(!filter::matches_live(
        &Filter::new().since(Timestamp::from_secs(101)),
        &event
    ));
    assert!(!filter::matches_live(
        &Filter::new().until(Timestamp::from_secs(99)),
        &event
    ));
}

#[test]
fn a_live_event_is_not_dropped_by_a_limit() {
    let keys = Keys::generate();
    let event = said(&keys, 9, vec![h("general")], 100);
    // NIP-01 counts `limit` against stored events only. A subscription that has
    // drained its limit still follows the group it asked about.
    assert!(filter::matches_live(&Filter::new().limit(0), &event));
}

#[test]
fn any_of_the_filters_will_do() {
    let keys = Keys::generate();
    let event = said(&keys, 9, vec![h("general")], 100);
    let filters = vec![
        Filter::new().kind(Kind::from_u16(1)),
        Filter::new().kind(Kind::from_u16(9)),
    ];
    assert!(filter::any_matches(&filters, &event));
    assert!(!filter::any_matches(&filters[..1], &event));
}

#[test]
fn a_selector_clamps_a_greedy_limit() {
    let selector = Selector::from_filter(&Filter::new().limit(MAX_LIMIT * 10)).expect("reduces");
    assert_eq!(selector.limit, MAX_LIMIT);
}

#[test]
fn a_filter_with_no_limit_gets_a_default() {
    let selector = Selector::from_filter(&Filter::new()).expect("reduces");
    assert_eq!(selector.limit, DEFAULT_LIMIT);
    assert!(!selector.void);
}

#[test]
fn a_search_filter_is_refused_rather_than_ignored() {
    let error = Selector::from_filter(&Filter::new().search("anything"))
        .expect_err("this relay does not search");
    assert!(error.to_string().contains("does not search"), "{error}");
}

#[test]
fn a_selector_carries_every_letter_it_was_given() {
    let filter = Filter::new()
        .custom_tags(letter('h'), ["general".to_owned()])
        .custom_tags(letter('p'), ["ff".to_owned()]);
    let selector = Selector::from_filter(&filter).expect("reduces");
    let letters: Vec<char> = selector.tags.iter().map(|(letter, _)| *letter).collect();
    assert_eq!(letters, vec!['h', 'p']);
}
