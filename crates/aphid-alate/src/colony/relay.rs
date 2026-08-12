//! The colony, and the seam a test puts something else in.
//!
//! Telegram's [`Api`] is one method because the Bot API is one method: a
//! request, an answer. A relay is not that shape. A client publishes when it
//! has something to say and hears from the relay whenever the relay has
//! something to send, and the two are not paired — so this is four methods, and
//! a test drives both directions of them with no socket anywhere.
//!
//! The websocket itself lives in [`aphid_colony::client`], so nothing in this
//! crate imports tungstenite.
//!
//! [`Api`]: crate::telegram::Api

use std::pin::Pin;
use std::sync::Arc;

use aphid_colony::client::Client;
use aphid_nostr::nostr::event::Event;
use aphid_nostr::nostr::filter::Filter;
use aphid_nostr::nostr::key::Keys;
use aphid_nostr::nostr::message::RelayMessage;

/// One call in flight.
pub type Ask<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// The next thing the colony said, or `None` when it hung up.
pub type Next<'a> = Pin<Box<dyn Future<Output = Option<RelayMessage<'static>>> + Send + 'a>>;

/// What the bridge needs of a colony.
pub trait Relay: Send + Sync {
    fn publish<'a>(&'a self, event: Event) -> Ask<'a>;
    fn subscribe<'a>(&'a self, id: &'a str, filters: Vec<Filter>) -> Ask<'a>;
    fn unsubscribe<'a>(&'a self, id: &'a str) -> Ask<'a>;
    fn recv(&self) -> Next<'_>;
}

/// A colony several tasks share.
pub type RelayFn = Arc<dyn Relay>;

/// The real one: a websocket to a colony.
pub struct Live {
    client: Client,
}

impl Live {
    /// Open a connection.
    ///
    /// # Errors
    ///
    /// Fails when the colony cannot be reached.
    pub async fn connect(url: &str) -> Result<Self, String> {
        let client = Client::connect(url).await.map_err(|why| why.to_string())?;
        Ok(Self { client })
    }
}

impl Relay for Live {
    fn publish<'a>(&'a self, event: Event) -> Ask<'a> {
        Box::pin(async move {
            self.client
                .publish(event)
                .await
                .map_err(|why| why.to_string())
        })
    }

    fn subscribe<'a>(&'a self, id: &'a str, filters: Vec<Filter>) -> Ask<'a> {
        Box::pin(async move {
            self.client
                .subscribe(id, filters)
                .await
                .map_err(|why| why.to_string())
        })
    }

    fn unsubscribe<'a>(&'a self, id: &'a str) -> Ask<'a> {
        Box::pin(async move {
            self.client
                .unsubscribe(id)
                .await
                .map_err(|why| why.to_string())
        })
    }

    fn recv(&self) -> Next<'_> {
        Box::pin(self.client.recv())
    }
}

/// Read this agent's key.
///
/// From the environment and never from the configuration file, for the reason
/// the bot token gives: a configuration file is copied and shared, and a key in
/// it goes with it.
///
/// # Errors
///
/// Fails when the variable is unset or empty, or holds something that is not a
/// secret key.
pub fn keys(variable: &str) -> Result<Keys, String> {
    let text = std::env::var(variable)
        .ok()
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "{variable} is not set; put this agent's colony key there, or change \
                 gateway.colony.key_env"
            )
        })?;

    Keys::parse(text.trim()).map_err(|_| format!("{variable} does not hold a secret key"))
}
