//! One window, and how the next `aphid alate gui` reaches it.
//!
//! A Unix socket with one JSON object for each line, which is the gateway's
//! protocol and for the gateway's reason: no bus to install, no service to
//! register, and the same mechanism on Linux and macOS.
//!
//! The socket is `$APHID_HOME/gui.sock` — beside `alate/` and not inside any
//! instance, because there is one window and it can be pointed at any alate.
//! A second `aphid alate gui` is not a second window: it says [`Command::Show`]
//! to the first and exits.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// What one window is told to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Command {
    /// Are you there? The only command that answers with anything but `ok`, and
    /// the one that decides whether this process opens a window or hands over.
    Ping,
    /// Come to the front, expanded.
    Show,
    /// Expand if collapsed, collapse if expanded.
    Toggle,
    /// Console to companion, or back.
    Mode,
    /// Watch this alate instead.
    Instance { name: String },
    /// Draw the creature as this familiar: `sap` or `drift`. A name this build
    /// has never heard of is refused rather than guessed at.
    Familiar { name: String },
    /// Come forward and open the menu. What the right button on a tray icon
    /// sends where the panel draws no menu of its own.
    Menu,
    /// Close the window and stop.
    Quit,
}

/// What a window says back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    /// Answer to [`Command::Ping`]: a window is up, and this is the alate it is
    /// watching.
    Pong { instance: Option<String> },
    /// Done.
    Ok,
    /// The command was understood and refused, with the reason.
    Refused { reason: String },
}

/// The control socket for this machine.
///
/// # Errors
///
/// Fails when there is no home directory to put it in.
pub fn socket_path() -> Result<PathBuf, crate::home::Error> {
    aphid_core::catalog::aphid_dir()
        .map(|dir| dir.join("gui.sock"))
        .ok_or(crate::home::Error::NoHome)
}

/// The remembered window state for this machine.
///
/// # Errors
///
/// Fails when there is no home directory to put it in.
pub fn config_path() -> Result<PathBuf, crate::home::Error> {
    aphid_core::catalog::aphid_dir()
        .map(|dir| dir.join("gui.json"))
        .ok_or(crate::home::Error::NoHome)
}

/// Say one thing to the window listening on `socket`.
///
/// # Errors
///
/// Fails when nothing is listening, which is the ordinary case of no window
/// being open, and when the window hangs up without answering.
pub async fn talk(socket: &Path, command: &Command) -> std::io::Result<Reply> {
    let stream = UnixStream::connect(socket).await?;
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_string(command)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();
    match lines.next_line().await? {
        Some(line) => serde_json::from_str(&line).map_err(std::io::Error::other),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the window hung up without answering",
        )),
    }
}

/// Whether a window is already up, and which alate it is watching.
///
/// Connecting is the only honest test: the socket file outlives the process
/// that made it, so its existence says nothing.
pub async fn running(socket: &Path) -> Option<Option<String>> {
    match talk(socket, &Command::Ping).await {
        Ok(Reply::Pong { instance }) => Some(instance),
        _ => None,
    }
}

/// The listening half, held by the process that owns the window.
#[derive(Debug)]
pub struct Control {
    listener: UnixListener,
    path: PathBuf,
}

impl Control {
    /// Take the socket, clearing a dead one out of the way.
    ///
    /// A window that was killed leaves its socket file behind. Nothing answers
    /// on it, so this removes it and binds again — the same thing a person
    /// would do, and the only way a crash does not need a manual cleanup before
    /// the next window opens.
    ///
    /// # Errors
    ///
    /// Fails when a window is already listening — the caller should hand over
    /// to it rather than open a second one — or when the socket cannot be made.
    pub async fn bind(path: &Path) -> std::io::Result<Self> {
        if running(path).await.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "a window is already open",
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Nothing answered, so whatever is at the path is a leftover.
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(Self {
            listener: UnixListener::bind(path)?,
            path: path.to_path_buf(),
        })
    }

    /// Answer commands until the socket breaks, handing each to `handle`.
    ///
    /// `handle` says what to answer, which is what lets the window report the
    /// alate it is watching in a `Pong` and refuse what it cannot do.
    pub async fn serve<F>(self, mut handle: F)
    where
        F: FnMut(Command) -> Reply + Send,
    {
        loop {
            let Ok((stream, _)) = self.listener.accept().await else {
                return;
            };
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            // One command for each connection. A control socket is not a
            // conversation, and a client that wants two things opens twice.
            let Ok(Some(line)) = lines.next_line().await else {
                continue;
            };
            let reply = match serde_json::from_str::<Command>(&line) {
                Ok(command) => handle(command),
                Err(error) => Reply::Refused {
                    reason: error.to_string(),
                },
            };
            let Ok(mut text) = serde_json::to_string(&reply) else {
                continue;
            };
            text.push('\n');
            let _ = writer.write_all(text.as_bytes()).await;
            let _ = writer.flush().await;
        }
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        // Leave no socket behind for the next window to have to clear.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket_in(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("aphid-gui-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn a_command_goes_out_and_an_answer_comes_back() {
        let path = socket_in("round-trip");
        let control = Control::bind(&path).await.expect("bind");
        let (sent, mut heard) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(control.serve(move |command| {
            let _ = sent.send(command);
            Reply::Ok
        }));

        let reply = talk(&path, &Command::Toggle).await.expect("talk");
        assert_eq!(reply, Reply::Ok);
        assert_eq!(heard.recv().await, Some(Command::Toggle));
    }

    #[tokio::test]
    async fn a_ping_says_which_alate_is_being_watched() {
        let path = socket_in("ping");
        let control = Control::bind(&path).await.expect("bind");
        tokio::spawn(control.serve(|_| Reply::Pong {
            instance: Some("work".to_owned()),
        }));

        assert_eq!(running(&path).await, Some(Some("work".to_owned())));
    }

    #[tokio::test]
    async fn nothing_listening_is_no_window() {
        let path = socket_in("closed");
        assert_eq!(running(&path).await, None);
    }

    #[tokio::test]
    async fn a_socket_left_by_a_dead_window_is_cleared_away() {
        let path = socket_in("stale");
        // What a killed process leaves: a file at the path with nothing behind
        // it. Binding used to fail with "address already in use".
        std::fs::write(&path, b"").expect("stale file");
        let control = Control::bind(&path).await.expect("bind over the leftover");
        tokio::spawn(control.serve(|_| Reply::Ok));
        assert_eq!(talk(&path, &Command::Show).await.expect("talk"), Reply::Ok);
    }

    #[tokio::test]
    async fn a_second_window_is_refused_so_it_can_hand_over() {
        let path = socket_in("second");
        let control = Control::bind(&path).await.expect("first");
        tokio::spawn(control.serve(|_| Reply::Pong { instance: None }));

        let error = Control::bind(&path)
            .await
            .expect_err("the second is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        // And what it does instead of opening a window.
        assert_eq!(
            talk(&path, &Command::Show).await.expect("talk"),
            Reply::Pong { instance: None }
        );
    }

    #[tokio::test]
    async fn a_line_that_is_not_a_command_is_refused_and_the_socket_lives_on() {
        let path = socket_in("garbage");
        let control = Control::bind(&path).await.expect("bind");
        tokio::spawn(control.serve(|_| Reply::Ok));

        let stream = UnixStream::connect(&path).await.expect("connect");
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(b"{\"kind\":\"fly\"}\n")
            .await
            .expect("write");
        let mut lines = BufReader::new(reader).lines();
        let line = lines.next_line().await.expect("read").expect("a line");
        assert!(matches!(
            serde_json::from_str::<Reply>(&line).expect("a reply"),
            Reply::Refused { .. }
        ));
        // The next command still works.
        assert_eq!(talk(&path, &Command::Quit).await.expect("talk"), Reply::Ok);
    }
}
