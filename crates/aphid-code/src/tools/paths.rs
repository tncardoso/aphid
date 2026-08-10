//! Path resolution, anchored to the workspace.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The directory the agent works in.
///
/// Cheap to clone: every tool closure captures one, and a tool closure has to be
/// `'static`.
#[derive(Clone, Debug)]
pub struct Workspace {
    root: Arc<PathBuf>,
}

impl Workspace {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        // Canonicalise once, so containment checks compare like with like. A
        // root that does not exist is kept as given rather than failing here.
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root: Arc::new(root),
        }
    }

    /// The current directory, walked up to the enclosing git repository when
    /// there is one.
    #[must_use]
    pub fn discover() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut candidate = cwd.as_path();
        loop {
            if candidate.join(".git").exists() {
                return Self::new(candidate);
            }
            match candidate.parent() {
                Some(parent) => candidate = parent,
                None => break,
            }
        }
        Self::new(cwd)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a tool-supplied path against the workspace root.
    ///
    /// Normalisation is lexical, because `write` has to name files that do not
    /// exist yet. This is a guardrail against a model wandering out of the
    /// project by accident, not a sandbox — the process still runs with your
    /// permissions, and a symlink inside the tree can still point outside it.
    ///
    /// # Errors
    ///
    /// Fails when the path resolves outside the workspace root.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let raw = Path::new(path);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };

        let normalised = normalise(&joined);
        if !normalised.starts_with(self.root.as_path()) {
            return Err(format!(
                "`{path}` resolves to {} , which is outside the workspace at {}",
                normalised.display(),
                self.root.display()
            ));
        }
        Ok(normalised)
    }

    /// Render a path relative to the root, for display.
    #[must_use]
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(self.root.as_path())
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::discover()
    }
}

/// Resolve `.` and `..` textually. Unlike `canonicalize` this works for paths
/// that do not exist yet.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> Workspace {
        Workspace {
            root: Arc::new(PathBuf::from("/work/project")),
        }
    }

    #[test]
    fn relative_paths_resolve_against_the_root() {
        let workspace = workspace();
        assert_eq!(
            workspace.resolve("src/main.rs").unwrap(),
            PathBuf::from("/work/project/src/main.rs")
        );
    }

    #[test]
    fn interior_parent_segments_are_folded_away() {
        let workspace = workspace();
        assert_eq!(
            workspace.resolve("src/../README.md").unwrap(),
            PathBuf::from("/work/project/README.md")
        );
    }

    #[test]
    fn escaping_the_workspace_is_refused() {
        let workspace = workspace();
        let error = workspace.resolve("../../etc/passwd").unwrap_err();
        assert!(error.contains("outside the workspace"), "{error}");

        let error = workspace.resolve("/etc/passwd").unwrap_err();
        assert!(error.contains("outside the workspace"), "{error}");
    }

    #[test]
    fn an_absolute_path_inside_the_root_is_accepted() {
        let workspace = workspace();
        assert_eq!(
            workspace.resolve("/work/project/src/lib.rs").unwrap(),
            PathBuf::from("/work/project/src/lib.rs")
        );
    }

    #[test]
    fn display_is_relative_to_the_root() {
        let workspace = workspace();
        assert_eq!(
            workspace.display(Path::new("/work/project/src/lib.rs")),
            "src/lib.rs"
        );
    }
}
