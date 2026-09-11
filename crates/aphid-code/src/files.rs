//! The file index behind `@` in the terminal.
//!
//! A terminal that asks for a path a keystroke at a time cannot walk the tree
//! each time: a repository of any size would answer slower than the typist.
//! So the tree is read once into memory, followed by a watcher, and every
//! query is answered from that.
//!
//! None of that is written here. It is [`fff_search`], the engine behind
//! `fff`, and this module is the whole of what aphid knows about it: an
//! [`Index`] you open on a directory and ask for paths. Anything that changes
//! in that crate changes in this file and nowhere else.

use std::path::Path;

use fff_search::file_picker::FilePicker;
use fff_search::{
    FFFMode, FFFQuery, FilePickerOptions, FileSearchConfig, FuzzySearchOptions, PaginationArgs,
    SharedFilePicker, SharedFrecency,
};

/// A tree, read into memory and kept in step with the disk.
///
/// Opening one starts two background threads: the scan, and the watcher that
/// follows it. Both stop when the index is dropped.
pub struct Index {
    picker: SharedFilePicker,
}

impl Index {
    /// Read `root` into memory.
    ///
    /// Returns as soon as the background scan has been started, not when it
    /// has finished, so the first search may see a part of the tree. The
    /// watcher then keeps it current for the rest of the session.
    ///
    /// # Errors
    ///
    /// Fails when the scan cannot be started — `root` is not a directory the
    /// process may read, or it is one the engine refuses to index whole, such
    /// as the filesystem root or the home directory.
    pub fn open(root: &Path) -> Result<Self, fff_search::Error> {
        let picker = SharedFilePicker::default();
        FilePicker::new_with_shared_state(
            picker.clone(),
            // Frecency is an LMDB database of what was opened when. Aphid does
            // not keep one: it would be the first file this terminal writes
            // outside a session, and the ranking is good enough without it.
            SharedFrecency::default(),
            FilePickerOptions {
                base_path: root.to_string_lossy().into_owned(),
                // The mode an agent's file list is for, rather than an editor's:
                // it is what decides how a watcher event is taken and how a
                // match is scored.
                mode: FFFMode::Ai,
                ..FilePickerOptions::default()
            },
        )?;
        Ok(Self { picker })
    }

    /// The best `limit` paths for `query`, relative to the root.
    ///
    /// An empty query is not an error: it is what the list opens on, and it
    /// gives back the head of the index. Ordering is the engine's, best first,
    /// and it is kept — the caller must not sort it again.
    ///
    /// Takes the index's read lock, which the background scan holds for
    /// writing while it swaps in what it found, so this can wait. Call it off
    /// the interface's own thread. An answer of `None` means the scan has not
    /// put anything there yet, which is a reason to ask again and not a reason
    /// to say anything to the user.
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Option<Vec<String>> {
        let guard = self.picker.read().ok()?;
        let picker = guard.as_ref()?;

        let parsed = FFFQuery::parse(query, FileSearchConfig);
        let found = picker.fuzzy_search(
            &parsed,
            // The query tracker boosts a file you keep landing on. It is the
            // other LMDB database, and it is left out for the same reason.
            None,
            FuzzySearchOptions {
                // Nought means "as many threads as this machine has".
                max_threads: 0,
                pagination: PaginationArgs { offset: 0, limit },
                ..FuzzySearchOptions::default()
            },
        );

        Some(
            found
                .items
                .iter()
                .map(|item| item.relative_path(picker))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::Index;

    /// A small tree of known files, in a directory of its own.
    fn tree() -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aphid-files-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("deep").join("nest")).expect("create");
        std::fs::write(root.join("readme.md"), "hi").expect("write");
        std::fs::write(root.join("deep").join("middle.rs"), "hi").expect("write");
        std::fs::write(root.join("deep").join("nest").join("buried.rs"), "hi").expect("write");
        root
    }

    /// Ask until the background scan has caught up, or give up.
    ///
    /// `wait_for_scan` is the engine's own answer to this and it is not used:
    /// it blocks, and this is exactly the shape the terminal uses, which is to
    /// ask again on the next keystroke.
    fn eventually(index: &Index, query: &str) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(found) = index.search(query, 10)
                && !found.is_empty()
            {
                return found;
            }
            assert!(Instant::now() < deadline, "the scan never found `{query}`");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_buried_file_is_found_by_its_name() {
        let root = tree();
        let index = Index::open(&root).expect("open");
        let found = eventually(&index, "buried");
        assert_eq!(
            found.first().map(String::as_str),
            Some("deep/nest/buried.rs"),
            "the path comes back relative to the root: {found:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_query_gives_back_the_head_of_the_index() {
        let root = tree();
        let index = Index::open(&root).expect("open");
        let found = eventually(&index, "");
        assert_eq!(found.len(), 3, "every file, and no directories: {found:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
