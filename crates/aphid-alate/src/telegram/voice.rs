//! A recording from a chat, as words for the agent.
//!
//! This is the part of the bridge that does not happen in the poll loop.
//! Fetching a file and reading it take seconds, and the loop is what serves
//! every chat: doing it there would stop the bot — no other conversation, no
//! `/cancel`, no permission button — for as long as it took. So the loop reads
//! the update, finds the recording in it, and starts one of these.
//!
//! The wire gains nothing from any of this. What goes down the socket at the
//! end is the [`Request::Prompt`] a typed message would have been, and the
//! daemon never learns that a telephone was involved.
//!
//! # What is given up
//!
//! Order. A task is loose, so a recording followed by a typed line can reach
//! the agent the other way round, and two recordings can change places if the
//! second is shorter. A `/cancel` sent while one is being read does not stop
//! it either — it is not a run yet — so the prompt arrives after, and the next
//! `/cancel` is the one that stops it.

use serde_json::Value;

#[cfg(feature = "voice")]
use serde_json::json;
#[cfg(feature = "voice")]
use tokio::sync::mpsc::UnboundedSender;

#[cfg(feature = "voice")]
use super::{LIMIT, Shared, chunks};
#[cfg(feature = "voice")]
use crate::gateway::wire::Request;

/// The largest file `getFile` will hand over, which is Telegram's limit and
/// not this one.
#[cfg(feature = "voice")]
const CEILING: u64 = 20 * 1024 * 1024;

/// The recording in a message, if it holds one.
///
/// Four places carry audio. `voice` is the microphone button and is always
/// Opus; `audio` is a music file; `video_note` is the round video, whose sound
/// is worth as much as anyone's; and `document` is anything sent as a file,
/// which is how a telephone attaches an `.ogg` — so it counts only when it
/// says it is audio.
#[must_use]
pub(super) fn recording(message: &Value) -> Option<String> {
    for kind in ["voice", "audio", "video_note"] {
        if let Some(id) = message
            .pointer(&format!("/{kind}/file_id"))
            .and_then(Value::as_str)
        {
            return Some(id.to_owned());
        }
    }

    let document = message.get("document")?;
    let mime = document.get("mime_type").and_then(Value::as_str)?;
    if !mime.starts_with("audio/") {
        return None;
    }
    Some(document.get("file_id")?.as_str()?.to_owned())
}

/// Read `file` and give the words to `sender`, which is the chat's connection.
///
/// Runs until it is done. Every way it can fail ends in a sentence in the chat
/// and a line in the log, and none of them stops the bridge.
#[cfg(feature = "voice")]
pub(super) async fn read(
    chat: i64,
    file: String,
    shared: Shared,
    sender: UnboundedSender<Request>,
) {
    let Some(voice) = shared.voice.clone() else {
        return;
    };

    // Before anything is fetched: a model that is still coming down should not
    // cost the chat a download as well as a wait.
    if let Some(why) = voice.not_yet() {
        say(&shared, chat, &why).await;
        return;
    }

    typing(&shared, chat).await;

    let bytes = match fetch(&shared, &file).await {
        Ok(bytes) => bytes,
        Err(why) => {
            tracing::warn!(chat, %why, "telegram: recording not fetched");
            say(&shared, chat, &why).await;
            return;
        }
    };

    typing(&shared, chat).await;

    // Trimmed here, and not left to the transcriber: whether a recording has
    // words in it is this end's question, and an answer of blanks is an answer
    // of nothing whichever transcriber gave it.
    let text = match voice
        .transcribe(bytes)
        .await
        .map(|text| text.trim().to_owned())
    {
        Ok(text) => text,
        Err(why) => {
            tracing::warn!(chat, %why, "telegram: recording not read");
            say(&shared, chat, &why).await;
            return;
        }
    };

    if text.is_empty() {
        tracing::info!(chat, "telegram: recording had no speech in it");
        say(
            &shared,
            chat,
            "I could not make out anything in that recording",
        )
        .await;
        return;
    }

    tracing::info!(chat, length = text.len(), "telegram: recording read");

    // What was heard, before what it leads to. Speech recognition is wrong
    // often enough that a chat has to be able to see the sentence the agent
    // was actually given.
    for piece in chunks(&format!("🎤 {text}"), LIMIT) {
        say(&shared, chat, piece).await;
    }

    // Never through `command()`. Speech is speech: "/new" said out loud is far
    // too easy to mishear for it to throw a conversation away.
    let _ = sender.send(Request::Prompt { text });
}

#[cfg(feature = "voice")]
/// Ask Telegram where the file is, and then get it.
async fn fetch(shared: &Shared, file: &str) -> Result<Vec<u8>, String> {
    let found = shared
        .api
        .call("getFile", json!({ "file_id": file }))
        .await?;

    if let Some(size) = found.get("file_size").and_then(Value::as_u64)
        && size > CEILING
    {
        return Err(format!(
            "that recording is {} MB, and a bot can only fetch {} MB",
            size / (1024 * 1024),
            CEILING / (1024 * 1024)
        ));
    }

    let path = found
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| "Telegram did not say where that recording is".to_owned())?;

    shared.api.fetch(path).await
}

#[cfg(feature = "voice")]
/// The `typing…` a chat shows while it waits. Telegram holds it for five
/// seconds, so it is renewed as the work goes on.
async fn typing(shared: &Shared, chat: i64) {
    let _ = shared
        .api
        .call(
            "sendChatAction",
            json!({ "chat_id": chat, "action": "typing" }),
        )
        .await;
}

#[cfg(feature = "voice")]
/// One message to a chat.
async fn say(shared: &Shared, chat: i64, text: &str) {
    let _ = shared
        .api
        .call("sendMessage", json!({ "chat_id": chat, "text": text }))
        .await;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_voice_message_is_a_recording() {
        let message = json!({ "voice": { "file_id": "a", "duration": 3 } });
        assert_eq!(recording(&message).as_deref(), Some("a"));
    }

    #[test]
    fn so_are_music_and_a_round_video() {
        assert_eq!(
            recording(&json!({ "audio": { "file_id": "b" } })).as_deref(),
            Some("b")
        );
        assert_eq!(
            recording(&json!({ "video_note": { "file_id": "c" } })).as_deref(),
            Some("c")
        );
    }

    #[test]
    fn a_file_counts_when_it_says_it_is_audio() {
        let message = json!({ "document": { "file_id": "d", "mime_type": "audio/ogg" } });
        assert_eq!(recording(&message).as_deref(), Some("d"));
    }

    #[test]
    fn a_file_that_is_not_audio_does_not() {
        let message = json!({ "document": { "file_id": "e", "mime_type": "application/zip" } });
        assert_eq!(recording(&message), None);
        // And one that does not say what it is stays a file.
        assert_eq!(recording(&json!({ "document": { "file_id": "f" } })), None);
    }

    #[test]
    fn a_photo_or_a_line_of_text_is_not_a_recording() {
        assert_eq!(recording(&json!({ "text": "hello" })), None);
        assert_eq!(recording(&json!({ "photo": [{ "file_id": "g" }] })), None);
    }
}
