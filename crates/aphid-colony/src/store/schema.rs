//! The tables, and the version they are.
//!
//! One file, opened by one process. The pragmas say so: `WAL` keeps a reader
//! from waiting on the writer, and `NORMAL` is the right amount of caution for a
//! chat hub, which is not a ledger and can afford to lose the last few seconds
//! if the machine loses power.

/// The schema this build writes.
///
/// It lives in `meta` under `schema`, with the same three-part policy the
/// configuration uses: absent is a fresh file to make, equal is a file to open,
/// and greater is refused by name.
pub const SCHEMA: u32 = 1;

/// The key `SCHEMA` is stored under.
pub const KEY: &str = "schema";

/// What to run when the file is opened.
pub const PRAGMAS: &str = "\
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
";

/// What to run when the file is new.
///
/// The primary key on `tags` is `(letter, value, event)` and not
/// `(event, letter, value)` on purpose: every tag query a colony asks is "the
/// events with `#h` equal to this", and that order answers it by scanning the
/// index straight into a set of ids.
///
/// The two partial unique indexes are not decoration. They are how a newer
/// version of a replaceable event refuses an older one: the `DELETE` in `save`
/// removes what is genuinely older, and anything that survives makes the
/// following `INSERT OR IGNORE` do nothing, which is what the caller reads as
/// [`Saved::Superseded`].
///
/// [`Saved::Superseded`]: super::Saved
pub const CREATE: &str = "\
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS events (
    id         BLOB PRIMARY KEY NOT NULL,
    pubkey     BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    kind       INTEGER NOT NULL,
    identifier TEXT NOT NULL DEFAULT '',
    json       TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS tags (
    event  BLOB NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    letter TEXT NOT NULL,
    value  TEXT NOT NULL,
    PRIMARY KEY (letter, value, event)
) WITHOUT ROWID, STRICT;

CREATE INDEX IF NOT EXISTS events_created          ON events(created_at DESC);
CREATE INDEX IF NOT EXISTS events_kind_time        ON events(kind, created_at DESC);
CREATE INDEX IF NOT EXISTS events_author_time      ON events(pubkey, created_at DESC);
CREATE INDEX IF NOT EXISTS events_kind_author_time ON events(kind, pubkey, created_at DESC);
CREATE INDEX IF NOT EXISTS tags_event             ON tags(event);

CREATE UNIQUE INDEX IF NOT EXISTS events_addressable
    ON events(kind, pubkey, identifier)
    WHERE kind >= 30000 AND kind < 40000;

CREATE UNIQUE INDEX IF NOT EXISTS events_replaceable
    ON events(kind, pubkey)
    WHERE kind = 0 OR kind = 3 OR (kind >= 10000 AND kind < 20000);
";
