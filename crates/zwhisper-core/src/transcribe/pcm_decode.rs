//! FLAC → normalized PCM decode (RFC Phase 2).
//!
//! Decodes the persisted FLAC artifact into normalized mono `f32` at the
//! ASR rate (16 kHz by default) using `symphonia` (pure-Rust FLAC
//! decode) + `rubato` (sinc resampling). This is the
//! `PcmAvailability::DecodeFromArtifact` path: it lets a PCM-preferring
//! backend (Parakeet) run after a restart, or retry from the durable
//! recording, with PCM produced by the *same* normalization the live
//! capture branch will use.
//!
//! ## Memory characteristic
//!
//! [`ArtifactDecodeSource`] decodes the whole file into one normalized
//! buffer up front, then hands it out in chunks (`reset` rewinds the
//! cursor — functionally the seekable contract the RFC asks for,
//! without re-decoding). This is correct and bounded by the recording
//! length; the bounded-RSS *streaming* path for very long recordings is
//! the temp-backed live source (Phase 4). Retry-from-artifact uses this
//! buffered decoder.
//!
//! ## Fast paths
//!
//! When the FLAC is already mono at the target rate (today's capture
//! produces 16 kHz mono), downmix and resampling are skipped entirely —
//! no resampler artifacts, no extra work.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

use super::audio_source::{PcmChunkSource, PcmFormat, PcmSourceError};
use super::error::TranscribeError;

// ----- Resampler quality parameters (algorithm tuning, not user config).
// These are sinc-interpolation knobs, not configurable values like
// timeouts/limits/URLs, so they live as documented module constants.
const SINC_LEN: usize = 256;
const SINC_F_CUTOFF: f32 = 0.95;
const SINC_OVERSAMPLING: usize = 256;
/// Fixed input-frame count fed to the resampler per `process` call.
const RESAMPLE_CHUNK: usize = 1024;

/// Native parameters read from the FLAC header during decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactProbe {
    pub native_sample_rate_hz: u32,
    pub native_channels: u16,
    /// Frame count per channel at the native rate, when the header
    /// declares it.
    pub native_frames: Option<u64>,
}

/// Decode a FLAC file into normalized mono `f32` at `target_rate_hz`.
/// Returns the samples plus the native parameters probed from the
/// header. Synchronous and fully buffering — the testable core.
pub fn decode_flac_normalized(
    path: &Path,
    target_rate_hz: u32,
) -> Result<(Vec<f32>, ArtifactProbe), TranscribeError> {
    if target_rate_hz == 0 {
        return Err(TranscribeError::AudioDecode {
            path: path.to_path_buf(),
            reason: "target ASR sample rate must be > 0".to_owned(),
        });
    }

    let decode_err = |reason: String| TranscribeError::AudioDecode {
        path: path.to_path_buf(),
        reason,
    };

    let file = File::open(path).map_err(|e| decode_err(format!("open: {e}")))?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    hint.with_extension("flac");

    // symphonia 0.6: `probe()` returns the `FormatReader` directly and
    // takes the options by value.
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| decode_err(format!("probe: {e}")))?;

    // The default audio track falls back to the first track with a known
    // codec, matching the old "first decodable track" selection.
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| decode_err("no decodable audio track".to_owned()))?;
    let track_id = track.id;
    let native_frames = track.num_frames;
    // Clone the audio params so the `&format` borrow is released before
    // the decode loop borrows `format` mutably.
    let params = track
        .codec_params
        .as_ref()
        .and_then(|c| c.audio())
        .ok_or_else(|| decode_err("track is missing audio codec parameters".to_owned()))?
        .clone();
    let native_rate = params
        .sample_rate
        .ok_or_else(|| decode_err("FLAC header is missing the sample rate".to_owned()))?;
    let channels = params
        .channels
        .as_ref()
        .ok_or_else(|| decode_err("FLAC header is missing the channel layout".to_owned()))?
        .count();
    if channels == 0 {
        return Err(decode_err("FLAC header declares zero channels".to_owned()));
    }
    let native_channels = u16::try_from(channels).unwrap_or(u16::MAX);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| decode_err(format!("decoder build: {e}")))?;

    // Decode every packet into one interleaved f32 buffer, immediately
    // folding to mono so the peak buffer is mono-sized. `scratch` is a
    // reused interleaved staging buffer, resized to each packet's frame
    // count before the copy.
    let mut mono_native: Vec<f32> = Vec::with_capacity(
        native_frames
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0),
    );
    let mut scratch: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            // symphonia 0.6: clean EOF is `Ok(None)`; a mid-stream
            // track-list reset (rare for plain FLAC) also ends decoding.
            Ok(None) | Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(decode_err(format!("next_packet: {e}"))),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                scratch.resize(decoded.samples_interleaved(), 0.0);
                decoded.copy_to_slice_interleaved(&mut scratch);
                fold_interleaved_to_mono(&scratch, channels, &mut mono_native);
            }
            // Recoverable per-packet errors: skip and keep going (the
            // loop continues on its own).
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => {}
            Err(e) => return Err(decode_err(format!("decode: {e}"))),
        }
    }

    let probe = ArtifactProbe {
        native_sample_rate_hz: native_rate,
        native_channels,
        native_frames,
    };

    // Fast path: already at the target rate → no resampling.
    if native_rate == target_rate_hz {
        return Ok((mono_native, probe));
    }

    let resampled = resample_mono(&mono_native, native_rate, target_rate_hz).map_err(decode_err)?;
    Ok((resampled, probe))
}

/// Average all channels of an interleaved buffer into a mono tail
/// appended to `out`. Channel count 1 is a pure copy.
fn fold_interleaved_to_mono(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels == 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let frames = interleaved.len() / channels;
    out.reserve(frames);
    let inv = 1.0_f32 / channels as f32;
    for f in 0..frames {
        let base = f * channels;
        let sum: f32 = interleaved[base..base + channels].iter().sum();
        out.push(sum * inv);
    }
}

/// Resample mono `f32` from `from_rate` to `to_rate` with a high-quality
/// sinc resampler. Output is trimmed to the analytically-expected frame
/// count (dropping the resampler's group delay and the zero-padded
/// flush tail) so the result is deterministic.
fn resample_mono(mono: &[f32], from_rate: u32, to_rate: u32) -> Result<Vec<f32>, String> {
    if mono.is_empty() {
        return Ok(Vec::new());
    }
    let ratio = f64::from(to_rate) / f64::from(from_rate);
    let params = SincInterpolationParameters {
        sinc_len: SINC_LEN,
        f_cutoff: SINC_F_CUTOFF,
        oversampling_factor: SINC_OVERSAMPLING,
        interpolation: SincInterpolationType::Cubic,
        window: WindowFunction::BlackmanHarris2,
    };
    let mut resampler =
        Async::<f32>::new_sinc(ratio, 1.0, &params, RESAMPLE_CHUNK, 1, FixedAsync::Input)
            .map_err(|e| format!("resampler construction: {e}"))?;

    // rubato 3.0: `process_all_into_buffer` runs the whole clip in one
    // call — it drops the resampler's group delay from the front, feeds
    // the final partial chunk, and pumps silence to flush the tail,
    // writing `ceil(ratio * input_len)` frames into the output buffer.
    let out_capacity = resampler.process_all_needed_output_len(mono.len());
    let mut out = vec![0.0_f32; out_capacity];

    // Mono == single interleaved channel, so a flat slice adapter works.
    let input = InterleavedSlice::new(mono, 1, mono.len())
        .map_err(|e| format!("resample input adapter: {e}"))?;
    let mut output = InterleavedSlice::new_mut(&mut out, 1, out_capacity)
        .map_err(|e| format!("resample output adapter: {e}"))?;

    let (_in_frames, out_frames) = resampler
        .process_all_into_buffer(&input, &mut output, mono.len(), None)
        .map_err(|e| format!("resample process: {e}"))?;
    out.truncate(out_frames);

    // Clamp to the analytically-expected (floored) frame count so the
    // result stays deterministic and matches the pre-rubato-3.0 output
    // length (`ceil` above can yield one extra frame).
    let expected_out =
        ((mono.len() as u128 * u128::from(to_rate)) / u128::from(from_rate)) as usize;
    out.truncate(expected_out);
    Ok(out)
}

/// A [`PcmChunkSource`] that decodes the persisted FLAC artifact into
/// normalized mono `f32` once, then yields it in chunks. `reset` rewinds
/// the cursor (the buffer is retained, so retries are cheap).
pub struct ArtifactDecodeSource {
    path: PathBuf,
    format: PcmFormat,
    chunk_frames: usize,
    state: Mutex<DecodeCursor>,
}

struct DecodeCursor {
    samples: Arc<[f32]>,
    cursor: usize,
}

impl std::fmt::Debug for ArtifactDecodeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactDecodeSource")
            .field("path", &self.path)
            .field("format", &self.format)
            .field("chunk_frames", &self.chunk_frames)
            .field("state", &"<DecodeCursor>")
            .finish()
    }
}

impl ArtifactDecodeSource {
    /// Decode `path` into normalized mono `f32` at `target_rate_hz` and
    /// build a chunk source yielding `chunk_frames`-sized chunks.
    pub fn open(
        path: &Path,
        target_rate_hz: u32,
        chunk_frames: usize,
    ) -> Result<Self, TranscribeError> {
        if chunk_frames == 0 {
            return Err(TranscribeError::AudioDecode {
                path: path.to_path_buf(),
                reason: "chunk_frames must be > 0".to_owned(),
            });
        }
        let (samples, _probe) = decode_flac_normalized(path, target_rate_hz)?;
        Ok(Self {
            path: path.to_path_buf(),
            format: PcmFormat {
                sample_rate_hz: target_rate_hz,
                channels: 1,
            },
            chunk_frames,
            state: Mutex::new(DecodeCursor {
                samples: Arc::from(samples.into_boxed_slice()),
                cursor: 0,
            }),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, DecodeCursor>, PcmSourceError> {
        self.state
            .lock()
            .map_err(|_| PcmSourceError::Io("decode cursor mutex poisoned".to_owned()))
    }
}

#[async_trait]
impl PcmChunkSource for ArtifactDecodeSource {
    fn format(&self) -> PcmFormat {
        self.format
    }

    fn total_frames(&self) -> Option<u64> {
        // Lock is cheap and never contended across an await here.
        self.lock().ok().map(|s| s.samples.len() as u64)
    }

    async fn next_chunk(&self) -> Result<Option<Arc<[f32]>>, PcmSourceError> {
        let mut state = self.lock()?;
        if state.cursor >= state.samples.len() {
            return Ok(None);
        }
        let end = (state.cursor + self.chunk_frames).min(state.samples.len());
        let chunk: Arc<[f32]> = Arc::from(&state.samples[state.cursor..end]);
        state.cursor = end;
        Ok(Some(chunk))
    }

    async fn reset(&self) -> Result<(), PcmSourceError> {
        self.lock()?.cursor = 0;
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("audio")
            .join(name)
    }

    #[test]
    fn decode_16k_mono_is_passthrough() {
        let (samples, probe) = decode_flac_normalized(&fixture("sine-16k-mono.flac"), 16_000)
            .expect("decode 16k mono");
        assert_eq!(probe.native_sample_rate_hz, 16_000);
        assert_eq!(probe.native_channels, 1);
        // 0.4 s @ 16 kHz = 6400 frames (allow a small encoder fence).
        assert!(
            (6300..=6500).contains(&samples.len()),
            "expected ~6400 frames, got {}",
            samples.len()
        );
        // A 440 Hz tone is not silent.
        let peak = samples.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.1, "tone should have non-trivial amplitude: {peak}");
    }

    #[test]
    fn decode_48k_stereo_downmixes_and_resamples_to_16k() {
        let (samples, probe) =
            decode_flac_normalized(&fixture("sine-48k-stereo.flac"), 16_000).expect("decode 48k");
        assert_eq!(probe.native_sample_rate_hz, 48_000);
        assert_eq!(probe.native_channels, 2);
        // 0.4 s resampled 48k -> 16k = 6400 frames (mono).
        assert!(
            (6300..=6500).contains(&samples.len()),
            "expected ~6400 resampled frames, got {}",
            samples.len()
        );
        let peak = samples.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.05, "resampled tone should not be silent: {peak}");
    }

    #[test]
    fn decode_silence_yields_near_zero_samples() {
        let (samples, _probe) =
            decode_flac_normalized(&fixture("silence-16k-mono.flac"), 16_000).expect("decode");
        assert!(!samples.is_empty());
        let peak = samples.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        assert!(peak < 1e-3, "silence should be ~0, peak was {peak}");
    }

    #[test]
    fn decode_missing_file_is_typed_error() {
        let err = decode_flac_normalized(&fixture("does-not-exist.flac"), 16_000).unwrap_err();
        assert!(matches!(err, TranscribeError::AudioDecode { .. }));
    }

    #[test]
    fn decode_non_flac_is_typed_error() {
        // The whisper JSON fixture is decidedly not a FLAC stream.
        let bad = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("whisper-cpp-segments.json");
        let err = decode_flac_normalized(&bad, 16_000).unwrap_err();
        assert!(matches!(err, TranscribeError::AudioDecode { .. }));
    }

    #[test]
    fn zero_target_rate_rejected() {
        let err = decode_flac_normalized(&fixture("sine-16k-mono.flac"), 0).unwrap_err();
        assert!(matches!(err, TranscribeError::AudioDecode { .. }));
    }

    #[tokio::test]
    async fn chunk_source_yields_all_frames_then_none() {
        let src = ArtifactDecodeSource::open(&fixture("sine-16k-mono.flac"), 16_000, 1000)
            .expect("open source");
        assert_eq!(src.format().sample_rate_hz, 16_000);
        assert_eq!(src.format().channels, 1);
        let total = src.total_frames().expect("known total");
        let mut collected = 0_u64;
        let mut chunks = 0;
        while let Some(chunk) = src.next_chunk().await.expect("chunk") {
            assert!(chunk.len() <= 1000);
            collected += chunk.len() as u64;
            chunks += 1;
        }
        assert_eq!(collected, total);
        assert!(chunks >= 6, "1000-frame chunks over ~6400 frames");
        // End of stream is sticky.
        assert!(src.next_chunk().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn chunk_source_reset_rewinds() {
        let src = ArtifactDecodeSource::open(&fixture("silence-16k-mono.flac"), 16_000, 512)
            .expect("open");
        let first = src.next_chunk().await.unwrap().expect("a chunk");
        // Drain.
        while src.next_chunk().await.unwrap().is_some() {}
        src.reset().await.unwrap();
        let again = src
            .next_chunk()
            .await
            .unwrap()
            .expect("a chunk after reset");
        assert_eq!(first.len(), again.len());
        assert_eq!(&first[..], &again[..]);
    }

    #[test]
    fn fold_interleaved_to_mono_averages_channels() {
        let mut out = Vec::new();
        // 2 frames, stereo: [L0,R0, L1,R1]
        fold_interleaved_to_mono(&[1.0, 3.0, -2.0, 2.0], 2, &mut out);
        assert_eq!(out, vec![2.0, 0.0]);
    }

    #[test]
    fn resample_empty_is_empty() {
        assert!(resample_mono(&[], 48_000, 16_000).unwrap().is_empty());
    }
}
