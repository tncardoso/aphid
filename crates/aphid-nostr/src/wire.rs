//! What crosses the socket, and the words a refusal is made of.
//!
//! NIP-01 is one JSON array for each line, in both directions, which is the
//! same shape the alate gateway uses and readable with the same tools. The
//! encoding itself is [`nostr`]'s; what is here is the half a relay writes and
//! a client reads only to report — the machine-readable prefix on an `OK`, a
//! `CLOSED` and a `NOTICE`.
//!
//! These builders live in a crate with no socket in it so that a test which
//! asserts on a refusal does not have to start one.

use nostr::event::EventId;
use nostr::message::{ClientMessage, RelayMessage, SubscriptionId};

use crate::Error;

/// Why a relay said no, in the words NIP-01 reserves.
///
/// The prefix is the machine-readable part: a client is meant to branch on it
/// and show the rest. Anything a relay cannot classify is [`Reason::Error`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// The relay already has this event. Answered `OK true`, not `OK false` —
    /// the client's event did arrive, and it should not send it again.
    Duplicate,
    /// The event is malformed: a bad id, a bad signature, a missing tag.
    Invalid,
    /// The author is not welcome here.
    Blocked,
    /// The author is welcome, but not to do this.
    Restricted,
    /// Slow down.
    RateLimited,
    /// The relay wants a NIP-42 handshake first. A colony never says this, and
    /// the word exists so that a client which handles it is not surprised.
    AuthRequired,
    /// Something went wrong that is the relay's fault, not the client's.
    Error,
}

impl Reason {
    /// The machine-readable word, without its colon.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Invalid => "invalid",
            Self::Blocked => "blocked",
            Self::Restricted => "restricted",
            Self::RateLimited => "rate-limited",
            Self::AuthRequired => "auth-required",
            Self::Error => "error",
        }
    }
}

/// One refusal, as NIP-01 wants it written: `restricted: join #general first`.
#[must_use]
pub fn say(reason: Reason, detail: &str) -> String {
    format!("{}: {detail}", reason.prefix())
}

/// Read one client frame.
///
/// # Errors
///
/// Fails when the line is not JSON, or is JSON that is not a client message.
pub fn parse(line: &str) -> Result<ClientMessage<'static>, Error> {
    serde_json::from_str(line).map_err(|source| Error::Malformed { source })
}

/// `["OK", <id>, true, ""]` — taken, with nothing to say about it.
#[must_use]
pub fn accepted(id: EventId) -> RelayMessage<'static> {
    RelayMessage::ok(id, true, "")
}

/// `["OK", <id>, true, "<reason>: <detail>"]` — taken, and explained.
///
/// This exists for the duplicate, which NIP-01 wants answered as a **success**
/// that says why nothing was written. A client that reads only the boolean is
/// right to carry on, and one that reads the sentence learns not to resend.
#[must_use]
pub fn accepted_with(id: EventId, reason: Reason, detail: &str) -> RelayMessage<'static> {
    RelayMessage::ok(id, true, say(reason, detail))
}

/// `["OK", <id>, false, "<reason>: <detail>"]`.
#[must_use]
pub fn refused(id: EventId, reason: Reason, detail: &str) -> RelayMessage<'static> {
    RelayMessage::ok(id, false, say(reason, detail))
}

/// `["CLOSED", <sub>, "<reason>: <detail>"]` — this subscription is over.
#[must_use]
pub fn closed(sub: &SubscriptionId, reason: Reason, detail: &str) -> RelayMessage<'static> {
    RelayMessage::closed(sub.clone(), say(reason, detail))
}

/// `["NOTICE", "<reason>: <detail>"]` — about the connection, not about any one
/// event, because there was no event id to answer.
#[must_use]
pub fn notice(reason: Reason, detail: &str) -> RelayMessage<'static> {
    RelayMessage::notice(say(reason, detail))
}

/// The `OK` an [`Error`] deserves.
#[must_use]
pub fn refusal(id: EventId, error: &Error) -> RelayMessage<'static> {
    refused(id, error.reason(), &error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_has_a_word() {
        for reason in [
            Reason::Duplicate,
            Reason::Invalid,
            Reason::Blocked,
            Reason::Restricted,
            Reason::RateLimited,
            Reason::AuthRequired,
            Reason::Error,
        ] {
            assert!(!reason.prefix().is_empty());
            assert!(!reason.prefix().contains(':'));
        }
    }

    #[test]
    fn a_refusal_reads_as_one_sentence() {
        assert_eq!(
            say(Reason::Restricted, "join #general before you talk in it"),
            "restricted: join #general before you talk in it"
        );
    }
}
