//! Where one alate lives.
//!
//! Each instance has a name and a directory of its own:
//!
//! ```text
//! ~/.aphid/alate/<name>/
//!   alate.json      the configuration
//!   AGENTS.md       the standing instructions for this alate
//!   HEARTBEAT.md    what to say when it wakes itself, if anything
//!   memory/         the memory: one markdown file for each path
//!   cron.json       the jobs it has scheduled for itself
//!   state.json      when the heartbeat last woke
//!   gateway.sock    the socket clients attach to
//!   alate.log       every frame the gateway sent, for the hours nobody watched
//!   .aphid/         skills, plugins and sessions, found the ordinary way
//! ```
//!
//! The last line is the reason for the whole shape. The home *is* the agent's
//! workspace, so [`skills::discover`], [`aphid_plugin::discover`] and
//! [`sessions_dir`] find their directories with no new code and no second
//! convention to learn.
//!
//! Unlike [`aphid_code::home_dir`], this honours `$APHID_HOME`. That function
//! ignores it deliberately, because moving `AGENTS.md` and skills now would
//! orphan setups that already exist; an alate has nothing yet to orphan, and
//! reading the variable is what lets a test keep its whole world in a temporary
//! directory.
//!
//! [`skills::discover`]: aphid_code::skills::discover
//! [`sessions_dir`]: aphid_code::session::sessions_dir

use std::path::{Path, PathBuf};

/// The directory, under `~/.aphid`, that holds every instance.
pub const DIR_NAME: &str = "alate";

/// The instance used when nobody names one.
pub const DEFAULT_NAME: &str = "default";

/// The longest name an instance may have. Long enough for a sentence nobody
/// wanted, short enough to stay inside a socket path on every platform.
pub const MAX_NAME: usize = 64;

/// Why a home could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("there is no home directory to keep an alate in")]
    NoHome,
    #[error("{name:?} cannot name an alate: {reason}")]
    Name { name: String, reason: &'static str },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

/// One instance's directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    name: String,
    root: PathBuf,
}

impl Home {
    /// The directory every instance lives under: `$APHID_HOME/alate`, or
    /// `~/.aphid/alate`.
    ///
    /// # Errors
    ///
    /// Fails when there is no home directory, which a container can manage.
    pub fn root_dir() -> Result<PathBuf, Error> {
        aphid_core::catalog::aphid_dir()
            .map(|dir| dir.join(DIR_NAME))
            .ok_or(Error::NoHome)
    }

    /// Open the instance called `name`, creating what is not there yet.
    ///
    /// # Errors
    ///
    /// Fails when the name is not one an instance may have, when there is no
    /// home directory, or when a directory cannot be created.
    pub fn open(name: &str) -> Result<Self, Error> {
        Self::open_in(&Self::root_dir()?, name)
    }

    /// Open an instance under an explicit root.
    ///
    /// The root is a parameter and not an environment lookup, so a test keeps
    /// its instances in a temporary directory without moving `$HOME`.
    ///
    /// # Errors
    ///
    /// Fails when the name is not one an instance may have, or when a directory
    /// cannot be created.
    pub fn open_in(root: &Path, name: &str) -> Result<Self, Error> {
        check_name(name)?;
        let home = Self {
            name: name.to_owned(),
            root: root.join(name),
        };
        home.ensure()?;
        Ok(home)
    }

    /// Every instance that exists, by name, in name order.
    ///
    /// A missing root is no instances rather than a failure: nobody has made
    /// one yet.
    ///
    /// # Errors
    ///
    /// Fails when there is no home directory, or when the root cannot be read.
    pub fn list() -> Result<Vec<String>, Error> {
        Self::list_in(&Self::root_dir()?)
    }

    /// Every instance under an explicit root.
    ///
    /// # Errors
    ///
    /// Fails when the root exists but cannot be read.
    pub fn list_in(root: &Path) -> Result<Vec<String>, Error> {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(Error::io(root, source)),
        };

        let mut names = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // A directory somebody made by hand under a name an alate cannot
            // have is not an instance, and listing it would only offer a name
            // that `open` then refuses.
            if check_name(&name).is_ok() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Create the directories an instance needs.
    ///
    /// `sessions` is left out on purpose: [`SessionStore::create`] makes it, and
    /// an instance that has never run should not look as though it has.
    ///
    /// [`SessionStore::create`]: aphid_code::session::store::SessionStore::create
    ///
    /// # Errors
    ///
    /// Fails when a directory cannot be created.
    pub fn ensure(&self) -> Result<(), Error> {
        for dir in [
            self.root.clone(),
            self.memory_dir(),
            self.aphid_dir().join("skills"),
            self.aphid_dir().join("plugins"),
        ] {
            std::fs::create_dir_all(&dir).map_err(|source| Error::io(&dir, source))?;
        }
        Ok(())
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where skills, plugins and sessions live, exactly as in any workspace.
    #[must_use]
    pub fn aphid_dir(&self) -> PathBuf {
        self.root.join(".aphid")
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.root.join("alate.json")
    }

    /// The instructions this alate always carries, found by
    /// [`aphid_code::context::discover`] because the home is the workspace root.
    #[must_use]
    pub fn instructions_file(&self) -> PathBuf {
        self.root.join(aphid_code::context::FILE_NAME)
    }

    #[must_use]
    pub fn heartbeat_file(&self) -> PathBuf {
        self.root.join("HEARTBEAT.md")
    }

    #[must_use]
    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }

    #[must_use]
    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    /// The jobs this alate has scheduled for itself.
    #[must_use]
    pub fn cron_file(&self) -> PathBuf {
        self.root.join("cron.json")
    }

    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.root.join("gateway.sock")
    }

    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.root.join("alate.log")
    }
}

/// Whether `name` may name an instance.
///
/// The rules exist so a name cannot reach outside the root: no separator is in
/// the allowed set, and a leading dot is refused, which takes `.` and `..` with
/// it. The same rules apply to each segment of a memory path, for the same
/// reason.
///
/// # Errors
///
/// Fails with the reason the name was refused.
pub fn check_name(name: &str) -> Result<(), Error> {
    let refuse = |reason| {
        Err(Error::Name {
            name: name.to_owned(),
            reason,
        })
    };

    if name.is_empty() {
        return refuse("it is empty");
    }
    if name.len() > MAX_NAME {
        return refuse("it is longer than 64 characters");
    }
    if name.starts_with('.') {
        return refuse("it starts with a dot");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return refuse("it holds something other than letters, digits, dot, dash and underscore");
    }
    Ok(())
}
