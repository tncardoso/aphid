//! Writing and reading session files.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use aphid_core::{Timestamp, Transcript};

use super::format::{self, Header, Line, Record};
use crate::tools::Workspace;

/// Where a workspace keeps its sessions.
#[must_use]
pub fn sessions_dir(workspace: &Workspace) -> PathBuf {
    workspace.root().join(".aphid").join("sessions")
}

/// An append-only session file.
///
/// Holds a watermark of how many messages have been written, so [`flush`] only
/// ever appends what is new.
///
/// [`flush`]: SessionStore::flush
pub struct SessionStore {
    path: PathBuf,
    id: String,
    file: File,
    written: usize,
}

impl SessionStore {
    /// Start a new session in `dir`.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created or the file cannot be opened.
    pub fn create(dir: &Path, cwd: &Path, model: Option<&str>) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let started = chrono::Utc::now();
        let id = new_id(started);
        let path = dir.join(format!("{id}.jsonl"));

        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        let header = Line::Session(Box::new(Header {
            id: id.clone(),
            cwd: cwd.display().to_string(),
            started,
            model: model.map(ToOwned::to_owned),
        }));
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
        file.flush()?;

        Ok(Self {
            path,
            id,
            file,
            written: 0,
        })
    }

    /// Reopen an existing session for appending, and replay it into `transcript`.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or reopened for appending.
    pub fn resume(path: &Path, transcript: &mut Transcript) -> std::io::Result<(Self, Header)> {
        let (header, records) = read(path)?;
        for record in &records {
            format::replay(transcript, record);
        }

        let file = OpenOptions::new().append(true).open(path)?;
        Ok((
            Self {
                path: path.to_path_buf(),
                id: header.id.clone(),
                file,
                written: transcript.len(),
            },
            header,
        ))
    }

    /// Append every message committed since the last call.
    ///
    /// # Errors
    ///
    /// Fails on a write error.
    pub fn flush(&mut self, transcript: &Transcript) -> std::io::Result<()> {
        // A transcript that shrank — `/clear`, or a rebuild by `set_system` —
        // is a different conversation. Rewinding rather than appending keeps the
        // file honest instead of interleaving two histories.
        if transcript.len() < self.written {
            self.written = transcript.len();
            return Ok(());
        }

        for index in self.written..transcript.len() {
            let Some(message) = transcript.get(index) else {
                continue;
            };
            let line = Line::Message(Box::new(format::record(&message)));
            writeln!(self.file, "{}", serde_json::to_string(&line)?)?;
        }
        self.file.flush()?;
        self.written = transcript.len();
        Ok(())
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// What a session file says about itself, without replaying it.
#[derive(Clone, Debug)]
pub struct Summary {
    pub path: PathBuf,
    pub header: Header,
    pub messages: usize,
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
            let (header, records) = read(&path).ok()?;
            Some(Summary {
                path,
                header,
                messages: records.len(),
            })
        })
        .collect();

    summaries.sort_by(|a, b| b.header.started.cmp(&a.header.started));
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
#[must_use]
pub fn resolve(dir: &Path, id: &str) -> Option<Summary> {
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

/// Read a session file into its header and records.
///
/// Unparseable lines are skipped rather than failing the load: a session
/// truncated by a crash should still open.
pub(super) fn read(path: &Path) -> std::io::Result<(Header, Vec<Record>)> {
    let file = File::open(path)?;
    let mut header = None;
    let mut records = Vec::new();

    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(&line) {
            Ok(Line::Session(found)) => header = Some(*found),
            Ok(Line::Message(record)) => records.push(*record),
            Err(_) => continue,
        }
    }

    let header = header.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} has no session header", path.display()),
        )
    })?;
    Ok((header, records))
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
