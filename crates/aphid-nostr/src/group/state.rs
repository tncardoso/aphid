//! What a group is, and what may be done to it.
//!
//! One fold over the moderation log gives one group, which is why every rule in
//! here is a pure function of the group and the action: the relay rebuilds its
//! whole state at start-up by replaying the events it stored, and a rule that
//! consulted a clock or a socket would give a different answer on the second
//! pass.

use std::collections::{BTreeMap, BTreeSet};

use nostr::event::{Kind, Tag};
use nostr::key::PublicKey;
use nostr::types::Timestamp;
use serde::{Deserialize, Serialize};

use super::Action;
use super::id::GroupId;
use crate::wire::Reason;
use crate::{Error, chat};

/// The shortest and the longest an invite code may be.
const CODE: std::ops::RangeInclusive<usize> = 8..=64;

/// What a member may do.
///
/// Two roles, because a hub where every participant is either the person who
/// runs it or an agent that talks in it gets nothing from a ladder. Kind 39003
/// declares exactly these two, so a client that reads the roles finds what it
/// will meet.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Member,
    Admin,
}

impl Role {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Admin => "admin",
        }
    }

    /// What kind 39003 says this role is for.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Member => "Reads and writes.",
            Self::Admin => "Reads, writes, and changes who is here.",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "member" => Some(Self::Member),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// Whether this role may add, remove, invite and edit.
    #[must_use]
    pub const fn moderates(self) -> bool {
        matches!(self, Self::Admin)
    }
}

/// One participant, and what it may do.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub pubkey: PublicKey,
    pub role: Role,
}

/// Who may read, and who may join.
///
/// `public` is always true in a colony, and cannot honestly be anything else:
/// with no NIP-42 handshake the relay does not know who is asking on a `REQ`.
/// See the module documentation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Access {
    pub public: bool,
    pub closed: bool,
}

impl Access {
    /// A channel: anybody reads, anybody joins.
    #[must_use]
    pub const fn channel() -> Self {
        Self {
            public: true,
            closed: false,
        }
    }

    /// A direct group: anybody reads, nobody joins.
    #[must_use]
    pub const fn direct() -> Self {
        Self {
            public: true,
            closed: true,
        }
    }

    /// The words that go in a 39000.
    ///
    /// Both halves are written, although NIP-29 only requires the restrictive
    /// one. A reader that looks for `public` finds it, and a reader that looks
    /// for the absence of `private` finds that too.
    #[must_use]
    pub fn tags(self) -> [Tag; 2] {
        let word = |word: &str| Tag::custom(word, Vec::<String>::new());
        [
            word(if self.public { "public" } else { "private" }),
            word(if self.closed { "closed" } else { "open" }),
        ]
    }

    /// Read the words back.
    ///
    /// By NIP-29's rule, which is that the restrictive word is the one that has
    /// to be there: no `private` means public, and no `closed` means open.
    #[must_use]
    pub fn read(tags: &[Tag]) -> Self {
        let has = |word: &str| tags.iter().any(|tag| tag.kind() == word);
        Self {
            public: !has("private"),
            closed: has("closed"),
        }
    }
}

/// Whether a publish is allowed, and the refusal if it is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Refuse(Reason, String),
}

/// What an applied action moved, so the relay re-signs only what changed.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub metadata: bool,
    pub admins: bool,
    pub members: bool,
}

impl Change {
    /// Nothing moved. The action was valid and had already been taken.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            metadata: false,
            admins: false,
            members: false,
        }
    }

    /// Whether anything at all moved.
    #[must_use]
    pub const fn any(self) -> bool {
        self.metadata || self.admins || self.members
    }
}

/// One group, as the relay holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: GroupId,
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub access: Access,
    /// Every member, admins included.
    pub members: BTreeMap<PublicKey, Role>,
    /// Invite codes that have not been spent.
    pub invites: BTreeSet<String>,
    pub created_at: Timestamp,
    /// When the group last changed.
    ///
    /// The metadata events carry this as their `created_at`, which makes them
    /// a function of the group alone: a relay that restarts and rebuilds signs
    /// byte-for-byte what is already stored, and so signs nothing.
    pub changed_at: Timestamp,
}

impl Group {
    /// A new channel with `owner` as its only admin.
    #[must_use]
    pub fn create(id: GroupId, owner: PublicKey, at: Timestamp) -> Self {
        Self {
            id,
            name: None,
            about: None,
            picture: None,
            access: Access::channel(),
            members: BTreeMap::from([(owner, Role::Admin)]),
            invites: BTreeSet::new(),
            created_at: at,
            changed_at: at,
        }
    }

    /// The group a pair talks in: both members, both admins, closed to anybody
    /// else for ever.
    ///
    /// # Errors
    ///
    /// Fails when the id is not a direct one, or when its halves are not keys.
    pub fn direct(id: &GroupId, at: Timestamp) -> Result<Self, Error> {
        let (first, second) = id.direct_members().ok_or_else(|| Error::Id {
            id: id.to_string(),
            why: "a direct group's id names its two members in hex",
        })?;

        Ok(Self {
            id: id.clone(),
            name: None,
            about: None,
            picture: None,
            access: Access::direct(),
            members: BTreeMap::from([(first, Role::Admin), (second, Role::Admin)]),
            invites: BTreeSet::new(),
            created_at: at,
            changed_at: at,
        })
    }

    #[must_use]
    pub fn role_of(&self, who: &PublicKey) -> Option<Role> {
        self.members.get(who).copied()
    }

    #[must_use]
    pub fn is_member(&self, who: &PublicKey) -> bool {
        self.members.contains_key(who)
    }

    #[must_use]
    pub fn is_direct(&self) -> bool {
        self.id.is_direct()
    }

    /// Whether `who` may publish `kind` into this group.
    ///
    /// A join request is the one thing a stranger may say to a group. Everything
    /// else needs membership, **in an open group as much as a closed one**:
    /// NIP-29's `open` governs joining, not posting. Joining on the first post
    /// was considered and refused, because an agent that mistypes a group name
    /// would otherwise quietly acquire membership of a group it should not be
    /// in, and publishing a 9021 first is one line in a bridge.
    #[must_use]
    pub fn may_publish(&self, who: &PublicKey, kind: Kind) -> Verdict {
        if kind == Kind::GroupJoinRequest || self.is_member(who) {
            return Verdict::Allow;
        }
        Verdict::Refuse(
            Reason::Restricted,
            format!("join {} before you talk in it", self.id),
        )
    }

    /// Whether `who` may read it.
    ///
    /// Always true while every group is public. It takes an [`Option`] that it
    /// never reads because the relay does not know who is asking, and it exists
    /// so that a private group, if one is ever wanted, has exactly one place to
    /// be refused in.
    #[must_use]
    pub fn may_read(&self, who: Option<&PublicKey>) -> bool {
        let _ = who;
        self.access.public
    }

    /// How many admins are left.
    fn admins(&self) -> usize {
        self.members
            .values()
            .filter(|role| role.moderates())
            .count()
    }

    /// Apply one moderation action.
    ///
    /// The event it came from must already have had its id and its signature
    /// checked. Creating and deleting a group are not here: they make and unmake
    /// the thing this takes by reference, so the relay handles them.
    ///
    /// # Errors
    ///
    /// Fails when the author may not do it, or the action does not carry what it
    /// needs. The message is the sentence that goes into the `OK`.
    pub fn apply(
        &mut self,
        action: &Action,
        by: &PublicKey,
        at: Timestamp,
    ) -> Result<Change, Error> {
        let change = match action {
            Action::PutUser { users } => {
                self.moderated_by(by)?;
                self.not_direct()?;
                let mut change = Change::none();
                for member in users {
                    let was = self.members.insert(member.pubkey, member.role);
                    if was != Some(member.role) {
                        change.members = true;
                        change.admins |=
                            member.role.moderates() || was.is_some_and(Role::moderates);
                    }
                }
                change
            }

            Action::RemoveUser { users } => {
                self.moderated_by(by)?;
                self.not_direct()?;
                let mut change = Change::none();
                for pubkey in users {
                    let Some(role) = self.members.get(pubkey).copied() else {
                        continue;
                    };
                    if role.moderates() && self.admins() == 1 {
                        return Err(Error::refused(
                            Reason::Invalid,
                            format!("{} needs one admin", self.id),
                        ));
                    }
                    self.members.remove(pubkey);
                    change.members = true;
                    change.admins |= role.moderates();
                }
                change
            }

            Action::EditMetadata {
                name,
                about,
                picture,
                access,
            } => {
                self.moderated_by(by)?;
                let mut change = Change::none();
                for (field, value) in [
                    (&mut self.name, name),
                    (&mut self.about, about),
                    (&mut self.picture, picture),
                ] {
                    if let Some(value) = value
                        && field.as_ref() != Some(value)
                    {
                        *field = Some(value.clone());
                        change.metadata = true;
                    }
                }
                // The words a colony writes are the only words it honours: a
                // group cannot be made private while nothing knows who is
                // asking, so only `closed` is taken from an edit.
                if let Some(access) = access
                    && access.closed != self.access.closed
                {
                    self.access.closed = access.closed;
                    change.metadata = true;
                }
                change
            }

            Action::CreateInvite { code } => {
                self.moderated_by(by)?;
                if !CODE.contains(&code.chars().count()) {
                    return Err(Error::refused(
                        Reason::Invalid,
                        format!(
                            "an invite code is {} to {} characters",
                            CODE.start(),
                            CODE.end()
                        ),
                    ));
                }
                self.invites.insert(code.clone());
                // An invite is not in any of the three metadata events, so
                // nothing needs re-signing for it.
                Change::none()
            }

            Action::JoinRequest { code } => {
                if self.is_member(by) {
                    return Ok(Change::none());
                }
                if self.is_direct() {
                    return Err(self.two_for_ever());
                }
                if self.access.closed {
                    let spent = code.as_ref().is_some_and(|code| self.invites.remove(code));
                    if !spent {
                        return Err(Error::refused(
                            Reason::Restricted,
                            format!("{} is closed; ask an admin to add you", self.id),
                        ));
                    }
                }
                self.members.insert(*by, Role::Member);
                Change {
                    members: true,
                    ..Change::none()
                }
            }

            Action::LeaveRequest => {
                let Some(role) = self.members.get(by).copied() else {
                    return Ok(Change::none());
                };
                if self.is_direct() {
                    return Err(self.two_for_ever());
                }
                if role.moderates() && self.admins() == 1 {
                    return Err(Error::refused(
                        Reason::Invalid,
                        format!("{} needs one admin", self.id),
                    ));
                }
                self.members.remove(by);
                Change {
                    members: true,
                    admins: role.moderates(),
                    ..Change::none()
                }
            }

            Action::CreateGroup | Action::DeleteGroup => {
                return Err(Error::refused(
                    Reason::Invalid,
                    "a group is made and unmade by the relay, not changed into being".to_owned(),
                ));
            }
        };

        if change.any() {
            self.changed_at = next(self.changed_at, at);
        }
        Ok(change)
    }

    /// # Errors
    ///
    /// Fails when the author is not an admin.
    fn moderated_by(&self, who: &PublicKey) -> Result<(), Error> {
        if self.role_of(who).is_some_and(Role::moderates) {
            return Ok(());
        }
        Err(Error::refused(
            Reason::Restricted,
            format!("only an admin changes {}", self.id),
        ))
    }

    /// # Errors
    ///
    /// Fails when this is the group a pair talks in.
    fn not_direct(&self) -> Result<(), Error> {
        if self.is_direct() {
            return Err(self.two_for_ever());
        }
        Ok(())
    }

    fn two_for_ever(&self) -> Error {
        Error::refused(
            Reason::Invalid,
            "a direct message has two members and always will".to_owned(),
        )
    }
}

/// When a group that changed at `last` and has just changed again says it did.
///
/// Strictly later than `last`, always, and that is not fussiness. The metadata
/// events carry `changed_at` as their `created_at`, and NIP-01 breaks a tie
/// between two versions of one addressable event on the **lower id** — so two
/// changes in the same second would leave the relay re-signing a new membership
/// list that the store then refuses in favour of the old one, roughly half the
/// time. A group made and joined within a second is not a rare case; it is what
/// a fresh colony does.
///
/// The clock is followed whenever it has moved. When it has not, the group
/// counts. That keeps the metadata a function of the group alone, which is what
/// lets a restart re-sign it and store nothing.
fn next(last: Timestamp, at: Timestamp) -> Timestamp {
    Timestamp::from_secs(at.as_secs().max(last.as_secs().saturating_add(1)))
}

/// Whether this kind is one the relay reads as a change to a group rather than
/// as something said in one.
#[must_use]
pub fn is_moderation(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::GroupPutUser
            | Kind::GroupRemoveUser
            | Kind::GroupEditMetadata
            | Kind::GroupCreateGroup
            | Kind::GroupDeleteGroup
            | Kind::GroupCreateInvite
            | Kind::GroupJoinRequest
            | Kind::GroupLeaveRequest
    )
}

/// Whether this kind is one a participant may put into a group at all.
///
/// A colony carries chat and moderation and nothing else. A relay that took
/// every kind would grow a public timeline nobody reads.
#[must_use]
pub fn is_carried(kind: Kind) -> bool {
    kind == chat::CHAT || is_moderation(kind)
}
