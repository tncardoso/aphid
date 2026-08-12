//! The relay's own voice.
//!
//! NIP-29 makes the relay the authority over its groups, so this is the one
//! thing in a colony that signs events nobody asked it to sign: kinds 39000 to
//! 39003, and nothing else. It never signs a kind 9 and never signs on a
//! participant's behalf — a colony has no way to put words in anybody's mouth.
//!
//! The state is a fold over the log. Nothing about a group is stored as a
//! group; it is replayed from the moderation events at start-up, for the reason
//! [`aphid_alate::memory::store`] gives about walks and indexes. A hub's
//! moderation log is hundreds of rows. When it is not, the answer is a snapshot
//! table with a `last_applied` id, and not a second source of truth.
//!
//! Because [`group::metadata`] and its siblings are a function of the group
//! alone, re-signing is free: the relay builds all four for every group at
//! start-up and hands them to the store, and a group that has not changed
//! produces the event that is already there. So a colony restarted with nothing
//! new signs nothing new, and one restarted after a repair repairs itself.
//!
//! [`aphid_alate::memory::store`]: https://docs.rs/aphid-alate

use std::collections::HashMap;

use aphid_nostr::nostr::event::{Event, FinalizeEvent, Kind};
use aphid_nostr::nostr::key::Keys;
use aphid_nostr::nostr::types::Timestamp;
use aphid_nostr::{Action, Change, Group, GroupId, Reason, Verdict, chat, group, wire};

use crate::store::{Saved, Store};

/// What the authority decided about one event.
#[derive(Debug)]
pub enum Ruling {
    /// Store it, send it on, and send these as well — the metadata the relay
    /// re-signed because of it.
    Accept { also: Vec<Event> },
    /// Nothing to do, and that is not a failure. Answered `OK true` with the
    /// reason, which is what makes creating a group and joining one idempotent:
    /// a client may ask twice without having to ask first whether it should.
    Ignore(Reason, String),
    /// Answered `OK false`.
    Refuse(Reason, String),
}

/// Why the authority could not be built.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Store(#[from] crate::store::Error),
    #[error("the relay could not sign its own group metadata: {0}")]
    Signing(String),
}

/// Every group, and the key that says what they are.
#[derive(Debug)]
pub struct Authority {
    keys: Keys,
    groups: HashMap<GroupId, Group>,
}

impl Authority {
    /// Replay the log and take charge of what it says.
    ///
    /// # Errors
    ///
    /// Fails when the log cannot be read.
    pub fn rebuild(store: &Store, keys: Keys) -> Result<Self, Error> {
        let mut authority = Self {
            keys,
            groups: HashMap::new(),
        };

        for event in store.moderation_log()? {
            // A malformed event that was stored before a rule tightened is
            // skipped rather than fatal: refusing to start because of one bad
            // line in the log would make a colony impossible to repair.
            let Ok(Some((id, action))) = Action::read(&event) else {
                continue;
            };
            let _ = authority.fold(&id, &action, &event);
        }

        Ok(authority)
    }

    /// The relay's own key, which is who signs the group metadata.
    #[must_use]
    pub fn public_key(&self) -> aphid_nostr::nostr::key::PublicKey {
        self.keys.public_key()
    }

    #[must_use]
    pub fn group(&self, id: &GroupId) -> Option<&Group> {
        self.groups.get(id)
    }

    /// Every group the relay knows, in name order.
    #[must_use]
    pub fn groups(&self) -> Vec<&Group> {
        let mut groups: Vec<&Group> = self.groups.values().collect();
        groups.sort_by(|one, other| one.id.cmp(&other.id));
        groups
    }

    /// Make the channels named in the configuration, if they are not there.
    ///
    /// A fresh colony needs somewhere to talk before anybody can ask for one.
    /// The relay is the owner, which is what lets anybody join them.
    ///
    /// # Errors
    ///
    /// Fails when a group cannot be signed for or stored.
    pub fn ensure_channels(
        &mut self,
        store: &Store,
        names: &[String],
    ) -> Result<Vec<Event>, Error> {
        let mut made = Vec::new();
        for name in names {
            let Ok(id) = GroupId::parse(name) else {
                continue;
            };
            if self.groups.contains_key(&id) {
                continue;
            }
            let now = Timestamp::now();
            self.groups
                .insert(id.clone(), Group::create(id, self.keys.public_key(), now));
            made.push(());
        }
        if made.is_empty() {
            return Ok(Vec::new());
        }
        self.publish_metadata(store)
    }

    /// Sign what every group is, and store what is not already stored.
    ///
    /// Answers with the events that were new, which is what has to go out to
    /// anybody listening. A group that has not changed signs to the same id and
    /// the store answers [`Saved::Duplicate`], so nothing is returned for it.
    ///
    /// # Errors
    ///
    /// Fails when a group cannot be signed for or stored.
    pub fn publish_metadata(&self, store: &Store) -> Result<Vec<Event>, Error> {
        let mut fresh = Vec::new();
        for group in self.groups.values() {
            for event in self.sign(group)? {
                if store.save(&event)? == Saved::Stored {
                    fresh.push(event);
                }
            }
        }
        Ok(fresh)
    }

    /// Rule on one event.
    ///
    /// The event must already have had its id and its signature checked; what
    /// is decided here is only what the groups decide.
    pub fn judge(&mut self, event: &Event) -> Ruling {
        // A kind 0 is how a participant says what it is called, and it belongs
        // to no group. It is the one thing a colony carries without an `h`.
        if event.kind == Kind::Metadata {
            return Ruling::Accept { also: Vec::new() };
        }

        if !group::is_carried(event.kind) {
            return Ruling::Refuse(
                Reason::Invalid,
                format!(
                    "a colony carries chat and moderation; it has nothing to do with a kind {}",
                    event.kind.as_u16()
                ),
            );
        }

        let Some(named) = chat::group_of(event) else {
            return Ruling::Refuse(
                Reason::Invalid,
                "a colony event needs an h tag naming its group".to_owned(),
            );
        };
        let id = match GroupId::parse(named) {
            Ok(id) => id,
            Err(why) => return Ruling::Refuse(Reason::Invalid, why.to_string()),
        };

        match Action::read(event) {
            Err(why) => Ruling::Refuse(Reason::Invalid, why.to_string()),
            Ok(Some((_, action))) => self.moderate(&id, &action, event),
            Ok(None) => self.say(&id, event),
        }
    }

    /// Something said in a group.
    fn say(&self, id: &GroupId, event: &Event) -> Ruling {
        let Some(group) = self.groups.get(id) else {
            return Ruling::Refuse(Reason::Invalid, format!("there is no group {id}"));
        };
        match group.may_publish(&event.pubkey, event.kind) {
            Verdict::Allow => Ruling::Accept { also: Vec::new() },
            Verdict::Refuse(reason, why) => Ruling::Refuse(reason, why),
        }
    }

    /// A change to a group.
    fn moderate(&mut self, id: &GroupId, action: &Action, event: &Event) -> Ruling {
        let change = match self.fold(id, action, event) {
            Ok(change) => change,
            Err(ruling) => return ruling,
        };

        if !change.any() {
            // The action was good and had already been taken. The event is
            // still stored — it is the log — and nothing needs re-signing.
            return Ruling::Accept { also: Vec::new() };
        }

        let Some(group) = self.groups.get(id) else {
            return Ruling::Accept { also: Vec::new() };
        };
        match self.sign_change(group, change) {
            Ok(also) => Ruling::Accept { also },
            Err(why) => Ruling::Refuse(Reason::Error, why.to_string()),
        }
    }

    /// Apply one action to the state, making and unmaking groups.
    ///
    /// This is the whole of the fold, and it is what `rebuild` replays, so a
    /// rule added here is a rule that applies to the past as well as the
    /// future.
    fn fold(&mut self, id: &GroupId, action: &Action, event: &Event) -> Result<Change, Ruling> {
        let now = event.created_at;

        match action {
            Action::CreateGroup => {
                if let Some(existing) = self.groups.get(id) {
                    // Idempotent for anybody already inside, which is what lets
                    // a client open a direct message without first asking
                    // whether it is open.
                    return if existing.is_member(&event.pubkey) {
                        Err(Ruling::Ignore(
                            Reason::Duplicate,
                            format!("{id} is already here"),
                        ))
                    } else {
                        Err(Ruling::Refuse(
                            Reason::Invalid,
                            format!("{id} already exists"),
                        ))
                    };
                }

                let group = if id.is_direct() {
                    // The id names both members in full, so the relay checks it
                    // against the author rather than believing a tag. This is
                    // the whole payoff for an id that carries its keys.
                    let names_the_author = id
                        .direct_members()
                        .is_some_and(|(one, other)| one == event.pubkey || other == event.pubkey);
                    if !names_the_author {
                        return Err(Ruling::Refuse(
                            Reason::Invalid,
                            "a direct group's id must name you as one of its two".to_owned(),
                        ));
                    }
                    match Group::direct(id, now) {
                        Ok(group) => group,
                        Err(why) => return Err(Ruling::Refuse(Reason::Invalid, why.to_string())),
                    }
                } else {
                    Group::create(id.clone(), event.pubkey, now)
                };

                self.groups.insert(id.clone(), group);
                Ok(Change {
                    metadata: true,
                    admins: true,
                    members: true,
                })
            }

            Action::DeleteGroup => {
                let Some(group) = self.groups.get(id) else {
                    return Err(Ruling::Refuse(
                        Reason::Invalid,
                        format!("there is no group {id}"),
                    ));
                };
                if !group
                    .role_of(&event.pubkey)
                    .is_some_and(|role| role.moderates())
                {
                    return Err(Ruling::Refuse(
                        Reason::Restricted,
                        format!("only an admin unmakes {id}"),
                    ));
                }
                self.groups.remove(id);
                // Nothing to re-sign: the group is gone, and the metadata that
                // described it stays in the log as the record that it was here.
                Ok(Change::none())
            }

            _ => {
                let Some(group) = self.groups.get_mut(id) else {
                    return Err(Ruling::Refuse(
                        Reason::Invalid,
                        format!("there is no group {id}"),
                    ));
                };
                group
                    .apply(action, &event.pubkey, now)
                    .map_err(|why| Ruling::Refuse(why.reason(), why.to_string()))
            }
        }
    }

    /// Sign all four metadata events for a group.
    fn sign(&self, group: &Group) -> Result<Vec<Event>, Error> {
        [
            group::metadata(group),
            group::admins(group),
            group::members(group),
            group::roles(group),
        ]
        .into_iter()
        .map(|builder| {
            builder
                .finalize(&self.keys)
                .map_err(|why| Error::Signing(why.to_string()))
        })
        .collect()
    }

    /// Sign only what moved.
    fn sign_change(&self, group: &Group, change: Change) -> Result<Vec<Event>, Error> {
        let mut events = Vec::new();
        let mut sign = |builder: aphid_nostr::nostr::event::EventBuilder| -> Result<(), Error> {
            events.push(
                builder
                    .finalize(&self.keys)
                    .map_err(|why| Error::Signing(why.to_string()))?,
            );
            Ok(())
        };

        if change.metadata {
            sign(group::metadata(group))?;
            // The roles never change, but a group that has just been made has
            // not published them yet, and a duplicate costs nothing.
            sign(group::roles(group))?;
        }
        if change.admins {
            sign(group::admins(group))?;
        }
        if change.members {
            sign(group::members(group))?;
        }
        Ok(events)
    }
}

/// The `OK` a ruling deserves, given the id it is about.
#[must_use]
pub fn answer(
    id: aphid_nostr::nostr::event::EventId,
    ruling: &Ruling,
) -> aphid_nostr::nostr::message::RelayMessage<'static> {
    match ruling {
        Ruling::Accept { .. } => wire::accepted(id),
        Ruling::Ignore(reason, why) => wire::accepted_with(id, *reason, why),
        Ruling::Refuse(reason, why) => wire::refused(id, *reason, why),
    }
}
