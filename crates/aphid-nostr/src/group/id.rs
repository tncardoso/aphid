//! What names a group.
//!
//! A group id is the `d` of its metadata and the `h` of every message in it, so
//! it travels on every line and has to be cheap to compare and safe to put in a
//! file name.

use std::fmt;

use nostr::key::PublicKey;
use serde::{Deserialize, Serialize};

use crate::Error;

/// What a direct group's id begins with.
const DIRECT: &str = "dm-";

/// How long each half of a direct group's id is: a public key in hex.
const KEY: usize = 64;

/// A NIP-29 group id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GroupId(String);

impl GroupId {
    /// Long enough for a direct group's id, which names both keys in full.
    pub const MAX: usize = 160;

    /// Read one.
    ///
    /// # Errors
    ///
    /// Fails when the id is empty, longer than [`GroupId::MAX`], starts with a
    /// dot, or holds anything but letters, digits, dash, dot and underscore.
    /// The rule is the one [`aphid_alate::home`] uses for an alate's name, and
    /// for the same reason: an id becomes a label, a path and a tag value, and
    /// each of those has an opinion about what a name may be.
    ///
    /// [`aphid_alate::home`]: https://docs.rs/aphid-alate
    pub fn parse(id: &str) -> Result<Self, Error> {
        let why = if id.is_empty() {
            Some("it is empty")
        } else if id.len() > Self::MAX {
            Some("it is too long")
        } else if id.starts_with('.') {
            Some("it starts with a dot")
        } else if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        {
            Some("it may hold only letters, digits, dash, dot and underscore")
        } else {
            None
        };

        match why {
            Some(why) => Err(Error::Id {
                id: id.to_owned(),
                why,
            }),
            None => Ok(Self(id.to_owned())),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is shaped like the group a pair talks in.
    ///
    /// The shape only. [`GroupId::direct_members`] does the reading, because
    /// two 32-byte keys have to be points on the curve and that is not a thing
    /// to check on every line that goes past.
    #[must_use]
    pub fn is_direct(&self) -> bool {
        let Some(rest) = self.0.strip_prefix(DIRECT) else {
            return false;
        };
        rest.len() == KEY * 2 + 1
            && rest.as_bytes()[KEY] == b'-'
            && rest
                .bytes()
                .enumerate()
                .all(|(at, byte)| at == KEY || byte.is_ascii_hexdigit())
    }

    /// The two members a direct group's id names, in the order it names them.
    ///
    /// The id carries both keys in full, so this is recovery and not a guess.
    /// The relay uses it to check a create against its author without trusting
    /// a tag, and the terminal uses it to label a chat `@thiago` before the
    /// membership list has arrived.
    #[must_use]
    pub fn direct_members(&self) -> Option<(PublicKey, PublicKey)> {
        if !self.is_direct() {
            return None;
        }
        let rest = self.0.strip_prefix(DIRECT)?;
        let first = PublicKey::from_hex(&rest[..KEY]).ok()?;
        let second = PublicKey::from_hex(&rest[KEY + 1..]).ok()?;
        Some((first, second))
    }
}

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for GroupId {
    type Error = Error;

    fn try_from(id: String) -> Result<Self, Error> {
        Self::parse(&id)
    }
}

impl From<GroupId> for String {
    fn from(id: GroupId) -> Self {
        id.0
    }
}

/// The group two participants talk in.
///
/// `dm-<lower hex>-<higher hex>`: the two keys sorted and joined. Deterministic
/// and symmetric, so both sides work it out without asking the relay and a
/// direct message needs no registry to open.
///
/// Both keys in full, and not a hash of them, for two reasons. The `nostr`
/// crate re-exports no general SHA-256, so a hash would mean a dependency for
/// one string; and an id that names its members is one the relay can check and
/// the terminal can label from, which a hash is not. It is long, and nobody
/// types it: the terminal shows `@thiago`.
///
/// It is a **grouping and not a secret**. The id names both parties in clear,
/// and a colony's groups are all world-readable. Nothing here is private.
#[must_use]
pub fn direct_id(a: &PublicKey, b: &PublicKey) -> GroupId {
    let (a, b) = (a.to_hex(), b.to_hex());
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    GroupId(format!("{DIRECT}{first}-{second}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_could_escape_a_directory_is_refused() {
        for bad in ["", ".", ".hidden", "one/two", "one two", "..", "a\0b"] {
            assert!(GroupId::parse(bad).is_err(), "{bad:?} is not a group id");
        }
        for good in ["general", "the-build", "a.b_c", "9"] {
            assert!(GroupId::parse(good).is_ok(), "{good:?} is a group id");
        }
    }

    #[test]
    fn a_channel_is_not_direct() {
        let id = GroupId::parse("general").expect("a group id");
        assert!(!id.is_direct());
        assert!(id.direct_members().is_none());
    }

    #[test]
    fn the_shape_is_the_whole_of_the_checking() {
        // A `PublicKey` is thirty-two bytes and is not checked against the
        // curve until something signs with it, so any id of the right shape
        // names two of them. That is enough for what the relay does with it:
        // it compares them against the author of the create, and an author
        // that is not on the curve has no signature to arrive with.
        let id =
            GroupId::parse(&format!("dm-{}-{}", "0".repeat(64), "1".repeat(64))).expect("an id");
        assert!(id.is_direct());
        assert!(id.direct_members().is_some());

        // A half of the wrong length is not a direct id at all.
        let short = GroupId::parse("dm-abcd-ef01").expect("an id");
        assert!(!short.is_direct());
        assert!(short.direct_members().is_none());
    }
}
