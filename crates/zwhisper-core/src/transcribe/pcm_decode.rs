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
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

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

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| decode_err(format!("probe: {e}")))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| decode_err("no decodable audio track".to_owned()))?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    let native_rate = params
        .sample_rate
        .ok_or_else(|| decode_err("FLAC header is missing the sample rate".to_owned()))?;
    let channels = params
        .channels
        .ok_or_else(|| decode_err("FLAC header is missing the channel layout".to_owned()))?
        .count();
    if channels == 0 {
        return Err(decode_err("FLAC header declares zero channels".to_owned()));
    }
    let native_channels = u16::try_from(channels).unwrap_or(u16::MAX);
    let native_frames = params.n_frames;

    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| decode_err(format!("decoder build: {e}")))?;

    // Decode every packet into one interleaved f32 buffer, immediately
    // folding to mono so the peak buffer is mono-sized.
    let mut mono_native: Vec<f32> = Vec::with_capacity(
        native_frames
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0),
    );
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // Clean EOF surfaces as an UnexpectedEof IoError.
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            // Track list changed mid-stream (rare for plain FLAC).
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(decode_err(format!("next_packet: {e}"))),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                if sample_buf.is_none() {
                    let spec = *decoded.spec();
                    let duration = decoded.capacity() as u64;
                    sample_buf = Some(SampleBuffer::<f32>::new(duration, spec));
                }
                if let Some(sb) = sample_buf.as_mut() {
                    sb.copy_interleaved_ref(decoded);
                    fold_interleaved_to_mono(sb.samples(), channels, &mut mono_native);
                }
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
    let mut resampler = SincFixedIn::<f32>::new(ratio, 1.0, params, RESAMPLE_CHUNK, 1)
        .map_err(|e| format!("resampler construction: {e}"))?;

    let expected_out =
        ((mono.len() as u128 * u128::from(to_rate)) / u128::from(from_rate)) as usize;
    let delay = resampler.output_delay();
    let mut out: Vec<f32> = Vec::with_capacity(expected_out + delay + RESAMPLE_CHUNK);

    // Feed fixed-size input chunks; pad the final short chunk with
    // silence, then flush the resampler's internal delay.
    let mut pos = 0;
    while pos < mono.len() {
        let end = (pos + RESAMPLE_CHUNK).min(mono.len());
        let mut chunk: Vec<f32> = mono[pos..end].to_vec();
        if chunk.len() < RESAMPLE_CHUNK {
            chunk.resize(RESAMPLE_CHUNK, 0.0);
        }
        let res = resampler
            .process(&[chunk], None)
            .map_err(|e| format!("resample process: {e}"))?;
        out.extend_from_slice(&res[0]);
        pos = end;
    }
    // Flush delayed frames buffered inside the resampler.
    let flushed = resampler
        .process_partial::<Vec<f32>>(None, None)
        .map_err(|e| format!("resample flush: {e}"))?;
    out.extend_from_slice(&flushed[0]);

    // Drop the group delay from the front, then clamp to the expected
    // length to discard the zero-pad/flush tail.
    if delay <= out.len() {
        out.drain(..delay);
    }
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
