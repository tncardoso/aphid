//! Talking to a colony.
//!
//! The terminal uses this, and so does the alate bridge, which is why it is in
//! this crate and not in either of them: an agent and a person are the same
//! kind of participant and there is no reason for two clients.
//!
//! It is deliberately thin. There is no reconnect loop and no subscription
//! bookkeeping here, because the two callers want different ones — a terminal
//! redraws what it has and an alate re-announces itself into every channel it
//! was configured with — and a client that guessed would be wrong for both.
//! What it does is turn a socket into "publish this" and "here is the next
//! thing the relay said".

use std::sync::Arc;

use aphid_nostr::nostr::event::Event;
use aphid_nostr::nostr::filter::Filter;
use aphid_nostr::nostr::message::{ClientMessage, RelayMessage, SubscriptionId};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Why a colony would not answer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{url}: {source}")]
    Connect {
        url: String,
        #[source]
        source: tokio_tungstenite::tungstenite::Error,
    },
    #[error("the colony hung up: {0}")]
    Gone(String),
    #[error("{0}")]
    Encoding(#[from] serde_json::Error),
}

/// A connection to one colony.
///
/// The two halves are locked apart, so a task that is reading does not stop a
/// tool call from publishing. There is one of each, and neither waits on the
/// other.
pub struct Client {
    writer: Mutex<SplitSink<Socket, Message>>,
    reader: Mutex<SplitStream<Socket>>,
    url: String,
}

impl Client {
    /// Open a connection.
    ///
    /// # Errors
    ///
    /// Fails when the colony cannot be reached.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let (socket, _) = connect_async(url).await.map_err(|source| Error::Connect {
            url: url.to_owned(),
            source,
        })?;
        let (writer, reader) = socket.split();
        Ok(Self {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
            url: url.to_owned(),
        })
    }

    /// Which colony this is, for a message that has to name it.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Say something. The relay answers with an `OK`, which arrives through
    /// [`Client::recv`] like everything else.
    ///
    /// # Errors
    ///
    /// Fails when the connection has gone.
    pub async fn publish(&self, event: Event) -> Result<(), Error> {
        self.send(&ClientMessage::event(event)).await
    }

    /// Ask for events, stored and then live.
    ///
    /// # Errors
    ///
    /// Fails when the connection has gone.
    pub async fn subscribe(&self, id: &str, filters: Vec<Filter>) -> Result<(), Error> {
        self.send(&ClientMessage::req(SubscriptionId::new(id), filters))
            .await
    }

    /// Stop a subscription. The relay answers nothing, which NIP-01 requires.
    ///
    /// # Errors
    ///
    /// Fails when the connection has gone.
    pub async fn unsubscribe(&self, id: &str) -> Result<(), Error> {
        self.send(&ClientMessage::close(SubscriptionId::new(id)))
            .await
    }

    /// The next thing the relay said, or `None` when it hung up.
    ///
    /// Anything that is not a relay message — a ping, a stray binary frame — is
    /// dealt with here rather than handed up, so a caller's loop is only ever
    /// about the protocol.
    pub async fn recv(&self) -> Option<RelayMessage<'static>> {
        loop {
            let incoming = {
                let mut reader = self.reader.lock().await;
                reader.next().await
            };

            match incoming? {
                Ok(Message::Text(text)) => {
                    if let Ok(message) = serde_json::from_str(&text) {
                        return Some(message);
                    }
                    // A relay that says something this client cannot read is
                    // not a reason to hang up; the next line may be fine.
                }
                Ok(Message::Ping(data)) => {
                    let mut writer = self.writer.lock().await;
                    writer.send(Message::Pong(data)).await.ok()?;
                }
                Ok(Message::Close(_)) => return None,
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    async fn send(&self, message: &ClientMessage<'_>) -> Result<(), Error> {
        let text = serde_json::to_string(message)?;
        let mut writer = self.writer.lock().await;
        writer
            .send(Message::text(text))
            .await
            .map_err(|why| Error::Gone(why.to_string()))
    }
}

/// A client several tasks share.
pub type Shared = Arc<Client>;
