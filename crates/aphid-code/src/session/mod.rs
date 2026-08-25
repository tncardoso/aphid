//! Sessions on disk.
//!
//! A session is one JSONL file per conversation, appended to as messages are
//! committed. Nothing is ever rewritten, so a crash costs at most the turn that
//! was in flight, and `--resume` is a replay of the file.

pub mod format;
mod plugin;
mod store;

pub use format::{AssistantRecord, Block, Header, Line, Record, ToolResultRecord};
pub use plugin::SessionComponent;
pub use store::{SessionStore, Summary, list, list_for, newest_for, resolve, sessions_dir};

use std::path::Path;
use std::sync::Arc;

use aphid_agent::Agent;
use aphid_core::{Role, Transcript};

/// Open a session — new, or continuing one — and produce the plugin that keeps
/// it up to date, plus whatever conversation needs splicing back in.
///
/// The plugin has to exist before the agent is built, because plugins are
/// registered on the builder. The restored transcript is therefore returned
/// rather than applied, and [`splice`] puts it in afterwards.
///
/// # Errors
///
/// Fails when the session directory or file cannot be opened.
pub fn attach(
    dir: &Path,
    root: &Path,
    cwd: &Path,
    model: Option<&str>,
    resume: Option<&Path>,
    listeners: Arc<aphid_agent::TranscriptListeners>,
) -> std::io::Result<(Arc<SessionComponent>, Option<Transcript>)> {
    let (store, restored) = match resume {
        Some(path) => {
            let mut transcript = Transcript::new();
            let (store, _header) = SessionStore::resume(path, &mut transcript)?;
            (store, Some(transcript))
        }
        None => (SessionStore::create(dir, root, cwd, model)?, None),
    };
    Ok((Arc::new(SessionComponent::new(store, listeners)), restored))
}

/// Read a session file, without opening it to write.
///
/// [`SessionStore::resume`] is the other way to get a conversation back, and it
/// opens the file for appending, because resuming means continuing to write it.
/// Showing a session that has ended is the other case: a front end that lists
/// what an agent did must not become a second writer of it.
///
/// # Errors
///
/// Fails when the file cannot be opened or read. Lines that do not parse are
/// skipped, so a session truncated by a crash still opens.
pub fn load(path: &Path) -> std::io::Result<(format::Header, Transcript)> {
    let (header, records) = store::read(path)?;
    let mut transcript = Transcript::new();
    for record in &records {
        format::replay(&mut transcript, record);
    }
    Ok((header, transcript))
}

/// Append a restored conversation to a freshly built agent.
///
/// The saved system prompt is dropped: resuming should pick up today's project
/// context, not replay yesterday's. Returns how many messages were restored.
pub fn splice(agent: &mut Agent, restored: &Transcript) -> usize {
    let keep: Vec<_> = (0..restored.len())
        .filter(|index| {
            restored
                .get(*index)
                .is_some_and(|message| message.role() != Role::System)
        })
        .filter_map(|index| restored.id_at(index))
        .collect();

    restored.compact_into(&keep, agent.transcript_mut());
    keep.len()
}
