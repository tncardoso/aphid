//! One group, and the gateway connection behind it.
//!
//! A group is a client, in the plain sense of [`crate::gateway::client`]: it
//! attaches, the daemon opens a session for it, and it sees that session's
//! frames and nothing else. So two channels are two conversations without the
//! bridge keeping a map of who is talking about what — the gateway already
//! does that, and it is the same thing it does for two terminals.
//!
//! Almost nothing comes back out. Per the rule that only `colony_send` posts,
//! [`Frame::Text`] is **dropped**: the agent's prose goes to
//! `aphid alate attach`, where a person can read it, and reaches the colony
//! only if the model decided to say it. [`Frame::Confirm`] is left alone, so a
//! terminal or the five-minute timeout answers it — an agent must not be able
//! to grant itself permission by being the only one listening.

use std::collections::VecDeque;
use std::path::PathBuf;

use aphid_nostr::GroupId;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::gateway::Client;
use crate::gateway::wire::{Envelope, Frame, Request};

/// Attach a connection for `group` and start serving it.
///
/// # Errors
///
/// Fails when the socket cannot be reached, which means the daemon has gone.
pub(super) async fn open(
    group: &GroupId,
    label: &str,
    socket: PathBuf,
) -> std::io::Result<UnboundedSender<Request>> {
    // Named, so `/sessions` says `colony: #general` and a person can see where
    // each conversation is being had.
    let client = Client::connect_as(&socket, Some(&format!("colony: {label}"))).await?;
    tracing::info!(group = %group, "colony: group attached");

    let (sender, requests) = mpsc::unbounded_channel();
    tokio::spawn(serve(
        Chat {
            group: group.clone(),
            session: None,
            waiting: VecDeque::new(),
        },
        client,
        requests,
    ));
    Ok(sender)
}

/// What one group holds between frames.
struct Chat {
    group: GroupId,
    /// The conversation this group is in, once the daemon has named it.
    session: Option<String>,
    /// What was said before the daemon named the conversation.
    ///
    /// Nothing can go down the socket until then: a request is stamped with
    /// whatever the connection watches at the moment the line is read, so a
    /// prompt sent in the same breath as the attach carries no session and is
    /// dropped. Held here, it is asked once there is somewhere to ask it of.
    /// The same reason [`crate::telegram::chat`] holds one.
    waiting: VecDeque<Request>,
}

/// One group, until the daemon hangs up or the bridge stops.
async fn serve(mut chat: Chat, mut client: Client, mut requests: UnboundedReceiver<Request>) {
    loop {
        tokio::select! {
            request = requests.recv() => match request {
                None => break,
                Some(request) => {
                    if chat.session.is_none() {
                        chat.waiting.push_back(request);
                        continue;
                    }
                    if client.send(&request).await.is_err() {
                        break;
                    }
                }
            },
            frame = client.recv() => match frame {
                Ok(Some(envelope)) => {
                    if chat.apply(&envelope, &mut client).await.is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            },
        }
    }
    tracing::info!(group = %chat.group, "colony: group detached");
}

impl Chat {
    /// One frame from the daemon.
    async fn apply(&mut self, envelope: &Envelope, client: &mut Client) -> std::io::Result<()> {
        match &envelope.frame {
            // The greeting names the session opened for this connection.
            // Anything said before now has somewhere to go.
            Frame::Hello { .. } => {
                self.session.clone_from(&envelope.session);
                while let Some(request) = self.waiting.pop_front() {
                    client.send(&request).await?;
                }
            }
            // The session ended, so what is said next opens a fresh one.
            Frame::SessionClosed { .. } if envelope.session == self.session => {
                self.session = None;
            }
            // Everything else belongs in `aphid alate attach`. A colony is a
            // chat between people and agents, not a debugging console: the
            // prose, the thinking, the tool calls and the notices all stay on
            // the gateway, and what reaches the colony is what the model chose
            // to say with `colony_send`.
            _ => {}
        }
        Ok(())
    }
}
