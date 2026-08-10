//! Project context and skill discovery, against real temp directories.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use aphid_code::{Workspace, context, skills};

struct Temp {
    root: PathBuf,
}

impl Temp {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aphid-discovery-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("temp dir");
        Self {
            root: root.canonicalize().expect("canonical"),
        }
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(&path, contents).expect("write");
        path
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn context_files_are_collected_outermost_first() {
    let temp = Temp::new();
    temp.write("AGENTS.md", "root rules");
    temp.write("crates/thing/AGENTS.md", "crate rules");
    let workspace = Workspace::new(&temp.root);

    let files = context::discover(&workspace, &temp.root.join("crates/thing"), None);

    let contents: Vec<&str> = files.iter().map(|file| file.content.as_str()).collect();
    // The most specific file is last, so it has the final word.
    assert_eq!(contents, vec!["root rules", "crate rules"]);
}

#[test]
fn a_global_context_file_comes_before_the_project() {
    let temp = Temp::new();
    temp.write("AGENTS.md", "project rules");
    temp.write("home/.aphid/AGENTS.md", "global rules");
    let workspace = Workspace::new(&temp.root);

    let files = context::discover(&workspace, &temp.root, Some(&temp.root.join("home")));

    let contents: Vec<&str> = files.iter().map(|file| file.content.as_str()).collect();
    assert_eq!(contents, vec!["global rules", "project rules"]);
}

#[test]
fn empty_and_missing_context_files_are_skipped() {
    let temp = Temp::new();
    temp.write("AGENTS.md", "   \n\n");
    let workspace = Workspace::new(&temp.root);

    assert!(context::discover(&workspace, &temp.root, None).is_empty());
}

#[test]
fn the_walk_stops_at_the_workspace_root() {
    let temp = Temp::new();
    // Above the workspace root, so it must not be picked up.
    temp.write("AGENTS.md", "outside");
    temp.write("project/AGENTS.md", "inside");
    let workspace = Workspace::new(temp.root.join("project"));

    let files = context::discover(&workspace, &temp.root.join("project"), None);

    let contents: Vec<&str> = files.iter().map(|file| file.content.as_str()).collect();
    assert_eq!(contents, vec!["inside"]);
}

#[test]
fn skills_load_from_both_layouts() {
    let temp = Temp::new();
    temp.write(
        ".aphid/skills/release/SKILL.md",
        "---\ndescription: How to cut a release\n---\n# Release\n",
    );
    temp.write(
        ".aphid/skills/review.md",
        "---\nname: code-review\ndescription: How we review\n---\nbody\n",
    );
    let workspace = Workspace::new(&temp.root);

    let (skills, diagnostics) = skills::discover(&workspace, None);

    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    // Sorted by name; `release` took its name from its directory, `review.md`
    // from its frontmatter.
    assert_eq!(names, vec!["code-review", "release"]);
    assert_eq!(skills[1].description, "How to cut a release");
    assert!(skills[1].path.ends_with("release/SKILL.md"));
}

#[test]
fn a_skill_without_a_description_is_reported_not_silently_dropped() {
    let temp = Temp::new();
    temp.write(".aphid/skills/broken.md", "---\nname: broken\n---\nbody\n");
    temp.write(".aphid/skills/fine.md", "---\ndescription: ok\n---\nbody\n");
    let workspace = Workspace::new(&temp.root);

    let (skills, diagnostics) = skills::discover(&workspace, None);

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "fine");
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].path.ends_with("broken.md"));
    assert!(diagnostics[0].message.contains("description"));
}

#[test]
fn a_project_skill_shadows_a_global_one() {
    let temp = Temp::new();
    temp.write(
        ".aphid/skills/release.md",
        "---\ndescription: project version\n---\n",
    );
    temp.write(
        "home/.aphid/skills/release.md",
        "---\ndescription: global version\n---\n",
    );
    let workspace = Workspace::new(&temp.root);

    let (skills, _) = skills::discover(&workspace, Some(&temp.root.join("home")));

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].description, "project version");
}

#[test]
fn no_skills_directory_is_not_a_problem() {
    let temp = Temp::new();
    let workspace = Workspace::new(&temp.root);

    let (skills, diagnostics) = skills::discover(&workspace, None);

    assert!(skills.is_empty());
    assert!(diagnostics.is_empty());
}
