//! What the creature is saying, over its head.
//!
//! The alate speaks twice: into the log, where everything is kept, and in a
//! balloon over the familiar, where the last thing it said stays until it says
//! another. The second is the one you read while you are doing something else,
//! which is the whole reason the window is on the desktop rather than in a
//! terminal.
//!
//! It follows the same frames the log does — a turn begins, text arrives, the
//! run ends — so nothing here asks anything of the daemon.
//!
//! In the desktop companion this borrows from, the balloon was laid out with
//! Pango, rasterized with cairo and drawn as a textured quad inside the GL
//! pass. Here it is an element of the window laid over the creature: real text,
//! in the same palette as everything else, that does not have to be redrawn
//! into a texture to change.

/// The last thing the alate said, and whether it is still saying it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Balloon {
    text: String,
    visible: bool,
    streaming: bool,
}

impl Balloon {
    /// A turn has started: clear what was said and wait for the new reply.
    ///
    /// The balloon is emptied and stays out of the way until the first text
    /// arrives, so a run that only calls tools does not leave the last answer
    /// hanging over a creature that has moved on.
    pub fn begin(&mut self) {
        self.text.clear();
        self.visible = false;
        self.streaming = true;
    }

    /// Another piece of the reply.
    pub fn append(&mut self, delta: &str) {
        self.text.push_str(delta);
        if !self.text.trim().is_empty() {
            self.visible = true;
        }
    }

    /// The run ended. What was said stays up.
    pub fn finish(&mut self) {
        self.streaming = false;
    }

    /// Say one thing, from the window rather than from the agent.
    pub fn show(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.visible = !self.text.trim().is_empty();
        self.streaming = false;
    }

    /// Put it away. `Escape` does this, as it did in the companion this follows.
    pub fn dismiss(&mut self) {
        self.visible = false;
        self.streaming = false;
    }

    /// Everything is thrown away: another alate, or another session.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// What to draw, when there is anything.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        if self.visible && !self.text.trim().is_empty() {
            Some(self.text.trim())
        } else {
            None
        }
    }

    /// Whether the reply is still arriving, which the window shows with a
    /// caret at the end.
    #[must_use]
    pub fn streaming(&self) -> bool {
        self.streaming
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_said_before_anything_is_said() {
        assert_eq!(Balloon::default().text(), None);
    }

    #[test]
    fn a_reply_arrives_in_pieces_and_reads_as_one() {
        let mut balloon = Balloon::default();
        balloon.begin();
        for piece in ["A ", "sentence ", "in pieces."] {
            balloon.append(piece);
        }
        assert_eq!(balloon.text(), Some("A sentence in pieces."));
        assert!(balloon.streaming());
        balloon.finish();
        assert!(!balloon.streaming());
        // And it stays up afterwards, which is the point of it.
        assert_eq!(balloon.text(), Some("A sentence in pieces."));
    }

    #[test]
    fn a_new_turn_takes_the_last_answer_down() {
        let mut balloon = Balloon::default();
        balloon.show("the last answer");
        balloon.begin();
        assert_eq!(balloon.text(), None, "the old answer is not left hanging");
    }

    #[test]
    fn a_turn_that_only_calls_tools_says_nothing() {
        let mut balloon = Balloon::default();
        balloon.begin();
        balloon.finish();
        assert_eq!(balloon.text(), None);
    }

    #[test]
    fn whitespace_alone_is_not_something_to_say() {
        let mut balloon = Balloon::default();
        balloon.begin();
        balloon.append("\n  \n");
        assert_eq!(balloon.text(), None);
        balloon.append("here it is");
        assert_eq!(balloon.text(), Some("here it is"));
    }

    #[test]
    fn dismissing_it_keeps_it_down_until_the_next_reply() {
        let mut balloon = Balloon::default();
        balloon.show("something");
        balloon.dismiss();
        assert_eq!(balloon.text(), None);
        balloon.begin();
        balloon.append("the next one");
        assert_eq!(balloon.text(), Some("the next one"));
    }
}
