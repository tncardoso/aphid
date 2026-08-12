//! What this crate refuses, in the words it refuses with.
//!
//! One enum for the whole crate. Every message is a full sentence, because most
//! of them end up in an `OK` or a `CLOSED` and are read by a person looking at
//! a log, not only by the client that asked.

use crate::wire::Reason;

/// Something a colony would not take.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A line that is not one JSON array a client may send.
    #[error("a client message is one JSON array for each line: {source}")]
    Malformed {
        #[source]
        source: serde_json::Error,
    },

    /// A filter that asked for a full-text search.
    ///
    /// Refused rather than ignored: a client that asked for a search and got an
    /// unfiltered firehose is worse off than one that got a `CLOSED`.
    #[error("this relay does not search; ask with ids, authors, kinds or tags")]
    Search,

    /// A group id that is not one.
    #[error("`{id}` is not a group id: {why}")]
    Id { id: String, why: &'static str },

    /// An event whose kind promises a tag that is not on it.
    #[error("a {kind} needs {want}")]
    Missing {
        kind: &'static str,
        want: &'static str,
    },

    /// Something the author may not do, or a state that will not take the
    /// change. The sentence is the one that goes into the `OK`.
    #[error("{why}")]
    Refused { reason: Reason, why: String },
}

impl Error {
    /// The word NIP-01 reserves for this refusal.
    ///
    /// Everything that is not a considered refusal is `invalid`, which is what
    /// NIP-01 asks a relay to say about a message it could not make sense of.
    #[must_use]
    pub fn reason(&self) -> Reason {
        match self {
            Self::Refused { reason, .. } => *reason,
            _ => Reason::Invalid,
        }
    }

    /// Build a refusal.
    #[must_use]
    pub fn refused(reason: Reason, why: impl Into<String>) -> Self {
        Self::Refused {
            reason,
            why: why.into(),
        }
    }
}
