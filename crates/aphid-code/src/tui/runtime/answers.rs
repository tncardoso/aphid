//! Questions the model asked, and where their answers go back to.
//!
//! Something outside the loop — a gated tool call, on the agent's own task —
//! needs an answer from whoever is at the keyboard. It cannot wait on the
//! model, because the model is not a thing that waits; it puts the question on
//! the hub and blocks on a channel of its own.
//!
//! That channel must not travel with the question. A model holding one end of
//! a live channel is a model that cannot be cloned, compared, logged or
//! replayed, and the message carrying it cannot either. So the runtime keeps
//! the channels and the message carries a [`RequestId`]: plain data all the
//! way to the screen and back.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a question waits before it gives up.
///
/// A person who walks away from a prompt must not leave a run wedged for the
/// rest of the day. Long enough to make a cup of tea and come back to it.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

/// Names one open question.
pub type RequestId = u64;

/// The reply channels for the questions the model is showing.
///
/// Cloneable and shared: whoever asks holds one, and whoever answers holds
/// another.
pub struct Answers<A>(Arc<Inner<A>>);

struct Inner<A> {
    waiting: Mutex<HashMap<RequestId, Sender<A>>>,
    next: AtomicU64,
}

impl<A> Answers<A> {
    /// Open a question. The caller blocks on the receiver; the id is what goes
    /// on the hub.
    pub fn open(&self) -> (RequestId, Receiver<A>) {
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = channel();
        if let Ok(mut waiting) = self.0.waiting.lock() {
            waiting.insert(id, sender);
        }
        (id, receiver)
    }

    /// Answer a question and forget it. A second answer for one id finds
    /// nothing, which is what makes a double keypress harmless.
    pub fn answer(&self, id: RequestId, answer: A) {
        let Ok(mut waiting) = self.0.waiting.lock() else {
            return;
        };
        if let Some(reply) = waiting.remove(&id) {
            // A closed receiver means the asker gave up first.
            let _ = reply.send(answer);
        }
    }

    /// Drop a question's sender, so whoever is blocked on it stops at once
    /// with nothing rather than waiting out the timeout.
    pub fn abandon(&self, id: RequestId) {
        if let Ok(mut waiting) = self.0.waiting.lock() {
            waiting.remove(&id);
        }
    }

    /// Abandon every open question. Called on the way out, so a session that
    /// quits with a prompt on screen releases the task blocked behind it
    /// instead of leaving it to time out.
    pub fn abandon_all(&self) {
        if let Ok(mut waiting) = self.0.waiting.lock() {
            waiting.clear();
        }
    }

    /// How many questions are open. For tests and for a status line.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.0.waiting.lock().map(|w| w.len()).unwrap_or_default()
    }
}

impl<A> Default for Answers<A> {
    fn default() -> Self {
        Self(Arc::new(Inner {
            waiting: Mutex::new(HashMap::new()),
            // Ids start at one so a zero can never be mistaken for a real
            // question on a wire that defaults its fields.
            next: AtomicU64::new(1),
        }))
    }
}

// Derived `Clone` would ask `A: Clone` for no reason: only the handle is
// copied, never an answer.
impl<A> Clone for Answers<A> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<A> std::fmt::Debug for Answers<A> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Answers")
            .field("open", &self.open_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Answers;

    #[test]
    fn an_answer_reaches_the_question_that_is_waiting() {
        let answers = Answers::<&str>::default();
        let (id, waiting) = answers.open();

        answers.answer(id, "yes");

        assert_eq!(waiting.try_recv(), Ok("yes"));
        assert_eq!(answers.open_count(), 0, "an answered question is closed");
    }

    #[test]
    fn two_questions_do_not_cross() {
        let answers = Answers::<&str>::default();
        let (first, one) = answers.open();
        let (second, two) = answers.open();
        assert_ne!(first, second);

        answers.answer(second, "for the second");

        assert_eq!(two.try_recv(), Ok("for the second"));
        assert!(one.try_recv().is_err(), "the first is still waiting");
    }

    #[test]
    fn answering_twice_is_harmless() {
        let answers = Answers::<&str>::default();
        let (id, waiting) = answers.open();

        answers.answer(id, "yes");
        answers.answer(id, "yes again");

        assert_eq!(waiting.try_recv(), Ok("yes"));
        assert!(waiting.try_recv().is_err(), "only the first got through");
    }

    #[test]
    fn abandoning_releases_whoever_is_blocked() {
        let answers = Answers::<&str>::default();
        let (_, waiting) = answers.open();

        answers.abandon_all();

        // Not a timeout: the sender is gone, so the wait ends at once.
        assert!(
            waiting.recv_timeout(Duration::from_millis(50)).is_err(),
            "a question nobody will answer must not hold its asker"
        );
    }
}
