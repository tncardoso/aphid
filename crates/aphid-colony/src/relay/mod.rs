//! The relay: one websocket, NIP-01 on it, NIP-29 groups behind it.
//!
//! `tokio-tungstenite` and not `axum`, because this server has one route and no
//! extractors, and because tungstenite's default features are `connect` and
//! `handshake` and **no TLS at all**. A loopback `ws://` needs none, and
//! refusing to pull one is the same instinct that made the alate gateway a Unix
//! socket. The one thing axum would have bought cheaply is NIP-11 — the
//! document a stock nostr client reads over plain HTTP to learn what a relay
//! supports — and a colony does not serve it. Its clients are its own terminal
//! and aphid agents, both of which already know. If that changes, the honest
//! answer is axum and not two hundred lines of hand-rolled HTTP.
//!
//! The shape is the alate gateway's: a [`broadcast`] channel for what everybody
//! may want, one task for each connection, subscribe before announcing, and a
//! lagging receiver reported rather than ignored. What differs is the filtering
//! — a relay filters by the connection's own subscriptions rather than by a
//! session id — and the fan-out, which carries the JSON already made so that
//! twenty connections do not encode one event twenty times.
//!
//! # The ordering hazard
//!
//! A `REQ` is answered in two phases: what is stored, then what arrives after.
//! An event accepted **between** the two is lost for ever, and this is the bug
//! every home-made relay ships. It presents as "messages sometimes just do not
//! arrive", months later.
//!
//! A colony closes the gap by never having one. The connection subscribes to
//! the fan-out when it opens, before it has read a single frame, and the
//! subscription is registered before its query runs — so anything published
//! while the query is in flight is already waiting in the channel when the loop
//! comes back to it. The ids sent in the stored phase are remembered, and a
//! live event that is one of them is dropped instead of sent twice. That set
//! only ever shrinks: it is filled once from the query and emptied as the
//! duplicates go past.

mod authority;
mod session;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aphid_nostr::nostr::event::Event;
use aphid_nostr::nostr::key::Keys;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

pub use authority::{Authority, Ruling};

use crate::store::Store;

/// How many events the fan-out holds for a connection that is behind.
const BROADCAST: usize = 1_024;

/// The longest one write may take before the connection is given up on.
///
/// A peer that is dead but has not said so must not hold a slot for ever.
const SLOW: Duration = Duration::from_secs(30);

/// The largest event this relay takes.
const MAX_EVENT: usize = 128 * 1024;

/// How far ahead of this relay's clock an event may claim to be.
///
/// NIP-01 leaves this to the relay. Without it, one client with a wrong clock
/// pins itself to the top of every log for ever. A `created_at` in the **past**
/// is allowed without limit: a bridge that was offline for a day is an ordinary
/// thing, and its messages belong where they happened.
const DRIFT: u64 = 900;

/// Why a relay would not start.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{address}: {source}")]
    Listen {
        address: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Store(#[from] crate::store::Error),
    #[error("{0}")]
    Authority(#[from] authority::Error),
}

/// What to start a relay with.
pub struct Options {
    /// Where to listen. Everything that reaches this address may publish and
    /// read, so it decides who is in the colony.
    pub address: SocketAddr,
    /// Every event the colony has been told.
    pub store: Store,
    /// The key the relay signs group metadata with. It is the group authority.
    pub keys: Keys,
    /// Channels to make at start-up if they are not there.
    pub channels: Vec<String>,
    /// The most messages to keep for one group. Trimmed once, here, and never
    /// on the hot path.
    pub history: usize,
}

/// One accepted event, ready to go out.
///
/// The JSON is made once, when the event is accepted, and shared from there. A
/// relay with twenty connections would otherwise encode the same event twenty
/// times to send the same bytes twenty times.
#[derive(Clone, Debug)]
pub(crate) struct Fanned {
    pub event: Arc<Event>,
    pub json: Arc<str>,
}

impl Fanned {
    fn new(event: Event) -> Self {
        let json: Arc<str> = Arc::from(event.as_json());
        Self {
            event: Arc::new(event),
            json,
        }
    }
}

/// What every connection shares.
pub(crate) struct Shared {
    pub store: Store,
    pub authority: Mutex<Authority>,
    pub fanout: broadcast::Sender<Fanned>,
    pub connections: AtomicUsize,
}

impl Shared {
    /// Take the authority, and take it even when another thread panicked
    /// holding it. The groups are a fold over a log that is still on disk, so
    /// the worst case is one wrong answer and not a colony that never carries
    /// anything again.
    fn authority(&self) -> std::sync::MutexGuard<'_, Authority> {
        match self.authority.lock() {
            Ok(authority) => authority,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Send an event to every connection that asked for one like it.
    fn publish(&self, event: Event) {
        // An error here is nobody listening, which is the ordinary state of a
        // colony at four in the morning.
        let _ = self.fanout.send(Fanned::new(event));
    }
}

/// A running relay.
pub struct Relay {
    address: SocketAddr,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
}

impl Relay {
    /// Bind, replay the log, make the channels that are missing, and start
    /// accepting.
    ///
    /// # Errors
    ///
    /// Fails when the address cannot be bound, when the log cannot be read, or
    /// when the relay cannot sign for its own groups.
    pub async fn bind(options: Options) -> Result<Self, Error> {
        // Ignored: a test that binds more than one relay in the same process
        // calls this more than once, and a second `set_global_default` must not
        // panic.
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .try_init();

        let Options {
            address,
            store,
            keys,
            channels,
            history,
        } = options;

        // Trimming before anything is served, so a colony that has been running
        // for a year does not spend its first query reading five years of chat.
        store.prune(history)?;

        let mut authority = Authority::rebuild(&store, keys, &channels)?;
        authority.ensure_channels(&store, &channels)?;
        // Sign what every group is. A group that has not changed signs to the
        // event that is already stored, so a quiet restart writes nothing.
        authority.publish_metadata(&store)?;

        let listener = TcpListener::bind(address)
            .await
            .map_err(|source| Error::Listen { address, source })?;
        let address = listener.local_addr().unwrap_or(address);
        tracing::info!(%address, "colony listening");

        let (fanout, _) = broadcast::channel(BROADCAST);
        let shared = Arc::new(Shared {
            store,
            authority: Mutex::new(authority),
            fanout,
            connections: AtomicUsize::new(0),
        });

        let accept = tokio::spawn(accept(listener, Arc::clone(&shared)));
        Ok(Self {
            address,
            shared,
            accept,
        })
    }

    /// Where it is listening. Not always what was asked for: a colony bound to
    /// port zero is how a test gets one nothing else is using.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// How many clients are attached.
    #[must_use]
    pub fn connections(&self) -> usize {
        self.shared.connections.load(Ordering::Relaxed)
    }

    /// Every group the relay knows, by id, in order.
    #[must_use]
    pub fn groups(&self) -> Vec<String> {
        self.shared
            .authority()
            .groups()
            .iter()
            .map(|group| group.id.to_string())
            .collect()
    }

    /// Wait for the accept loop to end, which it does when it is aborted.
    ///
    /// By reference and not by value: dropping a relay is what stops it, so a
    /// method that consumed one would stop the thing it was waiting for.
    pub async fn joined(&mut self) {
        let _ = (&mut self.accept).await;
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        // The connections are children of the accept loop's tasks and go with
        // it. A colony that is dropped is a colony that is over.
        self.accept.abort();
    }
}

/// Take connections until the task is aborted.
async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // One refused connection is not a reason to stop taking them.
            continue;
        };
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let connections = shared.connections.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(connections, "client connected");
            session::serve(stream, Arc::clone(&shared)).await;
            let connections = shared.connections.fetch_sub(1, Ordering::Relaxed) - 1;
            tracing::info!(connections, "client disconnected");
        });
    }
}
