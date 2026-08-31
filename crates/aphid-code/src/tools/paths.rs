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
    read_only: Arc<Vec<PathBuf>>,
    read_write: Arc<Vec<PathBuf>>,
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
            read_only: Arc::new(Vec::new()),
            read_write: Arc::new(Vec::new()),
        }
    }

    /// A workspace with explicit additional host roots. These are intended for
    /// a sandbox policy; ordinary coding sessions use [`Workspace::new`].
    pub fn with_grants(
        root: impl Into<PathBuf>,
        read_only: Vec<PathBuf>,
        read_write: Vec<PathBuf>,
    ) -> Result<Self, String> {
        let workspace = Self::new(root);
        let read_only = canonical_roots(read_only)?;
        let read_write = canonical_roots(read_write)?;
        for left in &read_only {
            for right in &read_write {
                if overlaps(left, right) {
                    return Err(format!(
                        "workspace grants overlap: {} and {}",
                        left.display(),
                        right.display()
                    ));
                }
            }
        }
        Ok(Self {
            root: workspace.root,
            read_only: Arc::new(read_only),
            read_write: Arc::new(read_write),
        })
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
        self.resolve_read(path)
    }

    /// Resolve a path that is only read.
    pub fn resolve_read(&self, path: &str) -> Result<PathBuf, String> {
        self.resolve_access(path, false)
    }

    /// Resolve a path that will be changed.
    pub fn resolve_write(&self, path: &str) -> Result<PathBuf, String> {
        self.resolve_access(path, true)
    }

    fn resolve_access(&self, path: &str, write: bool) -> Result<PathBuf, String> {
        let raw = Path::new(path);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };

        let normalised = normalise(&joined);
        let allowed = if write {
            self.read_write
                .iter()
                .chain(std::iter::once(self.root.as_ref()))
                .any(|root| normalised.starts_with(root))
        } else {
            self.read_only
                .iter()
                .chain(self.read_write.iter())
                .chain(std::iter::once(self.root.as_ref()))
                .any(|root| normalised.starts_with(root))
        };
        if !allowed {
            return Err(format!(
                "`{path}` resolves to {} , which is outside the allowed workspace roots",
                normalised.display(),
            ));
        }
        // A lexical check alone permits `inside/link -> /outside`. Canonicalise
        // the target, or its nearest existing parent for a new file, and check
        // the resolved path one more time.
        let resolved = existing_parent(&normalised)?;
        let roots: Vec<&PathBuf> = if write {
            self.read_write
                .iter()
                .chain(std::iter::once(self.root.as_ref()))
                .collect()
        } else {
            self.read_only
                .iter()
                .chain(self.read_write.iter())
                .chain(std::iter::once(self.root.as_ref()))
                .collect()
        };
        if !roots
            .iter()
            .any(|root| resolved.starts_with(root.as_path()))
        {
            return Err(format!(
                "`{path}` follows a link outside the allowed workspace roots"
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

fn canonical_roots(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for path in paths {
        if !path.is_absolute() {
            return Err(format!(
                "workspace grant {} is not absolute",
                path.display()
            ));
        }
        let path = path.canonicalize().map_err(|error| {
            format!(
                "could not resolve workspace grant {}: {error}",
                path.display()
            )
        })?;
        if roots.iter().any(|root: &PathBuf| overlaps(root, &path)) {
            return Err(format!(
                "workspace grant {} overlaps another grant",
                path.display()
            ));
        }
        roots.push(path);
    }
    Ok(roots)
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn existing_parent(path: &Path) -> Result<PathBuf, String> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| format!("{} has no existing parent", path.display()))?;
    }
    let canonical = existing
        .canonicalize()
        .map_err(|error| format!("could not resolve {}: {error}", existing.display()))?;
    Ok(if existing == path {
        canonical
    } else {
        canonical.join(path.strip_prefix(existing).unwrap_or(Path::new("")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> Workspace {
        Workspace {
            root: Arc::new(PathBuf::from("/work/project")),
            read_only: Arc::new(Vec::new()),
            read_write: Arc::new(Vec::new()),
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
        assert!(
            error.contains("outside the allowed workspace roots"),
            "{error}"
        );

        let error = workspace.resolve("/etc/passwd").unwrap_err();
        assert!(
            error.contains("outside the allowed workspace roots"),
            "{error}"
        );
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
