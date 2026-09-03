//! Speech to text, so a client that carries audio can hand the agent words.
//!
//! A recording is not a thing an agent can read. This module makes it one, and
//! stops there: what the words then mean, and which conversation they go to, is
//! the client's business. The Telegram bridge is the first client to ask, and
//! nothing here knows that.
//!
//! ```text
//! bytes ──sniff──► decode ──► 16 kHz mono ──► Parakeet ──► words
//! ```
//!
//! # The seam
//!
//! [`Transcribe`] is a trait for the reason [`Api`] is one: a test must be able
//! to put something else in the place of a model that is 670 MB on the disk and
//! as much again in memory.
//!
//! # What it costs
//!
//! Inference is arithmetic and nothing else, so it runs on a blocking thread —
//! the daemon's runtime has one worker, and a recording would stop the clock,
//! the socket and the bot for as long as it took. The model is held behind a
//! lock that is taken **inside** that thread, never across an `await`, so two
//! recordings at once queue rather than fight for the same cores.
//!
//! [`Api`]: crate::telegram::api::Api

pub mod audio;
pub mod model;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use transcribe_rs::onnx::Quantization;
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};

pub use model::{Files, Progress, State};

/// One transcription in flight.
pub type Transcription<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// What a client needs of a transcriber.
pub trait Transcribe: Send + Sync {
    /// Turn one audio file into words.
    ///
    /// The bytes are a whole file, of whatever kind the sender's telephone
    /// made. An empty answer is not a failure: it is a recording with no speech
    /// in it, and the caller says so in its own words.
    fn transcribe(&self, audio: Vec<u8>) -> Transcription<'_>;

    /// Why a recording cannot be read yet, and `None` when one can.
    ///
    /// Asked before anything is fetched, so a model that is still coming down
    /// does not cost a chat a download on top of the wait.
    fn not_yet(&self) -> Option<String> {
        None
    }
}

/// A shared transcriber, held by every client that can carry audio.
pub type TranscribeFn = Arc<dyn Transcribe>;

/// Longer than this, and the recording is cut into pieces before it is read.
///
/// Parakeet takes the whole waveform into one tensor, so a long recording is
/// not slow but impossible. The chunker cuts at the quietest point near each
/// boundary and joins the texts.
const CHUNK: f32 = 30.0;

/// How often the keeper looks at the clock.
const TICK: Duration = Duration::from_secs(30);

/// The real transcriber.
pub struct Voice {
    files: Files,
    longest: Option<Duration>,
    /// The model, once it has been read, and the moment it was last used.
    ///
    /// A `std::sync::Mutex` and not tokio's: it is only ever taken on a
    /// blocking thread, and one that cannot be held across an `await` is one
    /// that cannot be held across an `await` by mistake.
    held: Arc<Mutex<Option<Held>>>,
}

/// The model in memory.
struct Held {
    model: ParakeetModel,
    used: Instant,
}

impl Voice {
    /// A transcriber that reads the model in `files` when it first has to.
    #[must_use]
    pub fn new(files: Files, longest: Option<Duration>) -> Self {
        Self {
            files,
            longest,
            held: Arc::new(Mutex::new(None)),
        }
    }

    /// The model's files, so a caller can say how far a download has got.
    #[must_use]
    pub fn files(&self) -> &Files {
        &self.files
    }

    /// Drop the model when `patience` passes with nothing to read.
    ///
    /// Runs until the task is dropped. `None` keeps the model for as long as
    /// the daemon runs, which is what an alate that hears something every few
    /// minutes wants.
    #[must_use]
    pub fn keep(&self, patience: Option<Duration>) -> tokio::task::JoinHandle<()> {
        let held = Arc::clone(&self.held);
        tokio::spawn(async move {
            let Some(patience) = patience else {
                return;
            };
            loop {
                tokio::time::sleep(TICK).await;
                let mut held = match held.lock() {
                    Ok(held) => held,
                    Err(_) => return,
                };
                if held
                    .as_ref()
                    .is_some_and(|model| model.used.elapsed() >= patience)
                {
                    tracing::info!("voice: model idle, dropped");
                    *held = None;
                }
            }
        })
    }
}

impl Transcribe for Voice {
    fn not_yet(&self) -> Option<String> {
        self.files.why_not()
    }

    fn transcribe(&self, audio: Vec<u8>) -> Transcription<'_> {
        let held = Arc::clone(&self.held);
        let directory = self.files.directory().to_path_buf();
        let longest = self.longest;
        let why_not = self.not_yet();

        Box::pin(async move {
            if let Some(why) = why_not {
                return Err(why);
            }

            tokio::task::spawn_blocking(move || {
                let samples = audio::samples(&audio)?;
                let seconds = samples.len() as f32 / audio::RATE as f32;
                if let Some(longest) = longest
                    && seconds > longest.as_secs_f32()
                {
                    return Err(format!(
                        "that recording is {:.0} minutes long, and this alate takes {:.0}",
                        seconds / 60.0,
                        longest.as_secs_f32() / 60.0
                    ));
                }

                // Taken here and nowhere else: this is a blocking thread, so
                // waiting on it costs nothing the runtime needs.
                let mut held = held
                    .lock()
                    .map_err(|_| "the transcriber is in a bad state".to_owned())?;
                if held.is_none() {
                    tracing::info!(directory = %directory.display(), "voice: reading model");
                    let model =
                        ParakeetModel::load(&directory, &Quantization::Int8).map_err(|error| {
                            format!("the transcription model did not load: {error}")
                        })?;
                    *held = Some(Held {
                        model,
                        used: Instant::now(),
                    });
                }
                let held = held.as_mut().expect("just put there");
                held.used = Instant::now();

                let text = read(&mut held.model, &samples, seconds)?;
                Ok(text.trim().to_owned())
            })
            .await
            .map_err(|error| format!("the transcriber stopped: {error}"))?
        })
    }
}

/// Read `samples`, in one piece or in several.
fn read(model: &mut ParakeetModel, samples: &[f32], seconds: f32) -> Result<String, String> {
    if seconds <= CHUNK {
        return model
            .transcribe_with(samples, &ParakeetParams::default())
            .map(|result| result.text)
            .map_err(|error| format!("the recording could not be read: {error}"));
    }

    use transcribe_rs::transcriber::{
        EnergyAdaptiveChunked, EnergyAdaptiveConfig, Transcriber as Chunked,
    };

    let mut chunked = EnergyAdaptiveChunked::new(
        EnergyAdaptiveConfig {
            target_chunk_secs: CHUNK,
            ..Default::default()
        },
        transcribe_rs::TranscribeOptions::default(),
    );
    chunked
        .transcribe(model, samples)
        .map(|result| result.text)
        .map_err(|error| format!("the recording could not be read: {error}"))
}
