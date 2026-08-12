//! The left-hand nav: what there is to talk in, and where there is something
//! new.
//!
//! Channels above, direct messages below, and each half newest-spoken-in first.
//! The order moves as people talk, which is what makes a busy hub readable and
//! is also why the selection is followed by id and not by row.

use aphid_nostr::GroupId;
use aphid_nostr::nostr::key::PublicKey;
use aphid_nostr::nostr::types::Timestamp;

use super::log::{Names, name_of};

/// Which half of the nav a chat belongs in.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Channel,
    Direct,
}

/// One row.
#[derive(Clone, Debug)]
pub struct Chat {
    pub id: GroupId,
    pub kind: Kind,
    /// The other member of a direct group, so the label can follow their name
    /// when their kind 0 arrives.
    pub other: Option<PublicKey>,
    /// Messages since this chat was last looked at.
    pub unread: usize,
    /// When something was last said in it.
    pub last: Timestamp,
    /// Whether this terminal is a member. A chat it can read but not write in
    /// is still worth showing: that is how you find one to join.
    pub joined: bool,
}

impl Chat {
    /// `#general`, or `@thiago`.
    #[must_use]
    pub fn label(&self, names: &Names) -> String {
        match (self.kind, self.other) {
            (Kind::Direct, Some(other)) => format!("@{}", name_of(other, names)),
            (Kind::Direct, None) => "@?".to_owned(),
            (Kind::Channel, _) => format!("#{}", self.id),
        }
    }
}

/// Every chat, in the order they are drawn.
#[derive(Debug, Default)]
pub struct Chats {
    rows: Vec<Chat>,
    /// Followed by id, because the rows move under it.
    selected: Option<GroupId>,
}

impl Chats {
    /// Note that a group exists, without disturbing what is selected.
    pub fn know(&mut self, id: &GroupId, me: &PublicKey) {
        if self.find(id).is_some() {
            return;
        }
        let (kind, other) = match id.direct_members() {
            Some((one, two)) => {
                // In a conversation with yourself both halves are you, and
                // `@me` is the honest label.
                let other = if one == *me { two } else { one };
                (Kind::Direct, Some(other))
            }
            None => (Kind::Channel, None),
        };

        self.rows.push(Chat {
            id: id.clone(),
            kind,
            other,
            unread: 0,
            last: Timestamp::zero(),
            joined: false,
        });
        self.order();
        if self.selected.is_none() {
            self.selected = Some(id.clone());
        }
    }

    /// Say whether this terminal is in the group.
    pub fn membership(&mut self, id: &GroupId, joined: bool) {
        if let Some(chat) = self.rows.iter_mut().find(|chat| chat.id == *id) {
            chat.joined = joined;
        }
    }

    /// Something was said. `unread` is false for the chat on screen.
    pub fn said(&mut self, id: &GroupId, at: Timestamp, unread: bool) {
        if let Some(chat) = self.rows.iter_mut().find(|chat| chat.id == *id) {
            chat.last = chat.last.max(at);
            if unread {
                chat.unread += 1;
            }
        }
        self.order();
    }

    /// This chat has been looked at.
    pub fn read(&mut self, id: &GroupId) {
        if let Some(chat) = self.rows.iter_mut().find(|chat| chat.id == *id) {
            chat.unread = 0;
        }
    }

    #[must_use]
    pub fn rows(&self) -> &[Chat] {
        &self.rows
    }

    #[must_use]
    pub fn current(&self) -> Option<&Chat> {
        self.selected.as_ref().and_then(|id| self.find(id))
    }

    #[must_use]
    pub fn selected(&self) -> Option<&GroupId> {
        self.selected.as_ref()
    }

    /// Which row is drawn as chosen.
    #[must_use]
    pub fn at(&self) -> usize {
        self.selected
            .as_ref()
            .and_then(|id| self.rows.iter().position(|chat| chat.id == *id))
            .unwrap_or(0)
    }

    pub fn select(&mut self, id: &GroupId) {
        if self.find(id).is_some() {
            self.selected = Some(id.clone());
            self.read(id);
        }
    }

    /// Move by one row, stopping at each end rather than wrapping: a list that
    /// wraps makes it impossible to tell the ends apart without reading.
    pub fn step(&mut self, by: isize) {
        if self.rows.is_empty() {
            return;
        }
        let at = self.at() as isize;
        let next = (at + by).clamp(0, self.rows.len() as isize - 1) as usize;
        let id = self.rows[next].id.clone();
        self.select(&id);
    }

    fn find(&self, id: &GroupId) -> Option<&Chat> {
        self.rows.iter().find(|chat| chat.id == *id)
    }

    fn order(&mut self) {
        self.rows.sort_by(|one, other| {
            one.kind
                .cmp(&other.kind)
                .then_with(|| other.last.cmp(&one.last))
                .then_with(|| one.id.cmp(&other.id))
        });
    }
}

#[cfg(test)]
mod tests {
    use aphid_nostr::direct_id;
    use aphid_nostr::nostr::key::Keys;

    use super::*;

    fn id(name: &str) -> GroupId {
        GroupId::parse(name).expect("a group id")
    }

    #[test]
    fn channels_come_before_direct_messages() {
        let me = Keys::generate().public_key();
        let other = Keys::generate().public_key();
        let mut chats = Chats::default();

        chats.know(&direct_id(&me, &other), &me);
        chats.know(&id("general"), &me);

        let kinds: Vec<Kind> = chats.rows().iter().map(|chat| chat.kind).collect();
        assert_eq!(kinds, vec![Kind::Channel, Kind::Direct]);
    }

    #[test]
    fn the_newest_spoken_in_rises() {
        let me = Keys::generate().public_key();
        let mut chats = Chats::default();
        chats.know(&id("general"), &me);
        chats.know(&id("build"), &me);

        chats.said(&id("general"), Timestamp::from_secs(100), true);
        assert_eq!(chats.rows()[0].id, id("general"));

        chats.said(&id("build"), Timestamp::from_secs(200), true);
        assert_eq!(chats.rows()[0].id, id("build"));
    }

    #[test]
    fn the_selection_follows_the_chat_and_not_the_row() {
        let me = Keys::generate().public_key();
        let mut chats = Chats::default();
        chats.know(&id("general"), &me);
        chats.know(&id("build"), &me);
        chats.select(&id("general"));

        // Something in the other chat moves it to the top; the selection stays
        // where the person put it.
        chats.said(&id("build"), Timestamp::from_secs(200), true);
        assert_eq!(chats.current().expect("one is chosen").id, id("general"));
    }

    #[test]
    fn looking_at_a_chat_clears_its_count() {
        let me = Keys::generate().public_key();
        let mut chats = Chats::default();
        chats.know(&id("general"), &me);
        chats.said(&id("general"), Timestamp::from_secs(100), true);
        assert_eq!(chats.rows()[0].unread, 1);

        chats.select(&id("general"));
        assert_eq!(chats.rows()[0].unread, 0);
    }

    #[test]
    fn a_direct_chat_is_labelled_by_the_other_one() {
        let me = Keys::generate().public_key();
        let other = Keys::generate();
        let mut names = Names::new();
        names.insert(other.public_key(), "scout".to_owned());

        let mut chats = Chats::default();
        chats.know(&direct_id(&me, &other.public_key()), &me);

        assert_eq!(chats.rows()[0].label(&names), "@scout");
    }

    #[test]
    fn stepping_stops_at_the_ends() {
        let me = Keys::generate().public_key();
        let mut chats = Chats::default();
        chats.know(&id("build"), &me);
        chats.know(&id("general"), &me);

        chats.step(-5);
        assert_eq!(chats.at(), 0);
        chats.step(5);
        assert_eq!(chats.at(), 1);
    }
}
