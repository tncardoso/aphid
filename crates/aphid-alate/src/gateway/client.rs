//! The attaching side of the socket.
//!
//! Thin on purpose. A client sends [`Request`]s and receives [`Envelope`]s; it
//! holds no agent, no memory and no opinion about what the frames mean. The
//! terminal in [`crate::tui`] is one client, and anything that can write a line
//! of JSON to a socket is another.

use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use super::wire::{Envelope, Request};

/// A connection to a running daemon.
pub struct Client {
    lines: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
}

impl Client {
    /// Attach to the instance listening on `socket`.
    ///
    /// # Errors
    ///
    /// Fails when nothing is listening, which is the ordinary case of an
    /// instance that is not running.
    pub async fn connect(socket: &Path) -> std::io::Result<Self> {
        Self::connect_as(socket, None).await
    }

    /// Attach, and say what this client is.
    ///
    /// The name is shown against the session in a listing, so that somebody
    /// reading it can tell a conversation in a terminal from one somewhere
    /// else. A terminal passes `None` and is listed as attached.
    ///
    /// # Errors
    ///
    /// Fails when nothing is listening.
    pub async fn connect_as(socket: &Path, channel: Option<&str>) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket).await?;
        let (reader, writer) = stream.into_split();
        let mut client = Self {
            lines: BufReader::new(reader).lines(),
            writer,
        };
        // Announce, because connecting alone does not: `is_listening` connects
        // too, and an alate must not open a conversation for every check that
        // it is awake.
        client
            .send(&Request::Attach {
                channel: channel.map(ToOwned::to_owned),
            })
            .await?;
        Ok(client)
    }

    /// Ask the daemon for something.
    ///
    /// # Errors
    ///
    /// Fails when the daemon has gone.
    pub async fn send(&mut self, request: &Request) -> std::io::Result<()> {
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await
    }

    /// The next envelope, or `None` when the daemon closed the connection.
    ///
    /// A line that does not parse is skipped rather than fatal: a daemon one
    /// version ahead may send a frame this build has no name for, and dropping
    /// the connection over it would be the worst of the available answers.
    ///
    /// # Errors
    ///
    /// Fails when the connection cannot be read.
    pub async fn recv(&mut self) -> std::io::Result<Option<Envelope>> {
        loop {
            let Some(line) = self.lines.next_line().await? else {
                return Ok(None);
            };
            if let Ok(envelope) = serde_json::from_str::<Envelope>(&line) {
                return Ok(Some(envelope));
            }
        }
    }
}

/// Whether an instance is listening on this socket.
///
/// Connecting and hanging up is the only honest test: a socket file exists
/// whether or not a daemon is behind it.
#[must_use]
pub fn is_listening(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}
