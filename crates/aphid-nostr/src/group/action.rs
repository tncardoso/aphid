//! Reading a moderation event.
//!
//! Every one of these carries an `h` naming the group it is about, and most
//! carry one more tag without which they mean nothing — a 9000 with no `p` asks
//! to add nobody. Reading is separated from applying so that the relay can
//! answer "this event is malformed" differently from "you may not do that": the
//! first is the client's bug and the second is the group's rule.

use nostr::event::{Event, Kind};
use nostr::key::PublicKey;

use super::id::GroupId;
use super::state::{Access, Member, Role};
use crate::{Error, chat};

/// One NIP-29 moderation event, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// 9000 — add users with the roles their `p` tags name.
    PutUser { users: Vec<Member> },
    /// 9001 — remove users.
    RemoveUser { users: Vec<PublicKey> },
    /// 9002 — set the name, the description, the picture, the access words.
    EditMetadata {
        name: Option<String>,
        about: Option<String>,
        picture: Option<String>,
        access: Option<Access>,
    },
    /// 9007 — make this group.
    CreateGroup,
    /// 9008 — unmake it.
    DeleteGroup,
    /// 9009 — mint an invite code.
    CreateInvite { code: String },
    /// 9021 — ask to join, with a code when the group is closed.
    JoinRequest { code: Option<String> },
    /// 9022 — leave.
    LeaveRequest,
}

impl Action {
    /// Read one, with the group it names.
    ///
    /// `None` when the kind is not a moderation kind, which is how the relay
    /// tells a moderation event from something said in a group.
    ///
    /// # Errors
    ///
    /// Fails when the kind is a moderation kind but the tags do not carry what
    /// it needs: any of them with no `h`, a 9000 or 9001 with no `p`, a 9009
    /// with no code.
    pub fn read(event: &Event) -> Result<Option<(GroupId, Self)>, Error> {
        let kind = event.kind;
        if !super::is_moderation(kind) {
            return Ok(None);
        }

        let group = chat::group_of(event).ok_or(Error::Missing {
            kind: "group event",
            want: "an h tag naming its group",
        })?;
        let group = GroupId::parse(group)?;

        let action = match kind {
            Kind::GroupPutUser => Self::PutUser {
                users: members(event, "put-user")?,
            },
            Kind::GroupRemoveUser => Self::RemoveUser {
                users: members(event, "remove-user")?
                    .into_iter()
                    .map(|member| member.pubkey)
                    .collect(),
            },
            Kind::GroupEditMetadata => Self::EditMetadata {
                name: word(event, "name").map(str::to_owned),
                about: word(event, "about").map(str::to_owned),
                picture: word(event, "picture").map(str::to_owned),
                access: read_access(event),
            },
            Kind::GroupCreateGroup => Self::CreateGroup,
            Kind::GroupDeleteGroup => Self::DeleteGroup,
            Kind::GroupCreateInvite => Self::CreateInvite {
                code: word(event, "code")
                    .ok_or(Error::Missing {
                        kind: "create-invite",
                        want: "a code tag",
                    })?
                    .to_owned(),
            },
            Kind::GroupJoinRequest => Self::JoinRequest {
                code: word(event, "code").map(str::to_owned),
            },
            Kind::GroupLeaveRequest => Self::LeaveRequest,
            // `is_moderation` and this match list the same kinds. The compiler
            // cannot see that, so the arm exists and is unreachable.
            _ => return Ok(None),
        };

        Ok(Some((group, action)))
    }

    /// The kind an action is carried by.
    #[must_use]
    pub const fn kind(&self) -> Kind {
        match self {
            Self::PutUser { .. } => Kind::GroupPutUser,
            Self::RemoveUser { .. } => Kind::GroupRemoveUser,
            Self::EditMetadata { .. } => Kind::GroupEditMetadata,
            Self::CreateGroup => Kind::GroupCreateGroup,
            Self::DeleteGroup => Kind::GroupDeleteGroup,
            Self::CreateInvite { .. } => Kind::GroupCreateInvite,
            Self::JoinRequest { .. } => Kind::GroupJoinRequest,
            Self::LeaveRequest => Kind::GroupLeaveRequest,
        }
    }
}

/// The first value of the first tag with this name.
fn word<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event
        .tags
        .iter()
        .find(|tag| tag.kind() == name)
        .and_then(|tag| tag.content())
}

/// Every `p` tag, with the role it names.
///
/// A role the relay does not know is read as [`Role::Member`] rather than
/// refused: NIP-29 lets a group declare its own roles, and a colony that met an
/// unknown one should let the person in, not lock them out.
fn members(event: &Event, kind: &'static str) -> Result<Vec<Member>, Error> {
    let users: Vec<Member> = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == "p")
        .filter_map(|tag| {
            let mut values = tag.as_slice().iter().skip(1);
            let pubkey = PublicKey::from_hex(values.next()?).ok()?;
            let role = values
                .filter_map(|name| Role::parse(name))
                .max()
                .unwrap_or_default();
            Some(Member { pubkey, role })
        })
        .collect();

    if users.is_empty() {
        return Err(Error::Missing {
            kind,
            want: "a p tag naming somebody",
        });
    }
    Ok(users)
}

/// The access words on an edit, when it carries any.
///
/// `None` leaves the group as it was, which is what an edit that only renames
/// the group means.
fn read_access(event: &Event) -> Option<Access> {
    let has = |word: &str| event.tags.iter().any(|tag| tag.kind() == word);
    let (public, private) = (has("public"), has("private"));
    let (open, closed) = (has("open"), has("closed"));

    if !(public || private || open || closed) {
        return None;
    }
    Some(Access::read(&event.tags))
}
