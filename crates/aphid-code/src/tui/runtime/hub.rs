//! Where every message comes in.

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// The one way into the loop.
///
/// Held by everything that has news: the key thread, the agent's hooks, a
/// plugin, a finished command. Sending never blocks and never fails in a way a
/// caller must handle — a closed hub means the session is over, which is an
/// answer and not an error.
pub struct Hub<M>(UnboundedSender<M>);

/// A hub and the end the runtime drains.
#[must_use]
pub fn channel<M>() -> (Hub<M>, UnboundedReceiver<M>) {
    let (sender, receiver) = unbounded_channel();
    (Hub(sender), receiver)
}

impl<M> Hub<M> {
    /// Deliver a message. False once the loop has gone.
    pub fn send(&self, msg: M) -> bool {
        self.0.send(msg).is_ok()
    }
}

// Derived `Clone` would ask `M: Clone`, which the message type has no reason
// to be just because its sender is copied around.
impl<M> Clone for Hub<M> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<M> std::fmt::Debug for Hub<M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Hub")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_closed_loop_says_so_rather_than_failing() {
        let (hub, receiver) = super::channel::<u8>();
        assert!(hub.send(1));

        drop(receiver);
        assert!(!hub.send(2), "the session is over, which is an answer");
    }

    #[test]
    fn a_hub_is_cloneable_whatever_it_carries() {
        // Deliberately not `Clone`: a message may carry a `Box<dyn …>` or an
        // outcome nobody wants copied, and that must not stop the senders from
        // being handed out.
        struct NotClone(u8);

        let (hub, mut receiver) = super::channel::<NotClone>();
        let second = hub.clone();
        assert!(second.send(NotClone(7)));
        assert_eq!(receiver.try_recv().map(|m| m.0), Ok(7));
    }
}
