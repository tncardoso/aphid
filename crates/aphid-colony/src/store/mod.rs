//! Every event, in SQLite.
//!
//! This is the workspace's first database, and it is here because a relay has
//! to answer a `REQ` — a filter over everything it has ever been told, with an
//! order and a limit. The rest of aphid keeps its state in files it can walk;
//! this cannot, and an index bolted onto a walk is the thing
//! [`aphid_alate::memory::store`] says not to build.
//!
//! What is stored is the **event exactly as it arrived**. What goes back on the
//! wire is that same string: an event is signed over its own encoding, and
//! re-serializing it risks a byte that is not the one the signature covers.
//!
//! One connection behind a mutex, reached from the relay through
//! `spawn_blocking`. Writes serialize in SQLite anyway and a hub does a handful
//! of messages a second, so a pool would be one more thing to be wrong. The day
//! a `COUNT` blocks a message, the answer is a pool of readers beside the one
//! writer, and that is a change inside this module and nowhere else.
//!
//! [`aphid_alate::memory::store`]: https://docs.rs/aphid-alate

mod query;
mod schema;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use aphid_nostr::Selector;
use aphid_nostr::group;
use aphid_nostr::nostr::event::{Event, Kind};
use aphid_nostr::nostr::key::PublicKey;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};

pub use query::{NEWEST_FIRST, OLDEST_FIRST, What};
pub use schema::SCHEMA;

/// Why the store would not answer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("the colony database: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("{path}: schema {found} was written by a newer aphid; this one understands {SCHEMA}")]
    Version { path: PathBuf, found: u32 },
    #[error("a stored event will not read back, which should not happen: {0}")]
    Malformed(String),
}

/// What became of an event that was offered.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Saved {
    /// Written, and worth sending to whoever is subscribed.
    Stored,
    /// Already here. NIP-01 wants this answered `OK true` with a note, because
    /// the event did arrive and the client should stop sending it.
    Duplicate,
    /// Replaceable or addressable, and older than what is already stored.
    /// Accepted on the wire and kept nowhere, which is what the NIP asks for.
    Superseded,
    /// Ephemeral. Fanned out, never written.
    NotStored,
}

impl Saved {
    /// Whether this event should go out to live subscriptions.
    #[must_use]
    pub const fn fans_out(self) -> bool {
        matches!(self, Self::Stored | Self::NotStored)
    }
}

/// Every event a colony has been told.
#[derive(Debug)]
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    /// Open the file at `path`, creating it if it is not there.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be opened, when the schema cannot be made, or
    /// when the file was written by a newer aphid.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let connection = Connection::open(path).map_err(|source| Error::Open {
            path: path.to_path_buf(),
            source,
        })?;
        Self::prepare(connection, path)
    }

    /// A store that lives only as long as it is held.
    ///
    /// This is the test double. A real SQLite opens in microseconds and answers
    /// the questions a fake would have to be taught, so there is no trait here
    /// and nothing to keep in step.
    ///
    /// # Errors
    ///
    /// Fails when the schema cannot be made.
    pub fn open_in_memory() -> Result<Self, Error> {
        let connection = Connection::open_in_memory()?;
        Self::prepare(connection, Path::new(":memory:"))
    }

    fn prepare(connection: Connection, path: &Path) -> Result<Self, Error> {
        connection.execute_batch(schema::PRAGMAS)?;
        connection.execute_batch(schema::CREATE)?;

        let found: Option<u32> = connection
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [schema::KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|value| value.parse().ok());

        match found {
            Some(found) if found > SCHEMA => {
                return Err(Error::Version {
                    path: path.to_path_buf(),
                    found,
                });
            }
            Some(_) => {}
            None => {
                connection.execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                    params![schema::KEY, SCHEMA.to_string()],
                )?;
            }
        }

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Take the lock, and take it even when another thread panicked holding it.
    ///
    /// The same tolerance [`aphid_alate::cron`] takes: the file is whole either
    /// way, and refusing every later message would turn one panic into a colony
    /// that never carries anything again.
    ///
    /// [`aphid_alate::cron`]: https://docs.rs/aphid-alate
    fn lock(&self) -> MutexGuard<'_, Connection> {
        match self.connection.lock() {
            Ok(connection) => connection,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Store one event.
    ///
    /// The event must already have had its id and its signature checked. This
    /// decides only what NIP-01 decides: whether it is new, already here, or
    /// older than what replaced it.
    ///
    /// # Errors
    ///
    /// Fails when the write fails.
    pub fn save(&self, event: &Event) -> Result<Saved, Error> {
        if event.kind.is_ephemeral() {
            return Ok(Saved::NotStored);
        }

        let id = event.id.as_bytes().to_vec();
        let pubkey = event.pubkey.as_bytes().to_vec();
        let created_at = clamp(event.created_at.as_secs());
        let kind = i64::from(event.kind.as_u16());
        // The `d` is part of what makes an addressable event unique, so it is a
        // column and not a row in `tags`. Everything else has an empty one,
        // which lets one predicate serve both classes.
        let identifier = if event.kind.is_addressable() {
            event.tags.identifier().unwrap_or_default().to_string()
        } else {
            String::new()
        };

        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if event.kind.is_replaceable() || event.kind.is_addressable() {
            // Drop what this replaces, but only what is really older. NIP-01
            // breaks a tie on `created_at` with the **lower** id, so an event
            // that ties and loses finds the old one still there.
            transaction.execute(
                "DELETE FROM events
                  WHERE kind = ?1 AND pubkey = ?2 AND identifier = ?3
                    AND (created_at < ?4 OR (created_at = ?4 AND id > ?5))",
                params![kind, &pubkey, &identifier, created_at, &id],
            )?;
        }

        let written = transaction.execute(
            "INSERT OR IGNORE INTO events (id, pubkey, created_at, kind, identifier, json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![&id, &pubkey, created_at, kind, &identifier, event.as_json()],
        )?;

        if written == 0 {
            // Either this exact event is already here, or a newer version of it
            // survived the delete and the unique index refused this one.
            let known = transaction
                .query_row("SELECT 1 FROM events WHERE id = ?1", [&id], |_| Ok(()))
                .optional()?
                .is_some();
            transaction.rollback()?;
            return Ok(if known {
                Saved::Duplicate
            } else {
                Saved::Superseded
            });
        }

        for tag in event.tags.iter() {
            let (Some(letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
                continue;
            };
            transaction.execute(
                "INSERT OR IGNORE INTO tags (event, letter, value) VALUES (?1, ?2, ?3)",
                params![&id, letter.as_str(), value],
            )?;
        }

        transaction.commit()?;
        Ok(Saved::Stored)
    }

    /// Answer one filter, newest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails, or when a stored event will not read back.
    pub fn query(&self, selector: &Selector) -> Result<Vec<Event>, Error> {
        if selector.void {
            return Ok(Vec::new());
        }
        let (sql, values) = query::build(selector, What::Events);
        self.events(&sql, values)
    }

    /// How many events one filter would have found, with no limit on it.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub fn count(&self, selector: &Selector) -> Result<usize, Error> {
        if selector.void {
            return Ok(0);
        }
        let (sql, values) = query::build(selector, What::Count);
        let connection = self.lock();
        let count: i64 =
            connection.query_row(&sql, params_from_iter(values.iter()), |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Every moderation event, oldest first: the log a group is folded from.
    ///
    /// # Errors
    ///
    /// Fails when the query fails, or when a stored event will not read back.
    pub fn moderation_log(&self) -> Result<Vec<Event>, Error> {
        let kinds: Vec<String> = [
            Kind::GroupPutUser,
            Kind::GroupRemoveUser,
            Kind::GroupEditMetadata,
            Kind::GroupCreateGroup,
            Kind::GroupDeleteGroup,
            Kind::GroupCreateInvite,
            Kind::GroupJoinRequest,
            Kind::GroupLeaveRequest,
        ]
        .iter()
        .map(|kind| kind.as_u16().to_string())
        .collect();

        let sql = format!(
            "SELECT json FROM events WHERE kind IN ({}){OLDEST_FIRST}",
            kinds.join(", ")
        );
        self.events(&sql, Vec::new())
    }

    /// The group metadata this key has signed: kinds 39000 to 39003.
    ///
    /// The relay reads its own back at start-up so it can re-sign only what has
    /// changed, rather than writing four new events for every group every time
    /// it wakes up.
    ///
    /// # Errors
    ///
    /// Fails when the query fails, or when a stored event will not read back.
    pub fn signed_by(&self, author: &PublicKey) -> Result<Vec<Event>, Error> {
        self.events(
            "SELECT json FROM events
              WHERE pubkey = ?1 AND kind >= 39000 AND kind <= 39003",
            vec![rusqlite::types::Value::Blob(author.as_bytes().to_vec())],
        )
    }

    /// Trim every group's chat down to its newest `keep` messages.
    ///
    /// Run once at start-up and never on the hot path. Without it a colony grows
    /// for ever, which is fine for a year and not for five. The tags go with the
    /// events, by the cascade.
    ///
    /// # Errors
    ///
    /// Fails when the delete fails.
    pub fn prune(&self, keep: usize) -> Result<usize, Error> {
        let connection = self.lock();
        let groups: Vec<String> = connection
            .prepare("SELECT DISTINCT value FROM tags WHERE letter = 'h'")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;

        let mut removed = 0;
        for group in groups {
            removed += connection.execute(
                &format!(
                    "DELETE FROM events WHERE id IN (
                        SELECT events.id FROM events
                          JOIN tags ON tags.event = events.id
                                   AND tags.letter = 'h' AND tags.value = ?1
                         WHERE events.kind = ?2{NEWEST_FIRST}
                         LIMIT -1 OFFSET ?3
                     )"
                ),
                params![
                    group,
                    i64::from(aphid_nostr::chat::CHAT.as_u16()),
                    clamp(keep as u64)
                ],
            )?;
        }
        Ok(removed)
    }

    /// Run a statement that selects `json` and read every row back.
    fn events(&self, sql: &str, values: Vec<rusqlite::types::Value>) -> Result<Vec<Event>, Error> {
        let connection = self.lock();
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map(params_from_iter(values.iter()), |row| {
            row.get::<_, String>(0)
        })?;

        let mut events = Vec::new();
        for json in rows {
            let json = json?;
            events.push(Event::from_json(&json).map_err(|_| Error::Malformed(json))?);
        }
        Ok(events)
    }
}

/// Whether a colony stores this kind at all.
///
/// Re-exported from the group model so the relay and the store agree.
#[must_use]
pub fn is_carried(kind: Kind) -> bool {
    group::is_carried(kind) || kind == Kind::Metadata || is_group_metadata(kind)
}

/// Whether this is one of the four events the relay signs about a group.
#[must_use]
pub fn is_group_metadata(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::GroupMetadata | Kind::GroupAdmins | Kind::GroupMembers | Kind::GroupRoles
    )
}

/// A nostr timestamp as SQLite holds it.
fn clamp(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX)
}
