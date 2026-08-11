//! Skills: instruction files the model opens on demand.
//!
//! Only each skill's name, description and path go into the system prompt. The
//! model reads the body with the `read` tool when a task matches — pi's
//! progressive disclosure, which keeps a dozen skills from costing a dozen
//! skills' worth of context on every request.
//!
//! Layout, searched in the workspace and then under `~/.aphid`:
//!
//! ```text
//! .aphid/skills/<name>/SKILL.md
//! .aphid/skills/<name>.md
//! ```

use std::path::{Path, PathBuf};

use crate::tools::Workspace;

/// The longest description that will be accepted into the prompt.
const MAX_DESCRIPTION: usize = 1024;

/// A skill the model can choose to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// Whether this came from the workspace rather than the home directory.
    pub project: bool,
}

/// A skill file that could not be used, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub message: String,
}

/// Find every skill available to a workspace.
///
/// `home` is where global skills live; pass `None` to skip them. It is a
/// parameter rather than an environment lookup so this is testable without
/// mutating process-wide state.
///
/// Project skills come first and shadow a global skill of the same name.
#[must_use]
pub fn discover(workspace: &Workspace, home: Option<&Path>) -> (Vec<Skill>, Vec<Diagnostic>) {
    let mut roots = vec![(workspace.root().join(".aphid").join("skills"), true)];
    if let Some(home) = home {
        roots.push((home.join(".aphid").join("skills"), false));
    }

    let mut skills: Vec<Skill> = Vec::new();
    let mut diagnostics = Vec::new();

    for (root, project) in roots {
        let (found, problems) = load_dir(&root, project);
        for skill in found {
            if !skills.iter().any(|existing| existing.name == skill.name) {
                skills.push(skill);
            }
        }
        diagnostics.extend(problems);
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    (skills, diagnostics)
}

fn load_dir(root: &Path, project: bool) -> (Vec<Skill>, Vec<Diagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();

    let Ok(entries) = std::fs::read_dir(root) else {
        // A missing skills directory is the normal case, not a problem.
        return (skills, diagnostics);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let candidate = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.extension().is_some_and(|ext| ext == "md") {
            path.clone()
        } else {
            continue;
        };

        if !candidate.is_file() {
            continue;
        }

        match load(&candidate, project) {
            Ok(skill) => skills.push(skill),
            Err(message) => diagnostics.push(Diagnostic {
                path: candidate,
                message,
            }),
        }
    }

    (skills, diagnostics)
}

fn load(path: &Path, project: bool) -> Result<Skill, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("could not read: {error}"))?;
    let front = frontmatter(&text);

    let description = front
        .get("description")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "no `description` in the frontmatter".to_owned())?;

    if description.len() > MAX_DESCRIPTION {
        return Err(format!(
            "description is {} characters, over the {MAX_DESCRIPTION} limit",
            description.len()
        ));
    }

    // A name in the frontmatter wins; otherwise the directory name for a
    // SKILL.md, or the file stem for a loose `.md`.
    let name = front
        .get("name")
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .or_else(|| fallback_name(path))
        .ok_or_else(|| "could not determine a name".to_owned())?;

    Ok(Skill {
        name,
        description: description.clone(),
        path: path.to_path_buf(),
        project,
    })
}

fn fallback_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy().to_string();
    if stem == "SKILL" {
        return path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().to_string());
    }
    Some(stem)
}

/// Parse a leading `---` block as flat `key: value` pairs.
///
/// Deliberately not a YAML parser: two known scalar keys do not justify the
/// dependency. Anything nested is ignored rather than rejected, so a richer
/// frontmatter still yields its name and description.
fn frontmatter(text: &str) -> std::collections::HashMap<String, String> {
    let mut fields = std::collections::HashMap::new();

    let Some(rest) = text.strip_prefix("---") else {
        return fields;
    };
    let rest = rest.trim_start_matches(['\r', '\n']);
    let Some(end) = rest.find("\n---") else {
        return fields;
    };

    for line in rest[..end].lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // Nested keys announce themselves with an empty value; skip them rather
        // than recording a key with nothing in it.
        if value.is_empty() {
            continue;
        }
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value);
        fields.insert(key.trim().to_owned(), value.to_owned());
    }

    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_reads_quoted_and_bare_values() {
        let fields = frontmatter("---\nname: thing\ndescription: \"does a thing\"\n---\n# Body\n");
        assert_eq!(fields.get("name").unwrap(), "thing");
        assert_eq!(fields.get("description").unwrap(), "does a thing");
    }

    #[test]
    fn a_document_without_frontmatter_yields_nothing() {
        assert!(frontmatter("# Just a heading\n").is_empty());
        assert!(frontmatter("---\nname: unterminated\n").is_empty());
    }

    #[test]
    fn nested_keys_are_skipped_rather_than_recorded_empty() {
        let fields = frontmatter("---\nname: thing\nmeta:\n  a: 1\ndescription: d\n---\n");
        assert!(!fields.contains_key("meta"));
        assert_eq!(fields.get("description").unwrap(), "d");
    }
}
