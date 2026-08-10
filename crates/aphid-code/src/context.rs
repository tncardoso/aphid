//! Project instructions: the `AGENTS.md` files that tell the agent how this
//! repository works.
//!
//! Files are collected from the workspace root down to the current directory, so
//! the most specific one is last and has the final word. A global
//! `~/.aphid/AGENTS.md` comes first, before any project's own.

use std::path::{Path, PathBuf};

use crate::tools::Workspace;

/// The file name looked for at each level.
pub const FILE_NAME: &str = "AGENTS.md";

/// One loaded instruction file.
#[derive(Clone, Debug)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// Collect instruction files for a working directory inside a workspace.
///
/// `home` is where the global `~/.aphid/AGENTS.md` lives; pass `None` to skip it.
/// It is a parameter rather than an environment lookup so this is testable
/// without mutating process-wide state.
///
/// Unreadable and empty files are skipped: a broken `AGENTS.md` should not stop
/// the agent from starting.
#[must_use]
pub fn discover(workspace: &Workspace, cwd: &Path, home: Option<&Path>) -> Vec<ContextFile> {
    let mut files = Vec::new();

    if let Some(home) = home {
        push_if_readable(&mut files, &home.join(".aphid").join(FILE_NAME));
    }

    // Walk up from cwd collecting directories, then visit them outermost first
    // so the deepest file is applied last.
    let mut directories = Vec::new();
    let mut candidate = Some(cwd);
    while let Some(directory) = candidate {
        directories.push(directory.to_path_buf());
        if directory == workspace.root() {
            break;
        }
        candidate = directory.parent();
    }
    directories.reverse();

    for directory in directories {
        push_if_readable(&mut files, &directory.join(FILE_NAME));
    }

    files
}

fn push_if_readable(files: &mut Vec<ContextFile>, path: &Path) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    if content.trim().is_empty() {
        return;
    }
    // The same file can be reached twice when cwd is the workspace root.
    if files.iter().any(|file| file.path == path) {
        return;
    }
    files.push(ContextFile {
        path: path.to_path_buf(),
        content,
    });
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}
