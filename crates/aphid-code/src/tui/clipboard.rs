//! Putting text on the clipboard, through the terminal itself.
//!
//! OSC 52 and not a system clipboard library: the terminal is the one thing
//! that is always there. It works over SSH, where a library on this side of the
//! link has no display to talk to, and it costs the workspace no dependency.
//!
//! The terminal is free to refuse. Some emulators keep OSC 52 off until they
//! are told otherwise, and there is no way to ask from in here whether this one
//! did. The status line says what aphid sent, which is all it can honestly say.

use std::io::Write;

use crate::base64;

/// The most base64 a terminal is known to read in one sequence.
///
/// Past this the sequence is cut, and a cut sequence puts a truncated
/// selection on the clipboard — worse than nothing, because it looks like it
/// worked. About 55 KB of text.
const MAX_PAYLOAD: usize = 74_994;

/// What a copy came to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Copied {
    /// The sequence went to the terminal. Whether the terminal acted on it is
    /// between the terminal and the user.
    Sent,
    /// More text than a terminal will read.
    TooLarge,
    /// The write to the terminal failed.
    Failed,
}

/// The OSC 52 sequence that puts `text` on the system clipboard.
///
/// `None` when the payload is longer than a terminal will read.
///
/// Inside tmux the sequence has to be wrapped again, or tmux eats it instead of
/// passing it on: without this, copying from a pane is silent and does nothing.
#[must_use]
pub fn osc52(text: &str, tmux: bool) -> Option<String> {
    let payload = base64::encode(text.as_bytes());
    if payload.len() > MAX_PAYLOAD {
        return None;
    }

    let sequence = format!("\x1b]52;c;{payload}\x07");
    if !tmux {
        return Some(sequence);
    }
    // tmux passes a sequence through when it is wrapped in a DCS of its own,
    // and every escape inside has to be doubled so tmux does not read the
    // inner sequence as the end of the outer one.
    Some(format!(
        "\x1bPtmux;{}\x1b\\",
        sequence.replace('\x1b', "\x1b\x1b")
    ))
}

/// Put `text` on the clipboard.
///
/// Safe to call between frames, which is the only time it is called: OSC 52
/// moves no cursor and paints no cell, so what ratatui drew stays drawn.
pub fn copy(text: &str) -> Copied {
    let Some(sequence) = osc52(text, std::env::var_os("TMUX").is_some()) else {
        return Copied::TooLarge;
    };

    let mut out = std::io::stdout();
    if out.write_all(sequence.as_bytes()).is_err() || out.flush().is_err() {
        return Copied::Failed;
    }
    Copied::Sent
}

#[cfg(test)]
mod tests {
    use super::{Copied, MAX_PAYLOAD, osc52};
    use crate::base64;

    #[test]
    fn an_osc52_sequence_carries_the_text() {
        let sequence = osc52("hello", false).expect("short enough");
        let payload = sequence
            .strip_prefix("\x1b]52;c;")
            .and_then(|rest| rest.strip_suffix('\x07'))
            .expect("the shape of the sequence");
        assert_eq!(base64::decode(payload).unwrap(), b"hello");
    }

    #[test]
    fn tmux_gets_the_sequence_wrapped() {
        let plain = osc52("hello", false).expect("short enough");
        let wrapped = osc52("hello", true).expect("short enough");

        assert!(wrapped.starts_with("\x1bPtmux;"));
        assert!(wrapped.ends_with("\x1b\\"));
        assert!(
            wrapped.contains(&plain.replace('\x1b', "\x1b\x1b")),
            "the inner sequence is carried with its escapes doubled"
        );
    }

    #[test]
    fn too_much_text_is_refused_rather_than_cut() {
        // Three bytes of text become four of base64, so this clears the cap.
        let huge = "x".repeat(MAX_PAYLOAD);
        assert_eq!(osc52(&huge, false), None);
        assert_eq!(super::copy(&huge), Copied::TooLarge);
    }
}
