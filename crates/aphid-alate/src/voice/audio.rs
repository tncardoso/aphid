//! Any audio file, as the samples a speech model reads.
//!
//! A model wants one shape and one shape only: 16 kHz, one channel, `f32` in
//! `[-1, 1]`. What arrives is whatever the sender's telephone made — an Opus
//! voice message, an mp3 sent as a file, the AAC track inside a video note.
//!
//! There are two routes here because Opus needs one of its own. It is the
//! codec every Telegram voice message is in, and the one codec symphonia
//! cannot decode. The decoder used for it takes packets rather than a file, so
//! the Ogg pages are unwrapped first.
//!
//! The Opus route is the cheap one, and not by accident: an Opus decoder
//! synthesises at whatever rate it is asked for, so asking for 16 kHz gets the
//! model's rate out of the codec itself. Nothing is resampled. Everything else
//! comes out at the rate it was recorded at and is brought down by [`rubato`].
//!
//! # What AAC costs
//!
//! Measured against another decoder of the same file, the AAC here is 19 dB
//! down — approximately the loss of the encoding itself, added a second time.
//! Opus, mp3 and wav all read correctly; a round video or an `.m4a` gives a
//! transcription with mistakes in it. Nothing here can mend that: it is what
//! the decoder gives. It is kept because a poor reading of a round video is
//! more use than no reading of one.

use std::io::Cursor;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// What every speech model in this crate reads.
pub const RATE: u32 = 16_000;

/// The magic an Ogg page starts with.
const OGG: &[u8] = b"OggS";
/// The magic the first packet of an Opus stream starts with.
const OPUS: &[u8] = b"OpusHead";

/// Read `bytes` as audio, and give back 16 kHz mono samples.
///
/// # Errors
///
/// Fails with a sentence when the bytes are not audio this build can read, or
/// when they are audio and are damaged.
pub fn samples(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if is_ogg_opus(bytes) {
        opus(bytes)
    } else {
        anything(bytes)
    }
}

/// Whether these bytes are Opus in an Ogg container.
///
/// The first page of an Ogg stream holds one packet, and for Opus that packet
/// is the `OpusHead` header. Looking for it inside the first page is enough to
/// tell Opus from the Vorbis that symphonia would take.
fn is_ogg_opus(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    head.starts_with(OGG) && head.windows(OPUS.len()).any(|window| window == OPUS)
}

/// The Opus route: Ogg pages in, 16 kHz samples out.
fn opus(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let mut reader = ogg::PacketReader::new(Cursor::new(bytes));

    // The two headers, in order and always present: `OpusHead` carries the
    // pre-skip, and `OpusTags` carries nothing worth having.
    let header = reader
        .read_packet_expected()
        .map_err(|error| format!("the recording is not readable Ogg: {error}"))?;
    let skip = pre_skip(&header.data)?;
    reader
        .read_packet_expected()
        .map_err(|error| format!("the recording has no Opus tags: {error}"))?;

    // One channel, because a model reads one. A stereo stream is downmixed by
    // the decoder itself, which is cheaper and better than doing it here.
    let mut decoder = opus_decoder::OpusDecoder::new(RATE, 1)
        .map_err(|error| format!("the Opus decoder did not start: {error:?}"))?;

    // The longest an Opus packet can be is 120 ms, so this frame is never
    // grown once the loop starts.
    let mut frame = vec![0f32; (RATE as usize / 1000) * 120];
    let mut samples = Vec::new();

    loop {
        match reader.read_packet() {
            Ok(None) => break,
            Ok(Some(packet)) => {
                let count = decoder
                    .decode_float(&packet.data, &mut frame, false)
                    .map_err(|error| format!("the recording could not be decoded: {error:?}"))?;
                samples.extend_from_slice(&frame[..count]);
            }
            Err(error) => return Err(format!("the recording ends badly: {error}")),
        }
    }

    // The encoder's own warm-up, which the header says the length of. It is
    // counted at 48 kHz whatever the decoder was asked for.
    let skip = (skip as usize * RATE as usize) / 48_000;
    Ok(samples.split_off(skip.min(samples.len())))
}

/// The pre-skip an `OpusHead` declares, in samples at 48 kHz.
fn pre_skip(header: &[u8]) -> Result<u16, String> {
    // Magic, version, channels, then the pre-skip: bytes ten and eleven.
    if header.len() < 12 || !header.starts_with(OPUS) {
        return Err("the recording has no Opus header".to_owned());
    }
    Ok(u16::from_le_bytes([header[10], header[11]]))
}

/// Everything that is not Opus, by way of symphonia.
fn anything(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let source = MediaSourceStream::new(Box::new(Cursor::new(bytes.to_vec())), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| format!("the file is not audio this build can read: {error}"))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| "the file has no audio in it".to_owned())?;
    let id = track.id;
    let parameters = track
        .codec_params
        .clone()
        .ok_or_else(|| "the file does not say how it is encoded".to_owned())?;
    let parameters = parameters
        .audio()
        .ok_or_else(|| "the file has no audio in it".to_owned())?
        .clone();

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&parameters, &AudioDecoderOptions::default())
        .map_err(|error| format!("the file is encoded in a way this build cannot read: {error}"))?;

    let mut rate = parameters.sample_rate.unwrap_or(RATE);
    let mut mono: Vec<f32> = Vec::new();
    let mut interleaved: Vec<f32> = Vec::new();

    while let Some(packet) = format
        .next_packet()
        .map_err(|error| format!("the file ends badly: {error}"))?
    {
        if packet.track_id != id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A damaged packet in the middle of a recording is not a reason to
            // lose the recording.
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(error) => return Err(format!("the file could not be decoded: {error}")),
        };

        let spec = decoded.spec();
        rate = spec.rate();
        let channels = spec.channels().count().max(1);

        // Interleaved, so one frame is `channels` samples in a row and the
        // downmix below is a walk rather than a gather.
        interleaved.clear();
        decoded.copy_to_vec_interleaved(&mut interleaved);

        mono.reserve(interleaved.len() / channels);
        for frame in interleaved.chunks_exact(channels) {
            // The mean, so a stereo recording is no louder than a mono one.
            mono.push(frame.iter().sum::<f32>() / channels as f32);
        }
    }

    if mono.is_empty() {
        return Err("the file has no sound in it".to_owned());
    }
    resample(mono, rate)
}

/// Bring `samples` from `rate` to [`RATE`].
///
/// # Errors
///
/// Fails when the resampler refuses the ratio, which means a rate no recording
/// has.
fn resample(samples: Vec<f32>, rate: u32) -> Result<Vec<f32>, String> {
    use rubato::audioadapter_buffers::direct::SequentialSlice;
    use rubato::{Fft, FixedSync, Resampler};

    if rate == RATE {
        return Ok(samples);
    }

    // A chunk of a quarter second: big enough that the FFT is efficient, small
    // enough that a short recording is not mostly padding.
    let chunk = (rate as usize / 4).max(256);
    let mut resampler =
        Fft::<f32>::new(rate as usize, RATE as usize, chunk, 1, FixedSync::Input)
            .map_err(|error| format!("{rate} Hz cannot be brought to {RATE} Hz: {error}"))?;

    let frames = samples.len();
    let input = SequentialSlice::new(&samples, 1, frames)
        .map_err(|error| format!("the recording could not be read for resampling: {error}"))?;

    // `process_all` takes the whole clip: it feeds the resampler in its own
    // chunks and trims the startup delay, which doing it by hand gets wrong as
    // leading silence.
    let done = resampler
        .process_all(&input, frames, None)
        .map_err(|error| format!("the recording could not be resampled: {error}"))?;
    Ok(done.take_data())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mono 16-bit PCM wav of `seconds` at `rate`, holding a sine of `hz`.
    ///
    /// Written here rather than kept in the repository: a wav is a header and
    /// then the samples, and a generated one can carry a frequency the test
    /// then looks for.
    fn wav(rate: u32, seconds: f32, hz: f32) -> Vec<u8> {
        let frames = (rate as f32 * seconds) as u32;
        let data = frames * 2;
        let mut bytes = Vec::with_capacity(44 + data as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes()); // bytes a second
        bytes.extend_from_slice(&2u16.to_le_bytes()); // bytes a frame
        bytes.extend_from_slice(&16u16.to_le_bytes()); // bits a sample
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data.to_le_bytes());
        for frame in 0..frames {
            let at = frame as f32 / rate as f32;
            let value = (std::f32::consts::TAU * hz * at).sin() * 0.5;
            #[expect(clippy::cast_possible_truncation, reason = "a sine of half scale fits")]
            bytes.extend_from_slice(&((value * 32767.0) as i16).to_le_bytes());
        }
        bytes
    }

    /// How many times the samples cross zero going up, which for a sine is how
    /// many cycles are in them.
    fn crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
            .count()
    }

    #[test]
    fn a_wav_already_at_the_right_rate_is_left_alone() {
        let samples = samples(&wav(RATE, 1.0, 440.0)).expect("a wav is audio");
        assert_eq!(samples.len(), RATE as usize);
        assert!(samples.iter().all(|value| value.abs() <= 1.0));
    }

    #[test]
    fn a_wav_at_another_rate_comes_back_at_sixteen_thousand() {
        let samples = samples(&wav(44_100, 1.0, 440.0)).expect("a wav is audio");
        // The resampler's own trimming moves the count by a few frames.
        let off = samples.len().abs_diff(RATE as usize);
        assert!(off < 200, "{} samples", samples.len());
    }

    #[test]
    fn resampling_keeps_the_frequency() {
        // 440 Hz for one second is 440 cycles, whatever rate it is carried at.
        let samples = samples(&wav(48_000, 1.0, 440.0)).expect("a wav is audio");
        let cycles = crossings(&samples);
        assert!(cycles.abs_diff(440) <= 3, "{cycles} cycles");
    }

    #[test]
    fn a_stereo_wav_is_mixed_down_and_not_made_louder() {
        // Two channels of the same sine: the mean of a value with itself is
        // that value, so the mixdown must not double it.
        let mut bytes = wav(RATE, 0.1, 440.0);
        let frames = (RATE / 10) as usize;
        // Turn the header stereo and interleave the samples with themselves.
        bytes[22] = 2;
        let rate_bytes = (RATE * 4).to_le_bytes();
        bytes[28..32].copy_from_slice(&rate_bytes);
        bytes[32] = 4;
        let samples_at = 44;
        let mono: Vec<u8> = bytes[samples_at..].to_vec();
        let mut stereo = Vec::with_capacity(mono.len() * 2);
        for frame in mono.as_chunks::<2>().0 {
            stereo.extend_from_slice(frame);
            stereo.extend_from_slice(frame);
        }
        bytes.truncate(samples_at);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a tenth of a second is small"
        )]
        let size = stereo.len() as u32;
        bytes[40..44].copy_from_slice(&size.to_le_bytes());
        bytes[4..8].copy_from_slice(&(36 + size).to_le_bytes());
        bytes.extend_from_slice(&stereo);

        let samples = samples(&bytes).expect("a wav is audio");
        assert_eq!(samples.len(), frames);
        let loudest = samples
            .iter()
            .fold(0f32, |most, value| most.max(value.abs()));
        assert!((0.4..=0.6).contains(&loudest), "{loudest}");
    }

    #[test]
    fn nonsense_is_refused_with_a_sentence() {
        let why = samples(b"this is not audio at all").expect_err("not audio");
        assert!(why.contains("not audio"), "{why}");
    }

    #[test]
    fn opus_in_ogg_takes_the_other_route() {
        // Not a real stream, so it fails inside the Opus route and not in
        // symphonia's — which is what this asserts: the sniff, not the decode.
        let mut bytes = b"OggS".to_vec();
        bytes.extend_from_slice(&[0; 22]);
        bytes.extend_from_slice(b"OpusHead");
        assert!(is_ogg_opus(&bytes));
        let why = samples(&bytes).expect_err("a truncated page");
        assert!(why.contains("Ogg") || why.contains("Opus"), "{why}");
    }
}
