//! The keys a colony holds.
//!
//! Two of them, and they are not the same kind of thing. `relay.key` is the
//! group **authority**: it signs what every group is, and a colony that loses it
//! cannot say so any more. `human.key` is a participant like any agent.
//!
//! A key file holds one line of hex and nothing else, and is created readable
//! by its owner alone. That is the same position the gateway socket takes in
//! [`aphid_alate`]: the file mode is the access control, so it has to be set
//! when the file is made and not afterwards.
//!
//! An agent's key is **not** here. An alate reads its own from an environment
//! variable named by its configuration, because a key that lives beside the
//! thing it authenticates is a key that travels with a backup.
//!
//! [`aphid_alate`]: https://docs.rs/aphid-alate

use std::path::{Path, PathBuf};

use aphid_nostr::nostr::key::Keys;

/// Why a key could not be read or made.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: this is not a secret key; delete it to make a new one")]
    Malformed { path: PathBuf },
}

/// Read the key at `path`, or make one and write it there.
///
/// # Errors
///
/// Fails when the file exists and is not a key, or when it cannot be read or
/// written.
pub fn open(path: &Path) -> Result<Keys, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(path, text.trim()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let keys = Keys::generate();
            write(path, &keys)?;
            Ok(keys)
        }
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Read a key from a string, in hex or as an `nsec`.
///
/// # Errors
///
/// Fails when the text is not a secret key.
pub fn parse(path: &Path, text: &str) -> Result<Keys, Error> {
    Keys::parse(text).map_err(|_| Error::Malformed {
        path: path.to_path_buf(),
    })
}

/// Write a key, readable by its owner alone.
///
/// # Errors
///
/// Fails when the directory cannot be created or the file cannot be written.
fn write(path: &Path, keys: &Keys) -> Result<(), Error> {
    let io = |source| Error::Io {
        path: path.to_path_buf(),
        source,
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }

    let mut text = keys.secret_key().to_secret_hex();
    text.push('\n');

    // The mode goes on at creation. A `write` followed by a `set_permissions`
    // leaves the key world-readable for as long as the two calls are apart,
    // which is long enough on a shared machine.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, &text).map_err(io)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_that_is_not_one_is_refused_by_name() {
        let path = Path::new("relay.key");
        let error = parse(path, "not a key").expect_err("this is not a key");
        assert!(error.to_string().contains("delete it"), "{error}");
    }
}
