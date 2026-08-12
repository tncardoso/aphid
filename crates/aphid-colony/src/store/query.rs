//! A [`Selector`] as SQL.
//!
//! Building the string is kept apart from running it so the shape of every
//! query a colony asks can be tested without a database — and, more to the
//! point, so there is one place to look when the SQL and
//! [`aphid_nostr::filter::matches_live`] disagree about what a filter meant.
//! They must not: the stored half of a subscription and the live half answer
//! the same question, and a message that slips between them is one nobody ever
//! sees.

use aphid_nostr::Selector;
use rusqlite::types::Value;

/// What the query is for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum What {
    /// The events themselves, newest first, up to the selector's limit.
    Events,
    /// How many there are. No order and no limit: a count of the newest five
    /// hundred is not a count.
    Count,
}

/// The order NIP-01 implies, and the one a group is rebuilt in.
///
/// One rule for the whole crate, so a tie between two events at the same second
/// is broken the same way everywhere.
pub const NEWEST_FIRST: &str = " ORDER BY created_at DESC, id ASC";
pub const OLDEST_FIRST: &str = " ORDER BY created_at ASC, id ASC";

/// Build the statement and the values to bind to it.
///
/// A void selector is the caller's problem: it asked for nothing, and the store
/// answers with nothing rather than running a query that cannot be written.
#[must_use]
pub fn build(selector: &Selector, what: What) -> (String, Vec<Value>) {
    let mut sql = String::from(match what {
        What::Events => "SELECT json FROM events WHERE 1 = 1",
        What::Count => "SELECT COUNT(*) FROM events WHERE 1 = 1",
    });
    let mut values = Vec::new();

    if !selector.ids.is_empty() {
        sql.push_str(" AND id IN ");
        list(&mut sql, selector.ids.len());
        values.extend(
            selector
                .ids
                .iter()
                .map(|id| Value::Blob(id.as_bytes().to_vec())),
        );
    }

    if !selector.authors.is_empty() {
        sql.push_str(" AND pubkey IN ");
        list(&mut sql, selector.authors.len());
        values.extend(
            selector
                .authors
                .iter()
                .map(|author| Value::Blob(author.as_bytes().to_vec())),
        );
    }

    if !selector.kinds.is_empty() {
        sql.push_str(" AND kind IN ");
        list(&mut sql, selector.kinds.len());
        values.extend(
            selector
                .kinds
                .iter()
                .map(|kind| Value::Integer(i64::from(kind.as_u16()))),
        );
    }

    if let Some(since) = selector.since {
        sql.push_str(" AND created_at >= ?");
        values.push(Value::Integer(seconds(since.as_secs())));
    }
    if let Some(until) = selector.until {
        sql.push_str(" AND created_at <= ?");
        values.push(Value::Integer(seconds(until.as_secs())));
    }

    // One sub-select for each letter. Letters are ANDed and values within a
    // letter are ORed, which is exactly NIP-01's rule. A join for each letter
    // would multiply rows and need a DISTINCT to put them back; this reads the
    // tags index once for each letter and hands back a set of ids.
    for (letter, wanted) in &selector.tags {
        sql.push_str(" AND id IN (SELECT event FROM tags WHERE letter = ? AND value IN ");
        list(&mut sql, wanted.len());
        sql.push(')');
        values.push(Value::Text(letter.to_string()));
        values.extend(wanted.iter().map(|value| Value::Text(value.clone())));
    }

    if what == What::Events {
        sql.push_str(NEWEST_FIRST);
        sql.push_str(" LIMIT ?");
        values.push(Value::Integer(
            i64::try_from(selector.limit).unwrap_or(i64::MAX),
        ));
    }

    (sql, values)
}

/// `(?, ?, ?)`, with one for each value.
fn list(sql: &mut String, count: usize) {
    sql.push('(');
    for at in 0..count {
        if at > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
    }
    sql.push(')');
}

/// A nostr timestamp as SQLite holds it.
///
/// A time past the year 292 277 026 596 is not a time a colony has to carry, and
/// saturating is better than a query that will not build.
fn seconds(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use aphid_nostr::nostr::event::{EventId, Kind};
    use aphid_nostr::nostr::types::Timestamp;

    use super::*;

    fn selector() -> Selector {
        Selector {
            limit: 10,
            ..Selector::default()
        }
    }

    #[test]
    fn a_selector_that_names_nothing_asks_for_everything() {
        let (sql, values) = build(&selector(), What::Events);
        assert_eq!(
            sql,
            format!("SELECT json FROM events WHERE 1 = 1{NEWEST_FIRST} LIMIT ?")
        );
        assert_eq!(values, vec![Value::Integer(10)]);
    }

    #[test]
    fn each_letter_gets_its_own_sub_select() {
        let selector = Selector {
            tags: vec![
                ('h', vec!["general".to_owned()]),
                ('p', vec!["aa".to_owned(), "bb".to_owned()]),
            ],
            ..selector()
        };
        let (sql, values) = build(&selector, What::Events);

        assert_eq!(sql.matches("SELECT event FROM tags").count(), 2);
        assert!(sql.contains("value IN (?)"), "{sql}");
        assert!(sql.contains("value IN (?, ?)"), "{sql}");
        assert_eq!(
            values,
            vec![
                Value::Text("h".to_owned()),
                Value::Text("general".to_owned()),
                Value::Text("p".to_owned()),
                Value::Text("aa".to_owned()),
                Value::Text("bb".to_owned()),
                Value::Integer(10),
            ]
        );
    }

    #[test]
    fn a_count_has_no_order_and_no_limit() {
        let (sql, values) = build(&selector(), What::Count);
        assert!(sql.starts_with("SELECT COUNT(*)"), "{sql}");
        assert!(!sql.contains("ORDER BY"), "{sql}");
        assert!(!sql.contains("LIMIT"), "{sql}");
        assert!(values.is_empty());
    }

    #[test]
    fn every_field_binds_in_the_order_it_is_written() {
        let selector = Selector {
            ids: vec![EventId::from_byte_array([1; 32])],
            kinds: vec![Kind::from_u16(9)],
            since: Some(Timestamp::from_secs(100)),
            until: Some(Timestamp::from_secs(200)),
            ..selector()
        };
        let (sql, values) = build(&selector, What::Events);

        assert!(sql.contains("id IN (?)"), "{sql}");
        assert!(sql.contains("kind IN (?)"), "{sql}");
        assert_eq!(values.len(), 5, "one id, one kind, since, until, limit");
        assert_eq!(values[1], Value::Integer(9));
        assert_eq!(values[2], Value::Integer(100));
        assert_eq!(values[3], Value::Integer(200));
    }
}
