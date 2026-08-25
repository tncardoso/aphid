//! Finding plugin files on disk.
//!
//! Layout, searched in the workspace and then under `~/.aphid`, matching how
//! [skills] are laid out:
//!
//! ```text
//! .aphid/plugins/<name>.rhai
//! .aphid/plugins/<name>/main.rhai
//! ```
//!
//! A project plugin shadows a global one of the same name, so a repository can
//! override a personal plugin without the user editing either.
//!
//! [skills]: https://github.com/tncardoso/aphid

use std::path::{Path, PathBuf};

/// The directory, under `.aphid`, that plugins live in.
pub const DIR_NAME: &str = "plugins";
/// The file a plugin directory must contain.
pub const ENTRY_FILE: &str = "main.rhai";
/// The extension a loose plugin file must have.
pub const EXTENSION: &str = "rhai";

/// A plugin file that was found but not yet loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginFile {
    pub name: String,
    pub path: PathBuf,
    /// The leading `//!` comment block, if the file opens with one.
    pub description: Option<String>,
    /// Whether this came from the workspace rather than the home directory.
    /// Project plugins are the ones that need a trust decision.
    pub project: bool,
}

/// A plugin file that could not be used, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub message: String,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plugin {}: {}", self.path.display(), self.message)
    }
}

/// Find every plugin available to a workspace.
///
/// `home` is where global plugins live; pass `None` to skip them. Both roots are
/// parameters rather than environment lookups so this is testable without
/// mutating process-wide state.
#[must_use]
pub fn discover(root: &Path, home: Option<&Path>) -> (Vec<PluginFile>, Vec<Diagnostic>) {
    let mut roots = vec![(root.join(".aphid").join(DIR_NAME), true)];
    if let Some(home) = home {
        roots.push((home.join(".aphid").join(DIR_NAME), false));
    }

    let mut plugins: Vec<PluginFile> = Vec::new();
    let mut diagnostics = Vec::new();

    for (dir, project) in roots {
        let (found, problems) = load_dir(&dir, project);
        for plugin in found {
            if !plugins.iter().any(|existing| existing.name == plugin.name) {
                plugins.push(plugin);
            }
        }
        diagnostics.extend(problems);
    }

    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    (plugins, diagnostics)
}

/// Describe one explicitly named file, for `--plugin <path>`.
///
/// Bypasses the layout rules: an explicit path is a deliberate act, so it is
/// never treated as a project plugin and never needs a trust decision.
///
/// # Errors
///
/// Fails when the path is not a readable file.
pub fn explicit(path: &Path) -> Result<PluginFile, Diagnostic> {
    let path = if path.is_dir() {
        path.join(ENTRY_FILE)
    } else {
        path.to_path_buf()
    };

    let text = std::fs::read_to_string(&path).map_err(|error| Diagnostic {
        path: path.clone(),
        message: format!("could not read: {error}"),
    })?;

    Ok(PluginFile {
        name: name_of(&path).unwrap_or_else(|| path.display().to_string()),
        description: description(&text),
        path,
        project: false,
    })
}

fn load_dir(dir: &Path, project: bool) -> (Vec<PluginFile>, Vec<Diagnostic>) {
    let mut plugins = Vec::new();
    let mut diagnostics = Vec::new();

    let Ok(entries) = std::fs::read_dir(dir) else {
        // A missing plugins directory is the normal case, not a problem.
        return (plugins, diagnostics);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let candidate = if path.is_dir() {
            path.join(ENTRY_FILE)
        } else if path.extension().is_some_and(|ext| ext == EXTENSION) {
            path.clone()
        } else {
            continue;
        };

        if !candidate.is_file() {
            continue;
        }

        match std::fs::read_to_string(&candidate) {
            Ok(text) => match name_of(&candidate) {
                Some(name) => plugins.push(PluginFile {
                    name,
                    description: description(&text),
                    path: candidate,
                    project,
                }),
                None => diagnostics.push(Diagnostic {
                    path: candidate,
                    message: "could not determine a name".to_owned(),
                }),
            },
            Err(error) => diagnostics.push(Diagnostic {
                path: candidate,
                message: format!("could not read: {error}"),
            }),
        }
    }

    (plugins, diagnostics)
}

/// The file stem, or the directory name for a `main.rhai`.
fn name_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy().to_string();
    if stem == "main" {
        return path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().to_string());
    }
    Some(stem)
}

/// Read a leading `//!` comment block as the plugin's description.
///
/// Cheaper than a manifest file and impossible to get out of step with the code
/// it describes, which is the same bargain skills make with their frontmatter.
fn description(text: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() && lines.is_empty() {
            continue;
        }
        match line.strip_prefix("//!") {
            Some(rest) => lines.push(rest.trim().to_owned()),
            None => break,
        }
    }

    let joined = lines.join(" ").trim().to_owned();
    (!joined.is_empty()).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leading_comment_block_becomes_the_description() {
        let text = "//! Blocks bad commands.\n//! And says so.\n\nfn on_tool_call(c) {}\n";
        assert_eq!(
            description(text).as_deref(),
            Some("Blocks bad commands. And says so.")
        );
    }

    #[test]
    fn a_file_without_a_comment_block_has_no_description() {
        assert_eq!(description("fn on_tool_call(c) {}\n"), None);
        assert_eq!(description("// an ordinary comment\n"), None);
    }

    #[test]
    fn a_directory_plugin_is_named_for_its_directory() {
        assert_eq!(
            name_of(Path::new("/x/.aphid/plugins/guard/main.rhai")).as_deref(),
            Some("guard")
        );
        assert_eq!(
            name_of(Path::new("/x/.aphid/plugins/guard.rhai")).as_deref(),
            Some("guard")
        );
    }
}
