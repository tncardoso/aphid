//! A temporary directory that cleans up after itself.
//!
//! Copied in spirit from `aphid-code`'s tests: this workspace has no `tempfile`
//! dependency, and one struct with a `Drop` is less than one more crate.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Temp {
    pub root: PathBuf,
}

// Each test binary compiles this module for itself, and none of them uses all
// of it.
#[allow(dead_code)]
impl Temp {
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // `/tmp` and not `std::env::temp_dir()`: on macOS `$TMPDIR` is a long
        // per-process path under `/private/var/folders/...`, which alone can
        // exceed the socket path a Unix domain socket allows. `/tmp` is short
        // on every platform this crate runs on, which is Unix-only already —
        // it binds a `UnixListener`.
        let root = PathBuf::from("/tmp").join(format!(
            "aphid-alate-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("temp dir");
        Self {
            root: root.canonicalize().expect("canonical"),
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path(name);
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
