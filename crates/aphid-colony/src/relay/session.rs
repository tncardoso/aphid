//! One connection, and the subscriptions on it.
//!
//! Everything a client may say arrives here as one line of JSON, and everything
//! the relay says back leaves the same way. The loop has two arms: what this
//! connection asked for, and what the colony has to say to everybody.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aphid_nostr::filter::{MAX_FILTERS, MAX_SUBSCRIPTIONS};
use aphid_nostr::nostr::event::{Event, EventId};
use aphid_nostr::nostr::filter::Filter;
use aphid_nostr::nostr::message::{ClientMessage, RelayMessage, SubscriptionId};
use aphid_nostr::nostr::types::Timestamp;
use aphid_nostr::{Reason, Selector, filter, wire};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_async};

use super::authority::{self, Ruling};
use super::{DRIFT, Fanned, MAX_EVENT, SLOW, Shared};

type Writer = SplitSink<WebSocketStream<TcpStream>, Message>;

/// The connection has gone. Every path answers with this rather than a reason,
/// because there is nowhere left to report a reason to.
struct Gone;

/// One live subscription.
struct Subscription {
    filters: Vec<Filter>,
    /// The ids already sent in the stored phase.
    ///
    /// Filled once, from the query, and emptied as its duplicates go past — so
    /// it starts at the size of one page and only ever shrinks. This is what
    /// stops an event that was committed just before the query, and broadcast
    /// just after it, from arriving twice.
    sent: HashSet<EventId>,
}

/// Serve one connection until it hangs up or falls too far behind.
pub(crate) async fn serve(stream: TcpStream, shared: Arc<Shared>) {
    let Ok(socket) = accept_async(stream).await else {
        return;
    };

    // Subscribe **before** reading a single frame. Anything published from now
    // on is held in the channel until this loop asks for it, which is what
    // closes the gap between a query and the subscription it answers.
    let mut fanout = shared.fanout.subscribe();
    let (mut writer, mut reader) = socket.split();
    let mut subscriptions: HashMap<SubscriptionId, Subscription> = HashMap::new();

    loop {
        let step = tokio::select! {
            fanned = fanout.recv() => match fanned {
                Ok(fanned) => live(&mut writer, &mut subscriptions, &fanned).await,
                Err(RecvError::Lagged(missed)) => lagged(&mut writer, &mut subscriptions, missed).await,
                Err(RecvError::Closed) => Err(Gone),
            },
            incoming = reader.next() => match incoming {
                Some(Ok(message)) => {
                    incoming_message(message, &shared, &mut subscriptions, &mut writer).await
                }
                Some(Err(_)) | None => Err(Gone),
            },
        };

        if step.is_err() {
            break;
        }
    }
}

/// Write one line, giving up on a peer that will not take it.
async fn write(writer: &mut Writer, text: String) -> Result<(), Gone> {
    match tokio::time::timeout(SLOW, writer.send(Message::text(text))).await {
        Ok(Ok(())) => Ok(()),
        // A timeout and a broken pipe are the same thing from here: there is
        // nobody to tell.
        Ok(Err(_)) | Err(_) => Err(Gone),
    }
}

/// Write one relay message.
async fn say(writer: &mut Writer, message: RelayMessage<'_>) -> Result<(), Gone> {
    let text = serde_json::to_string(&message).map_err(|_| Gone)?;
    write(writer, text).await
}

/// Write an `EVENT` without encoding the event again.
async fn forward(writer: &mut Writer, id: &SubscriptionId, json: &str) -> Result<(), Gone> {
    let sub = serde_json::to_string(id.as_str()).map_err(|_| Gone)?;
    write(writer, format!("[\"EVENT\",{sub},{json}]")).await
}

/// Something arrived on the socket.
async fn incoming_message(
    message: Message,
    shared: &Arc<Shared>,
    subscriptions: &mut HashMap<SubscriptionId, Subscription>,
    writer: &mut Writer,
) -> Result<(), Gone> {
    match message {
        Message::Text(text) => request(&text, shared, subscriptions, writer).await,
        // Tungstenite answers a ping itself when it next writes, but a colony
        // can be silent for hours and a proxy in between will not wait.
        Message::Ping(data) => {
            match tokio::time::timeout(SLOW, writer.send(Message::Pong(data))).await {
                Ok(Ok(())) => Ok(()),
                _ => Err(Gone),
            }
        }
        Message::Close(_) => Err(Gone),
        _ => Ok(()),
    }
}

/// One client frame.
async fn request(
    line: &str,
    shared: &Arc<Shared>,
    subscriptions: &mut HashMap<SubscriptionId, Subscription>,
    writer: &mut Writer,
) -> Result<(), Gone> {
    let message = match wire::parse(line) {
        Ok(message) => message,
        // There is no event id and no subscription to answer about, so NOTICE
        // is the only thing NIP-01 leaves.
        Err(why) => return say(writer, wire::notice(Reason::Invalid, &why.to_string())).await,
    };

    match message {
        ClientMessage::Event(event) => publish(event.into_owned(), shared, writer).await,
        ClientMessage::Req {
            subscription_id,
            filters,
        } => {
            let filters = filters
                .into_iter()
                .map(|filter| filter.into_owned())
                .collect();
            subscribe(
                subscription_id.into_owned(),
                filters,
                shared,
                subscriptions,
                writer,
            )
            .await
        }
        ClientMessage::Count {
            subscription_id,
            filter,
        } => count(&subscription_id, &filter, shared, writer).await,
        ClientMessage::Close(id) => {
            subscriptions.remove(&id);
            // NIP-01 wants no answer to a CLOSE.
            Ok(())
        }
        // A colony asks nobody who they are, so there is nothing to answer an
        // AUTH with. Saying so by name is kinder than ignoring it.
        ClientMessage::Auth(event) => {
            say(
                writer,
                wire::refused(
                    event.id,
                    Reason::Error,
                    "this colony does not use NIP-42; anything that reaches it may publish",
                ),
            )
            .await
        }
        ClientMessage::NegOpen {
            subscription_id, ..
        }
        | ClientMessage::NegMsg {
            subscription_id, ..
        }
        | ClientMessage::NegClose { subscription_id } => {
            say(
                writer,
                RelayMessage::NegErr {
                    subscription_id,
                    message: "error: this colony has no negentropy".into(),
                },
            )
            .await
        }
    }
}

/// Somebody published something.
async fn publish(event: Event, shared: &Arc<Shared>, writer: &mut Writer) -> Result<(), Gone> {
    let id = event.id;

    if event.as_json().len() > MAX_EVENT {
        return say(
            writer,
            wire::refused(
                id,
                Reason::Invalid,
                &format!("an event may not be longer than {MAX_EVENT} bytes"),
            ),
        )
        .await;
    }

    if !event.verify_id() {
        return say(
            writer,
            wire::refused(id, Reason::Invalid, "the id is not the hash of the event"),
        )
        .await;
    }
    if !event.verify_signature() {
        return say(
            writer,
            wire::refused(id, Reason::Invalid, "the signature does not check"),
        )
        .await;
    }

    let now = Timestamp::now();
    if event.created_at.as_secs() > now.as_secs().saturating_add(DRIFT) {
        return say(
            writer,
            wire::refused(
                id,
                Reason::Invalid,
                "created_at is in the future; check this machine's clock",
            ),
        )
        .await;
    }

    // Ask before ruling. A resent event must not be folded into a group twice,
    // and this is one indexed lookup.
    match known(shared, id).await {
        Err(why) => {
            tracing::error!(%why, "colony: could not check for a duplicate");
            return say(writer, wire::refused(id, Reason::Error, &why)).await;
        }
        Ok(true) => {
            return say(
                writer,
                wire::accepted_with(id, Reason::Duplicate, "this colony has it"),
            )
            .await;
        }
        Ok(false) => {}
    }

    // The lock is held for the ruling alone. Storing happens after it is
    // dropped, so a slow disk never blocks another connection's group rules.
    let ruling = shared.authority().judge(&event);
    if let Ruling::Refuse(Reason::Error, why) = &ruling {
        tracing::error!(%why, "colony: the authority could not rule");
    }
    let also = match &ruling {
        Ruling::Accept { also } => also.clone(),
        Ruling::Ignore(..) | Ruling::Refuse(..) => Vec::new(),
    };

    if matches!(ruling, Ruling::Accept { .. }) {
        let mut writing = vec![event];
        writing.extend(also);

        match store(shared, writing).await {
            Ok(stored) => {
                for event in stored {
                    shared.publish(event);
                }
            }
            Err(why) => {
                tracing::error!(%why, "colony: could not store an accepted event");
                return say(writer, wire::refused(id, Reason::Error, &why)).await;
            }
        }
    }

    say(writer, authority::answer(id, &ruling)).await
}

/// A new subscription, and the events that already answer it.
async fn subscribe(
    id: SubscriptionId,
    filters: Vec<Filter>,
    shared: &Arc<Shared>,
    subscriptions: &mut HashMap<SubscriptionId, Subscription>,
    writer: &mut Writer,
) -> Result<(), Gone> {
    if filters.is_empty() || filters.len() > MAX_FILTERS {
        return say(
            writer,
            wire::closed(
                &id,
                Reason::Invalid,
                &format!("a REQ carries one to {MAX_FILTERS} filters"),
            ),
        )
        .await;
    }

    // A REQ that names a live subscription replaces it, which NIP-01 requires,
    // so the count is only checked for one that does not.
    if !subscriptions.contains_key(&id) && subscriptions.len() >= MAX_SUBSCRIPTIONS {
        return say(
            writer,
            wire::closed(
                &id,
                Reason::Error,
                &format!("this connection already holds {MAX_SUBSCRIPTIONS} subscriptions"),
            ),
        )
        .await;
    }

    let mut selectors = Vec::with_capacity(filters.len());
    for filter in &filters {
        match Selector::from_filter(filter) {
            Ok(selector) => selectors.push(selector),
            Err(why) => {
                return say(writer, wire::closed(&id, Reason::Invalid, &why.to_string())).await;
            }
        }
    }

    // Registered **before** the query runs. Anything published while it is in
    // flight waits in the fan-out and is delivered when this returns.
    subscriptions.insert(
        id.clone(),
        Subscription {
            filters,
            sent: HashSet::new(),
        },
    );

    let stored = match query(shared, selectors).await {
        Ok(stored) => stored,
        Err(why) => {
            tracing::error!(%why, "colony: could not answer a subscription's stored phase");
            subscriptions.remove(&id);
            return say(writer, wire::closed(&id, Reason::Error, &why)).await;
        }
    };

    let mut sent = HashSet::with_capacity(stored.len());
    for event in &stored {
        sent.insert(event.id);
        forward(writer, &id, &event.as_json()).await?;
    }
    if let Some(subscription) = subscriptions.get_mut(&id) {
        subscription.sent = sent;
    }

    say(writer, RelayMessage::eose(id)).await
}

/// How many events one filter would find.
async fn count(
    id: &SubscriptionId,
    filter: &Filter,
    shared: &Arc<Shared>,
    writer: &mut Writer,
) -> Result<(), Gone> {
    let selector = match Selector::from_filter(filter) {
        Ok(selector) => selector,
        Err(why) => return say(writer, wire::closed(id, Reason::Invalid, &why.to_string())).await,
    };

    let shared = Arc::clone(shared);
    let counted = tokio::task::spawn_blocking(move || shared.store.count(&selector)).await;

    match counted {
        Ok(Ok(count)) => say(writer, RelayMessage::count(id.clone(), count)).await,
        Ok(Err(why)) => {
            tracing::error!(%why, "colony: could not count");
            say(writer, wire::closed(id, Reason::Error, &why.to_string())).await
        }
        Err(_) => {
            tracing::error!("colony: the store gave up counting");
            say(writer, wire::closed(id, Reason::Error, "the store gave up")).await
        }
    }
}

/// An event the colony accepted, for everybody who asked for one like it.
async fn live(
    writer: &mut Writer,
    subscriptions: &mut HashMap<SubscriptionId, Subscription>,
    fanned: &Fanned,
) -> Result<(), Gone> {
    for (id, subscription) in subscriptions.iter_mut() {
        if !filter::any_matches(&subscription.filters, &fanned.event) {
            continue;
        }
        // The stored phase already sent this one. Forget it, so the set shrinks
        // toward nothing rather than growing for the life of the subscription.
        if subscription.sent.remove(&fanned.event.id) {
            continue;
        }
        forward(writer, id, &fanned.json).await?;
    }
    Ok(())
}

/// This connection missed events it will never learn about.
///
/// The alate gateway only notices a lag, because a terminal that missed a frame
/// has lost a repaint. A chat client that missed an event has lost a message
/// nothing will tell it about again, so every subscription is closed and the
/// client is asked to start over. It is the only honest recovery.
async fn lagged(
    writer: &mut Writer,
    subscriptions: &mut HashMap<SubscriptionId, Subscription>,
    missed: u64,
) -> Result<(), Gone> {
    let why = format!("the colony fell behind by {missed}; ask again");
    for id in subscriptions.keys() {
        say(writer, wire::closed(id, Reason::Error, &why)).await?;
    }
    subscriptions.clear();
    Ok(())
}

/// Whether the colony already has this event.
async fn known(shared: &Arc<Shared>, id: EventId) -> Result<bool, String> {
    let selector = Selector {
        ids: vec![id],
        limit: 1,
        ..Selector::default()
    };
    let shared = Arc::clone(shared);
    match tokio::task::spawn_blocking(move || shared.store.query(&selector)).await {
        Ok(Ok(found)) => Ok(!found.is_empty()),
        Ok(Err(why)) => Err(why.to_string()),
        Err(_) => Err("the store gave up".to_owned()),
    }
}

/// Store these, and answer with the ones that were new.
async fn store(shared: &Arc<Shared>, events: Vec<Event>) -> Result<Vec<Event>, String> {
    let shared = Arc::clone(shared);
    let written = tokio::task::spawn_blocking(move || {
        let mut fresh = Vec::with_capacity(events.len());
        for event in events {
            match shared.store.save(&event) {
                Ok(saved) if saved.fans_out() => fresh.push(event),
                Ok(_) => {}
                Err(why) => return Err(why.to_string()),
            }
        }
        Ok(fresh)
    })
    .await;

    match written {
        Ok(result) => result,
        Err(_) => Err("the store gave up".to_owned()),
    }
}

/// Answer a `REQ`'s stored phase.
///
/// One query for each filter, each with its own limit, because NIP-01 scopes a
/// limit to the filter it is written on. The results are then one set, newest
/// first, in the order the whole crate uses.
async fn query(shared: &Arc<Shared>, selectors: Vec<Selector>) -> Result<Vec<Event>, String> {
    let shared = Arc::clone(shared);
    let found = tokio::task::spawn_blocking(move || {
        let mut seen = HashSet::new();
        let mut all = Vec::new();
        for selector in &selectors {
            for event in shared
                .store
                .query(selector)
                .map_err(|why| why.to_string())?
            {
                if seen.insert(event.id) {
                    all.push(event);
                }
            }
        }
        all.sort_by(|one, other| {
            other
                .created_at
                .cmp(&one.created_at)
                .then_with(|| one.id.cmp(&other.id))
        });
        Ok(all)
    })
    .await;

    match found {
        Ok(result) => result,
        Err(_) => Err("the store gave up".to_owned()),
    }
}
