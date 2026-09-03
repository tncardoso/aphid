//! The model: the files on the disk, and the 670 MB in memory.
//!
//! Two lifetimes are kept apart here. The **files** are fetched once and stay
//! where a machine keeps such things, because a model is an artifact in the
//! manner of the binary and not the state of one instance. The **memory** is
//! read from those files at the first recording and dropped again when nothing
//! has asked for a while: 670 MB is a great deal to hold in a daemon that stays
//! up for weeks, and reading it again costs seconds.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;

/// Where the files come from.
const SOURCE: &str = "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main";

/// The files a Parakeet directory is, with the size and checksum each must
/// have.
///
/// The checksums are written here rather than asked of the repository, and
/// that is the point of them: a constant also pins the version. On the day the
/// files behind that address change, this build says so instead of quietly
/// reading something else.
const FILES: [File; 4] = [
    File {
        name: "encoder-model.int8.onnx",
        bytes: 652_183_999,
        sha256: "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09",
    },
    File {
        name: "decoder_joint-model.int8.onnx",
        bytes: 18_202_004,
        sha256: "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70",
    },
    File {
        name: "nemo128.onnx",
        bytes: 139_764,
        sha256: "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
    },
    File {
        name: "vocab.txt",
        bytes: 93_939,
        sha256: "",
    },
];

/// One file of the model.
struct File {
    name: &'static str,
    bytes: u64,
    /// The checksum, and an empty string for a file the repository stores
    /// whole rather than through LFS and gives no checksum for.
    sha256: &'static str,
}

/// How far a fetch has got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
}

impl Progress {
    /// How far along, in whole percent.
    #[must_use]
    pub fn percent(self) -> u64 {
        if self.total == 0 {
            return 0;
        }
        self.done.saturating_mul(100) / self.total
    }
}

/// What the model is doing, seen from anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// The files are not there and nothing is getting them.
    Missing(String),
    /// The files are on their way.
    Fetching(Progress),
    /// The files are there. Whether they are also in memory is not the caller's
    /// business.
    Ready,
}

/// The model's files and the state of getting them.
#[derive(Clone)]
pub struct Files {
    directory: PathBuf,
    state: Arc<Mutex<State>>,
}

impl Files {
    /// A model expected in `directory`.
    #[must_use]
    pub fn new(directory: PathBuf) -> Self {
        let state = if complete(&directory) {
            State::Ready
        } else {
            State::Missing("the transcription model is not on this machine".to_owned())
        };
        Self {
            directory,
            state: Arc::new(Mutex::new(state)),
        }
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// What the model is doing.
    #[must_use]
    pub fn state(&self) -> State {
        self.state
            .lock()
            .map_or(State::Ready, |state| state.clone())
    }

    /// A sentence saying why a recording cannot be transcribed yet, and `None`
    /// when it can.
    #[must_use]
    pub fn why_not(&self) -> Option<String> {
        match self.state() {
            State::Ready => None,
            State::Fetching(progress) => Some(format!(
                "the transcription model is still coming down, {}% of {} MB. \
                 Send the recording again in a few minutes.",
                progress.percent(),
                progress.total / 1_000_000
            )),
            State::Missing(why) => Some(why),
        }
    }

    /// How many bytes the whole model is.
    #[must_use]
    pub fn size() -> u64 {
        FILES.iter().map(|file| file.bytes).sum()
    }

    /// Get whatever of the model is not on the disk.
    ///
    /// Each file goes to `<name>.part`, is measured, and is only then given its
    /// name. A daemon killed in the middle of this leaves no half a model that
    /// loads and then fails.
    ///
    /// # Errors
    ///
    /// Fails with a sentence when the directory cannot be made, when the
    /// download cannot be made, or when a file arrives wrong.
    pub async fn fetch(&self) -> Result<(), String> {
        if matches!(self.state(), State::Ready) {
            return Ok(());
        }

        let total = Self::size();
        self.set(State::Fetching(Progress { done: 0, total }));

        if let Err(error) = tokio::fs::create_dir_all(&self.directory).await {
            let why = format!("{} could not be made: {error}", self.directory.display());
            self.set(State::Missing(why.clone()));
            return Err(why);
        }

        let client = reqwest::Client::new();
        let mut done = 0;

        for file in &FILES {
            let path = self.directory.join(file.name);
            if measures_up(&path, file).await {
                done += file.bytes;
                self.set(State::Fetching(Progress { done, total }));
                continue;
            }

            if let Err(why) = self.one(&client, file, &path, &mut done, total).await {
                self.set(State::Missing(why.clone()));
                return Err(why);
            }
        }

        self.set(State::Ready);
        Ok(())
    }

    /// One file, to `.part` and then into place.
    async fn one(
        &self,
        client: &reqwest::Client,
        file: &File,
        path: &Path,
        done: &mut u64,
        total: u64,
    ) -> Result<(), String> {
        use futures_util::StreamExt;
        use sha2::{Digest, Sha256};

        // Added to the name and not put in place of the extension: the model
        // has files named `x.int8.onnx`, and replacing the extension would
        // give `x.int8.part` — which is a name, but not one that says what it
        // is half of.
        let part = path.with_file_name(format!("{}.part", file.name));
        let response = client
            .get(format!("{SOURCE}/{}", file.name))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| format!("{} could not be fetched: {error}", file.name))?;

        let mut writing = tokio::fs::File::create(&part)
            .await
            .map_err(|error| format!("{} could not be written: {error}", part.display()))?;
        let mut digest = Sha256::new();
        let mut stream = response.bytes_stream();
        let start = *done;
        let mut written = 0u64;

        while let Some(piece) = stream.next().await {
            let piece = piece.map_err(|error| format!("{} stopped coming: {error}", file.name))?;
            digest.update(&piece);
            writing
                .write_all(&piece)
                .await
                .map_err(|error| format!("{} could not be written: {error}", part.display()))?;
            written = written.saturating_add(u64::try_from(piece.len()).unwrap_or(0));
            *done = start.saturating_add(written);
            self.set(State::Fetching(Progress { done: *done, total }));
        }
        writing
            .flush()
            .await
            .map_err(|error| format!("{} could not be written: {error}", part.display()))?;
        drop(writing);

        if !file.sha256.is_empty() {
            let got = hex(&digest.finalize());
            if got != file.sha256 {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(format!(
                    "{} came down wrong: expected {}, got {got}. \
                     Either the download was damaged or the model behind that \
                     address has changed.",
                    file.name, file.sha256
                ));
            }
        }

        tokio::fs::rename(&part, path)
            .await
            .map_err(|error| format!("{} could not be put in place: {error}", file.name))?;
        *done = start.saturating_add(file.bytes);
        self.set(State::Fetching(Progress { done: *done, total }));
        Ok(())
    }

    fn set(&self, state: State) {
        if let Ok(mut held) = self.state.lock() {
            *held = state;
        }
    }
}

/// Whether every file of the model is in `directory` with the right size.
///
/// A `.part` beside them counts for nothing: a file has its name only once it
/// is whole.
fn complete(directory: &Path) -> bool {
    FILES.iter().all(|file| {
        std::fs::metadata(directory.join(file.name)).is_ok_and(|found| found.len() == file.bytes)
    })
}

/// Whether the file that is there is the one that should be.
///
/// Size alone, and on purpose: measuring 652 MB with a hash at every start
/// would cost seconds of every start to catch what the `.part` rename already
/// makes unlikely. The hash is checked when the bytes come down.
async fn measures_up(path: &Path, file: &File) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|found| found.len() == file.bytes)
}

/// A digest as the lower-case hexadecimal a checksum is written in.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_model_is_about_670_megabytes() {
        assert_eq!(Files::size(), 670_619_706);
    }

    #[test]
    fn progress_is_percent_and_never_divides_by_nothing() {
        assert_eq!(Progress { done: 0, total: 0 }.percent(), 0);
        assert_eq!(Progress { done: 1, total: 4 }.percent(), 25);
        assert_eq!(
            Progress {
                done: 670_619_706,
                total: 670_619_706
            }
            .percent(),
            100
        );
    }

    #[test]
    fn a_missing_model_says_why_and_a_ready_one_says_nothing() {
        let files = Files::new(PathBuf::from("/nowhere/at/all"));
        assert!(files.why_not().is_some());
        files.set(State::Ready);
        assert_eq!(files.why_not(), None);
    }

    #[test]
    fn a_model_on_the_way_says_how_far() {
        let files = Files::new(PathBuf::from("/nowhere/at/all"));
        files.set(State::Fetching(Progress {
            done: 335_309_853,
            total: 670_619_706,
        }));
        let said = files.why_not().expect("a reason");
        assert!(said.contains("50%"), "{said}");
    }
}
