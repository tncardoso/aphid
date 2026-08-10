//! Where plugins are found, and which one wins.

use std::path::{Path, PathBuf};

use aphid_plugin::{discover, explicit};

/// A scratch tree with a workspace and a home directory, removed on drop.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let root = std::env::temp_dir().join(format!(
            "aphid-discovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create the tree");
        Self { root }
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Write a plugin at `<base>/.aphid/plugins/<relative>`.
    fn plugin(&self, base: &Path, relative: &str, source: &str) {
        let path = base.join(".aphid").join("plugins").join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the directory");
        std::fs::write(path, source).expect("write the plugin");
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn both_layouts_are_found_and_sorted() {
    let tree = Tree::new();
    tree.plugin(&tree.workspace(), "loose.rhai", "//! A loose file.\n");
    tree.plugin(&tree.workspace(), "bundled/main.rhai", "//! A directory.\n");
    tree.plugin(&tree.workspace(), "notes.txt", "ignored");

    let (found, problems) = discover(&tree.workspace(), None);

    assert!(problems.is_empty(), "{problems:?}");
    let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["bundled", "loose"]);
    assert_eq!(found[0].description.as_deref(), Some("A directory."));
    assert!(
        found.iter().all(|p| p.project),
        "both came from the project"
    );
}

#[test]
fn a_project_plugin_shadows_a_global_one() {
    let tree = Tree::new();
    tree.plugin(&tree.workspace(), "guard.rhai", "//! The project one.\n");
    tree.plugin(&tree.home(), "guard.rhai", "//! The global one.\n");
    tree.plugin(&tree.home(), "only-global.rhai", "//! Just mine.\n");

    let (found, _) = discover(&tree.workspace(), Some(&tree.home()));

    let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["guard", "only-global"]);

    let guard = &found[0];
    assert_eq!(guard.description.as_deref(), Some("The project one."));
    assert!(guard.project);

    let global = &found[1];
    assert!(!global.project, "a home plugin needs no trust decision");
}

#[test]
fn a_missing_directory_is_not_a_problem() {
    let tree = Tree::new();
    let (found, problems) = discover(&tree.workspace(), Some(&tree.home()));

    assert!(found.is_empty());
    assert!(
        problems.is_empty(),
        "an absent directory is the normal case"
    );
}

#[test]
fn an_explicit_path_is_never_a_project_plugin() {
    let tree = Tree::new();
    tree.plugin(
        &tree.workspace(),
        "one-off.rhai",
        "//! Passed on the command line.\n",
    );
    let path = tree
        .workspace()
        .join(".aphid")
        .join("plugins")
        .join("one-off.rhai");

    let file = explicit(&path).expect("the file is readable");

    assert_eq!(file.name, "one-off");
    assert!(!file.project, "an explicit path is a deliberate act");
}

#[test]
fn an_explicit_path_that_is_not_there_is_reported() {
    let tree = Tree::new();
    let problem = explicit(&tree.root.join("nope.rhai")).expect_err("no such file");
    assert!(problem.message.contains("could not read"), "{problem:?}");
}

#[test]
fn the_shipped_examples_all_load() {
    use std::sync::Arc;

    use aphid_plugin::{Capabilities, PluginHost};

    let examples = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("plugins");

    let mut files = Vec::new();
    for entry in std::fs::read_dir(&examples).expect("the examples are there") {
        let path = entry.expect("an entry").path();
        files.push(aphid_plugin::explicit(&path).expect("readable"));
    }
    assert!(files.len() >= 5, "found {} examples", files.len());

    let caps = Capabilities::full(&examples);
    let (host, diagnostics) = PluginHost::load(&files, &caps, Arc::new(aphid_plugin::Silent));

    assert!(
        diagnostics.is_empty(),
        "an example does not load: {diagnostics:?}"
    );
    assert_eq!(host.plugins().len(), files.len());
    assert!(
        host.plugins().iter().all(|p| !p.hooks().is_empty()),
        "every example defines at least one hook"
    );
}
