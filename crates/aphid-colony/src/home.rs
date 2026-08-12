//! Where one colony lives.
//!
//! Each hub has a name and a directory of its own:
//!
//! ```text
//! ~/.aphid/colony/<name>/
//!   colony.json     the configuration
//!   relay.key       the relay's own secret key, readable by nobody else
//!   human.key       the secret key the terminal talks with
//!   colony.db       every event, in SQLite
//! ```
//!
//! Unlike an alate's home this is **not** an agent workspace. There is no
//! `AGENTS.md`, no skills and no plugins, because a colony runs no agent: it is
//! a place agents talk, and everything in it is either a key or a log.
//!
//! It honours `$APHID_HOME` for the reason [`aphid_alate::home`] gives — it is
//! what lets a test keep its whole world in a temporary directory.
//!
//! [`aphid_alate::home`]: https://docs.rs/aphid-alate

use std::path::{Path, PathBuf};

/// The directory, under `~/.aphid`, that holds every hub.
pub const DIR_NAME: &str = "colony";

/// The hub used when nobody names one.
pub const DEFAULT_NAME: &str = "default";

/// The longest name a hub may have.
pub const MAX_NAME: usize = 64;

/// Why a home could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("there is no home directory to keep a colony in")]
    NoHome,
    #[error("{name:?} cannot name a colony: {reason}")]
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
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// One hub's directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    name: String,
    root: PathBuf,
}

impl Home {
    /// The directory every hub lives under: `$APHID_HOME/colony`, or
    /// `~/.aphid/colony`.
    ///
    /// # Errors
    ///
    /// Fails when there is no home directory, which a container can manage.
    pub fn root_dir() -> Result<PathBuf, Error> {
        aphid_core::catalog::aphid_dir()
            .map(|dir| dir.join(DIR_NAME))
            .ok_or(Error::NoHome)
    }

    /// Open the hub called `name`, creating what is not there yet.
    ///
    /// # Errors
    ///
    /// Fails when the name is not one a hub may have, when there is no home
    /// directory, or when the directory cannot be created.
    pub fn open(name: &str) -> Result<Self, Error> {
        Self::open_in(&Self::root_dir()?, name)
    }

    /// Open a hub under an explicit root.
    ///
    /// The root is a parameter and not an environment lookup, so a test keeps
    /// its hubs in a temporary directory without moving `$HOME`.
    ///
    /// # Errors
    ///
    /// Fails when the name is not one a hub may have, or when the directory
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

    /// Every hub that exists, by name, in name order.
    ///
    /// # Errors
    ///
    /// Fails when there is no home directory, or when the root cannot be read.
    pub fn list() -> Result<Vec<String>, Error> {
        Self::list_in(&Self::root_dir()?)
    }

    /// Every hub under an explicit root.
    ///
    /// A missing root is no hubs rather than a failure: nobody has made one yet.
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
            // A directory made by hand under a name a colony cannot have is not
            // a hub, and listing it would only offer a name `open` then refuses.
            if check_name(&name).is_ok() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Create the directory a hub needs.
    ///
    /// One directory: the database, the keys and the configuration are all files
    /// in it. Nothing here is made until something writes.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created.
    pub fn ensure(&self) -> Result<(), Error> {
        std::fs::create_dir_all(&self.root).map_err(|source| Error::io(&self.root, source))
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.root.join("colony.json")
    }

    /// The key the relay signs group metadata with. It is the group authority,
    /// and this file is what makes it the same authority after a restart.
    #[must_use]
    pub fn relay_key(&self) -> PathBuf {
        self.root.join("relay.key")
    }

    /// The key the person at the terminal talks with.
    #[must_use]
    pub fn human_key(&self) -> PathBuf {
        self.root.join("human.key")
    }

    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.root.join("colony.db")
    }
}

/// Whether `name` may name a hub.
///
/// The rules exist so a name cannot reach outside the root: no separator is in
/// the allowed set, and a leading dot is refused, which takes `.` and `..` with
/// it.
///
/// This is a copy of `aphid_alate::home::check_name`, and deliberately so. The
/// alate bridge depends on this crate, so this crate cannot depend on the alate,
/// and lifting forty lines into `aphid-core` for two callers would buy less than
/// it costs. If a third caller appears, lift it there.
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
