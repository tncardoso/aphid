//! Sessions on disk.
//!
//! A session is one JSONL file per conversation, appended to as messages are
//! committed. Nothing is ever rewritten, so a crash costs at most the turn that
//! was in flight, and `--resume` is a replay of the file.
//!
//! The messages of a session form a tree: each one names its parent, so a
//! conversation can be forked at any message and continued on a new branch,
//! and the file keeps every branch. A `head` line records where the session
//! was left, and a resume comes back there.

pub mod format;
mod plugin;
mod store;
pub mod tree;

pub use format::{AssistantRecord, Block, Header, Line, Record, ToolResultRecord};
pub use plugin::SessionComponent;
pub use store::{
    Contents, SessionStore, Summary, append_label, list, list_for, newest_for, read, resolve,
    sessions_dir, split_address,
};
pub use tree::{Tree, TreeView, Turn};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aphid_agent::Agent;
use aphid_core::{Role, Transcript};

/// A session to continue, and the message on it to continue from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resume {
    pub path: PathBuf,
    /// A message id, or a prefix of one. `None` is the session's head.
    pub at: Option<String>,
}

impl Resume {
    /// The session in `path`, at its head.
    #[must_use]
    pub fn head(path: PathBuf) -> Self {
        Self { path, at: None }
    }
}

/// Open a session — new, or continuing one — and produce the plugin that keeps
/// it up to date, plus whatever conversation needs splicing back in.
///
/// The plugin has to exist before the agent is built, because plugins are
/// registered on the builder. The restored transcript is therefore returned
/// rather than applied, and [`splice`] puts it in afterwards.
///
/// # Errors
///
/// Fails when the session directory or file cannot be opened, or when the
/// message to resume at is not in the file.
pub fn attach(
    dir: &Path,
    root: &Path,
    cwd: &Path,
    model: Option<&str>,
    resume: Option<&Resume>,
    listeners: Arc<aphid_agent::TranscriptListeners>,
) -> std::io::Result<(Arc<SessionComponent>, Option<Transcript>)> {
    let (store, restored) = match resume {
        Some(resume) => {
            let mut transcript = Transcript::new();
            let (store, _header) =
                SessionStore::resume(&resume.path, resume.at.as_deref(), &mut transcript)?;
            (store, Some(transcript))
        }
        None => (SessionStore::create(dir, root, cwd, model)?, None),
    };
    Ok((Arc::new(SessionComponent::new(store, listeners)), restored))
}

/// Read a session file at its head, without opening it to write.
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
    load_at(path, None)
}

/// Read one branch of a session file: the path from the root to `at`, a
/// message id or a prefix of one. `None` is the head.
///
/// # Errors
///
/// Fails as [`load`] does, and when `at` names no message.
pub fn load_at(path: &Path, at: Option<&str>) -> std::io::Result<(format::Header, Transcript)> {
    let contents = store::read(path)?;
    let target = match at {
        Some(at) => Some(contents.resolve(at).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} has no message {at}", path.display()),
            )
        })?),
        None => contents.head.clone(),
    };
    let mut transcript = Transcript::new();
    for index in contents.path_to(target.as_deref()) {
        format::replay(&mut transcript, &contents.records[index]);
    }
    Ok((contents.header, transcript))
}

/// Append a restored conversation to a freshly built agent.
///
/// The saved system prompt is dropped: resuming should pick up today's project
/// context, not replay yesterday's. So are system notes from the middle of the
/// conversation. Returns how many messages were restored.
///
/// `restored` must be what `session` replayed, message for message: the
/// session is told which message landed where, so what the agent adds next is
/// written after the right one.
pub fn splice(
    agent: &mut Agent,
    restored: &Transcript,
    session: Option<&SessionComponent>,
) -> usize {
    let base = agent.transcript().len();
    let keep: Vec<usize> = (0..restored.len())
        .filter(|index| {
            restored
                .get(*index)
                .is_some_and(|message| message.role() != Role::System)
        })
        .collect();
    let ids: Vec<_> = keep
        .iter()
        .filter_map(|index| restored.id_at(*index))
        .collect();
    restored.compact_into(&ids, agent.transcript_mut());

    if let Some(session) = session {
        let _ = session.with_store(|store| {
            let old = store.line();
            // Today's system prompt stands in for the saved one, so a branch
            // that starts right after the system prompt hangs from it.
            let saved_system = restored
                .get(0)
                .is_some_and(|message| message.role() == Role::System);
            let mut line: Vec<Option<String>> = (0..base)
                .map(|index| {
                    if index == 0 && saved_system {
                        old.first().cloned().flatten()
                    } else {
                        None
                    }
                })
                .collect();
            line.extend(keep.iter().map(|index| old.get(*index).cloned().flatten()));
            let tip = store.tip().map(ToOwned::to_owned);
            store.adopt(line, tip);
        });
    }
    keep.len()
}

/// How a checkout picks where the session continues.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Move {
    /// Continue the newest branch under the message.
    Jump,
    /// Start a new branch at the message. At a prompt, the branch starts
    /// before it, and the prompt comes back to be edited and sent again. At
    /// an answer that ends its turn, the branch starts after it.
    Fork,
}

/// What a checkout did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    /// The message the session now continues from.
    pub head: Option<String>,
    /// A prompt to put back in the input, for a fork at a prompt.
    pub prefill: Option<String>,
}

/// Move the session to another message of its tree, and the agent's
/// transcript with it.
///
/// A message on the branch the agent holds is reached by cutting the
/// transcript back, which copies nothing. Any other message is reached by
/// replaying its branch from the file.
///
/// # Errors
///
/// Fails when the file cannot be read or written, when `node` is not in it,
/// or when a fork would start in the middle of a turn.
pub fn checkout(
    session: &SessionComponent,
    agent: &mut Agent,
    node: &str,
    how: Move,
) -> Result<Checkout, String> {
    let contents = session
        .with_store(|store| store.contents())?
        .map_err(|error| error.to_string())?;
    let node = contents
        .resolve(node)
        .ok_or_else(|| format!("there is no message {node} in this session"))?;
    let record = contents
        .get(&node)
        .ok_or_else(|| format!("there is no message {node} in this session"))?;

    let (base, prefill) = match how {
        Move::Jump => (contents.newest_leaf_under(&node), None),
        Move::Fork => match record.role {
            Role::User => (record.parent.clone(), Some(text_of(record))),
            Role::System => (Some(node), None),
            Role::Assistant
                if !record
                    .content
                    .iter()
                    .any(|block| matches!(block, Block::ToolCall { .. })) =>
            {
                (Some(node), None)
            }
            Role::Assistant | Role::ToolResult => {
                return Err(
                    "a branch cannot start in the middle of a turn: pick a prompt or a final answer"
                        .to_owned(),
                );
            }
        },
    };

    move_to(session, agent, &contents, base.as_deref())?;
    Ok(Checkout {
        head: base,
        prefill,
    })
}

fn move_to(
    session: &SessionComponent,
    agent: &mut Agent,
    contents: &Contents,
    base: Option<&str>,
) -> Result<(), String> {
    let position = base.and_then(|base| {
        session
            .with_store(|store| store.position(base))
            .ok()
            .flatten()
    });

    match position {
        Some(index) => {
            agent.transcript_mut().truncate(index + 1);
            session
                .with_store(|store| {
                    store.rewind(index + 1);
                    store.mark_head()
                })?
                .map_err(|error| error.to_string())
        }
        None => {
            let mut restored = Transcript::new();
            let mut line = Vec::new();
            for index in contents.path_to(base) {
                let record = &contents.records[index];
                format::replay(&mut restored, record);
                line.push(record.id.clone());
            }
            let keep = system_prompt(agent.transcript());
            agent.transcript_mut().truncate(keep);
            session.with_store(|store| store.adopt(line, base.map(ToOwned::to_owned)))?;
            splice(agent, &restored, Some(session));
            session
                .with_store(SessionStore::mark_head)?
                .map_err(|error| error.to_string())
        }
    }
}

/// Continue another session file, at `resume`, in place of the one being
/// written now. Returns the restored transcript's message count.
///
/// # Errors
///
/// Fails when the file cannot be opened or the message is not in it.
pub fn open(
    session: &SessionComponent,
    agent: &mut Agent,
    resume: &Resume,
) -> Result<usize, String> {
    let mut restored = Transcript::new();
    let (store, _header) = SessionStore::resume(&resume.path, resume.at.as_deref(), &mut restored)
        .map_err(|error| error.to_string())?;
    session.switch(store)?;
    let keep = system_prompt(agent.transcript());
    agent.transcript_mut().truncate(keep);
    Ok(splice(agent, &restored, Some(session)))
}

/// Start a new session file in place of the one being written now, and clear
/// the conversation. The system prompt stays.
///
/// # Errors
///
/// Fails when the file cannot be created.
pub fn start(
    session: &SessionComponent,
    agent: &mut Agent,
    dir: &Path,
    root: &Path,
    cwd: &Path,
) -> Result<(), String> {
    let model = agent.model().id.to_string();
    let store =
        SessionStore::create(dir, root, cwd, Some(&model)).map_err(|error| error.to_string())?;
    session.switch(store)?;
    let keep = system_prompt(agent.transcript());
    agent.transcript_mut().truncate(keep);
    Ok(())
}

/// Name a branch: the one that holds the turn `at`, or the one the session is
/// on. The label goes on the first turn of that branch. On the first branch,
/// it names the session.
///
/// # Errors
///
/// Fails when the file cannot be read or written, or holds no prompt yet.
pub fn rename(session: &SessionComponent, at: Option<&str>, text: &str) -> Result<(), String> {
    let path = session.path().ok_or("the session is not being saved")?;
    let view = Tree::read(&path).map_err(|error| error.to_string())?.view();
    let start = match at {
        Some(at) => view.branch_start_of(at),
        None => view.branch_start(),
    };
    let start = start
        .or_else(|| view.turns.last())
        .ok_or("there is nothing to name yet: send a prompt first")?;
    let node = start.id.clone();
    session
        .with_store(|store| store.label(&node, text))?
        .map_err(|error| error.to_string())
}

/// How many messages at the start of `transcript` are the system prompt.
fn system_prompt(transcript: &Transcript) -> usize {
    usize::from(
        transcript
            .get(0)
            .is_some_and(|message| message.role() == Role::System),
    )
}

/// The text of a message, for putting a prompt back in the input.
fn text_of(record: &Record) -> String {
    record
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
