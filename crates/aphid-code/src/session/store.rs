//! Writing and reading session files.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use aphid_core::{Timestamp, Transcript};

use super::format::{self, HeadRecord, Header, LabelRecord, Line, Record};
use super::tree::Tree;

/// `$APHID_HOME/sessions`, or `~/.aphid/sessions` — one directory, shared by
/// every project on the machine.
///
/// Without a home directory (a container with no `$HOME`, say), this falls
/// back to a temporary directory: aphid keeps running, it just does not
/// persist the session past a reboot, rather than refusing to start.
#[must_use]
pub fn sessions_dir() -> PathBuf {
    aphid_core::catalog::aphid_dir()
        .map(|dir| dir.join("sessions"))
        .unwrap_or_else(|| std::env::temp_dir().join("aphid").join("sessions"))
}

/// The project name used in a session's filename, purely cosmetic: the last
/// component of `root`. Not used for filtering — see [`list_for`].
fn slug(root: &Path) -> std::borrow::Cow<'_, str> {
    root.file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or(std::borrow::Cow::Borrowed("root"))
}

/// The sessions in `dir` that belong to the project at `root`, newest first.
///
/// Filtered by `header.cwd` compared to `root` component-by-component
/// (`Path::starts_with`), not by filename: two projects whose last path
/// component is a prefix of the other's (`app` and `app-backend`, say) would
/// make a string-prefix filter on the filename ambiguous
/// (`"app-backend-...".starts_with("app-")` is `true`), so the filename stays
/// cosmetic and filtering goes through the full path already recorded in
/// each session's header.
#[must_use]
pub fn list_for(dir: &Path, root: &Path) -> Vec<Summary> {
    list(dir)
        .into_iter()
        .filter(|summary| Path::new(&summary.header.cwd).starts_with(root))
        .collect()
}

/// An append-only session file, holding a tree of messages.
///
/// Every message is a node with an id and a parent, so a session can branch:
/// a fork writes the new branch's messages with the fork point as their parent,
/// and nothing already written changes.
///
/// The store maps each message of the agent's transcript to the node it was
/// written as, so [`flush`] only ever appends what is new, and a checkout knows
/// which part of the transcript it can keep.
///
/// [`flush`]: SessionStore::flush
pub struct SessionStore {
    path: PathBuf,
    id: String,
    file: File,
    /// The node each message of the agent's transcript was written as, by
    /// index. `None` for one that is not in the file: the system prompt built
    /// fresh for a resumed session, say.
    line: Vec<Option<String>>,
    /// The node the next message hangs from. `None` starts a new root.
    tip: Option<String>,
}

impl SessionStore {
    /// Start a new session in `dir`.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created or the file cannot be opened.
    pub fn create(
        dir: &Path,
        root: &Path,
        cwd: &Path,
        model: Option<&str>,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let started = chrono::Utc::now();
        let id = new_id(started);
        let path = dir.join(format!("{}-{id}.jsonl", slug(root)));

        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        let mut store = Self {
            path,
            id: id.clone(),
            file,
            line: Vec::new(),
            tip: None,
        };
        store.write(&Line::Session(Box::new(Header {
            id,
            cwd: cwd.display().to_string(),
            started,
            model: model.map(ToOwned::to_owned),
        })))?;
        Ok(store)
    }

    /// Reopen an existing session for appending, and replay one branch of it
    /// into `transcript`.
    ///
    /// The branch is the path from the root to `at`, a node id or a prefix of
    /// one. Without `at` it is the session's head: where it was left.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or reopened for appending, or when
    /// `at` names no node.
    pub fn resume(
        path: &Path,
        at: Option<&str>,
        transcript: &mut Transcript,
    ) -> std::io::Result<(Self, Header)> {
        let contents = read(path)?;
        let target = match at {
            Some(at) => Some(contents.resolve(at).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} has no message {at}", path.display()),
                )
            })?),
            None => contents.head.clone(),
        };

        let mut line = Vec::new();
        for index in contents.path_to(target.as_deref()) {
            let record = &contents.records[index];
            format::replay(transcript, record);
            line.push(record.id.clone());
        }

        let file = OpenOptions::new().append(true).open(path)?;
        Ok((
            Self {
                path: path.to_path_buf(),
                id: contents.header.id.clone(),
                file,
                line,
                tip: target,
            },
            contents.header,
        ))
    }

    /// Append every message committed since the last call.
    ///
    /// # Errors
    ///
    /// Fails on a write error.
    pub fn flush(&mut self, transcript: &Transcript) -> std::io::Result<()> {
        // A transcript that shrank without a checkout is a rewind all the same:
        // what follows hangs from the last message that is still there, and the
        // file says so, rather than interleaving two histories.
        if transcript.len() < self.line.len() {
            self.rewind(transcript.len());
            self.mark_head()?;
        }

        for index in self.line.len()..transcript.len() {
            let Some(message) = transcript.get(index) else {
                continue;
            };
            let id = new_node_id();
            let mut record = format::record(&message);
            record.id = Some(id.clone());
            record.parent = self.tip.take();
            self.write(&Line::Message(Box::new(record)))?;
            self.line.push(Some(id.clone()));
            self.tip = Some(id);
        }
        Ok(())
    }

    /// Where `node` sits in the agent's transcript, when it is on the branch
    /// being written.
    #[must_use]
    pub fn position(&self, node: &str) -> Option<usize> {
        self.line.iter().position(|id| id.as_deref() == Some(node))
    }

    /// Keep the first `keep` messages of the branch and continue from the last
    /// of them. The transcript has to be cut to the same length.
    pub fn rewind(&mut self, keep: usize) {
        self.line.truncate(keep);
        self.tip = self.line.last().cloned().flatten();
    }

    /// Replace the map from the transcript to the file, after the transcript
    /// was rebuilt: `line` holds a node per message, and `tip` is where the
    /// next message hangs.
    pub fn adopt(&mut self, line: Vec<Option<String>>, tip: Option<String>) {
        self.line = line;
        self.tip = tip;
    }

    /// The node each message of the agent's transcript was written as.
    #[must_use]
    pub fn line(&self) -> &[Option<String>] {
        &self.line
    }

    /// The node the next message hangs from.
    #[must_use]
    pub fn tip(&self) -> Option<&str> {
        self.tip.as_deref()
    }

    /// Record in the file that the session continues from the tip, so a
    /// resume comes back to it.
    ///
    /// # Errors
    ///
    /// Fails on a write error.
    pub fn mark_head(&mut self) -> std::io::Result<()> {
        self.write(&Line::Head(HeadRecord {
            at: self.tip.clone(),
            ts: chrono::Utc::now(),
        }))
    }

    /// Name the branch that starts at `node`.
    ///
    /// # Errors
    ///
    /// Fails on a write error.
    pub fn label(&mut self, node: &str, text: &str) -> std::io::Result<()> {
        self.write(&label_line(node, text))
    }

    /// Everything in the file now, including what other writers appended.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read.
    pub fn contents(&self) -> std::io::Result<Contents> {
        read(&self.path)
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write one line with one `write` call.
    ///
    /// The file is opened to append, so the kernel puts each write at the end
    /// on its own. Two writers on one session (a fork kept open next to the
    /// conversation it came from) therefore never tear each other's lines,
    /// and need no lock.
    fn write(&mut self, line: &Line) -> std::io::Result<()> {
        let mut text = serde_json::to_string(line)?;
        text.push('\n');
        self.file.write_all(text.as_bytes())?;
        self.file.flush()
    }
}

fn label_line(node: &str, text: &str) -> Line {
    Line::Label(LabelRecord {
        node: node.to_owned(),
        text: text.to_owned(),
        ts: chrono::Utc::now(),
    })
}

/// Name the branch that starts at `node`, in a session file nobody here is
/// writing. One line, appended in one write, as [`SessionStore`] writes.
///
/// # Errors
///
/// Fails when the file cannot be opened or written.
pub fn append_label(path: &Path, node: &str, text: &str) -> std::io::Result<()> {
    let mut line = serde_json::to_string(&label_line(node, text))?;
    line.push('\n');
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(line.as_bytes())
}

/// What a session file says about itself, without replaying it.
#[derive(Clone, Debug)]
pub struct Summary {
    pub path: PathBuf,
    pub header: Header,
    /// How many messages the file holds, on every branch.
    pub messages: usize,
    /// The name of the session: its label, or its first prompt.
    pub title: String,
}

/// Every readable session in `dir`, newest first.
#[must_use]
pub fn list(dir: &Path) -> Vec<Summary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut summaries: Vec<Summary> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| {
            let tree = Tree::read(&path).ok()?;
            Some(Summary {
                messages: tree.nodes.len(),
                title: tree.title(),
                header: tree.header,
                path,
            })
        })
        .collect();

    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.header.started));
    summaries
}

/// The most recent session recorded for `cwd`.
#[must_use]
pub fn newest_for(dir: &Path, cwd: &Path) -> Option<Summary> {
    let cwd = cwd.display().to_string();
    list(dir)
        .into_iter()
        .find(|summary| summary.header.cwd == cwd)
}

/// Find a session by id, or by a prefix of one.
///
/// An address of one message, `<session>:<node>`, finds its session; the
/// node is for [`split_address`] to take apart.
#[must_use]
pub fn resolve(dir: &Path, id: &str) -> Option<Summary> {
    let (id, _) = split_address(id);
    let sessions = list(dir);
    sessions
        .iter()
        .find(|summary| summary.header.id == id)
        .or_else(|| {
            sessions
                .iter()
                .find(|summary| summary.header.id.starts_with(id))
        })
        .cloned()
}

/// Take `<session>:<node>` apart, at the last `:`. A message id has no `:`,
/// so a bare id comes back with no node, and an alate's id for a fork, which
/// is itself an address, keeps its own `:`.
#[must_use]
pub fn split_address(address: &str) -> (&str, Option<&str>) {
    match address.rsplit_once(':') {
        Some((session, node)) if !node.is_empty() => (session, Some(node)),
        Some((session, _)) => (session, None),
        None => (address, None),
    }
}

/// Everything a session file holds, in the order it was written.
#[derive(Clone, Debug)]
pub struct Contents {
    pub header: Header,
    /// Every message, each with an id: a file from before sessions were trees
    /// is given ids by position, each message the child of the one before.
    pub records: Vec<Record>,
    /// Where the session continues: the last node a `Head` line or a message
    /// named.
    pub head: Option<String>,
    /// The last name given to each branch, by the node it starts at.
    pub labels: HashMap<String, String>,
    index: HashMap<String, usize>,
}

impl Contents {
    /// The record with this id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Record> {
        self.index.get(id).map(|index| &self.records[*index])
    }

    /// The full id of the node `id` names, exactly or by a prefix.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<String> {
        if self.index.contains_key(id) {
            return Some(id.to_owned());
        }
        self.records
            .iter()
            .filter_map(|record| record.id.as_deref())
            .find(|candidate| candidate.starts_with(id))
            .map(ToOwned::to_owned)
    }

    /// The records from the root to `at`, by index. Empty for `None`.
    #[must_use]
    pub fn path_to(&self, at: Option<&str>) -> Vec<usize> {
        let mut path = Vec::new();
        let mut next = at.and_then(|id| self.index.get(id).copied());
        while let Some(index) = next {
            // A file edited by hand could hold a cycle; a path is never longer
            // than the file.
            if path.len() > self.records.len() {
                break;
            }
            path.push(index);
            next = self.records[index]
                .parent
                .as_deref()
                .and_then(|parent| self.index.get(parent).copied());
        }
        path.reverse();
        path
    }

    /// The newest message under `id`, which is a leaf: a message is always
    /// written after its parent, so the newest one has no children yet.
    #[must_use]
    pub fn newest_leaf_under(&self, id: &str) -> Option<String> {
        let start = *self.index.get(id)?;
        let mut under = vec![false; self.records.len()];
        under[start] = true;
        let mut newest = start;
        for (index, record) in self.records.iter().enumerate().skip(start + 1) {
            let parent = record
                .parent
                .as_deref()
                .and_then(|parent| self.index.get(parent));
            if parent.is_some_and(|parent| under[*parent]) {
                under[index] = true;
                newest = index;
            }
        }
        self.records[newest].id.clone()
    }
}

/// Read a session file into its header and messages.
///
/// Unparseable lines are skipped rather than failing the load: a session
/// truncated by a crash should still open.
///
/// # Errors
///
/// Fails when the file cannot be read or has no header.
pub fn read(path: &Path) -> std::io::Result<Contents> {
    let file = File::open(path)?;
    let mut header = None;
    let mut records: Vec<Record> = Vec::new();
    let mut head = None;
    let mut labels = HashMap::new();
    let mut index = HashMap::new();

    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(&line) {
            Ok(Line::Session(found)) => header = Some(*found),
            Ok(Line::Message(record)) => {
                let mut record = *record;
                if record.id.is_none() {
                    // Written before sessions were trees: a line, in order.
                    record.id = Some(records.len().to_string());
                    record.parent = records.last().and_then(|last| last.id.clone());
                }
                head.clone_from(&record.id);
                if let Some(id) = &record.id {
                    index.insert(id.clone(), records.len());
                }
                records.push(record);
            }
            Ok(Line::Head(moved)) => head = moved.at,
            Ok(Line::Label(label)) => {
                labels.insert(label.node, label.text);
            }
            Err(_) => continue,
        }
    }

    let header = header.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} has no session header", path.display()),
        )
    })?;
    Ok(Contents {
        header,
        records,
        head,
        labels,
        index,
    })
}

/// A sortable, unique-enough id: a timestamp plus a counter.
///
/// Sessions are per-workspace and created one at a time, so this does not need
/// to be a ULID.
fn new_id(now: Timestamp) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "{}-{:04x}",
        now.format("%Y%m%dT%H%M%S"),
        COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff
    )
}

/// Eight random hex digits: the id of one message.
///
/// Random rather than counted, so two writers on one file need not agree on
/// the next number. Within one session a clash is a one-in-four-billion chance
/// per pair of messages.
fn new_node_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u32(std::process::id());
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        hasher.write_u128(now.as_nanos());
    }
    format!("{:08x}", hasher.finish() as u32)
}
