//! Backend-agnostic audio representation (RFC: AudioSource).
//!
//! This is the first of the two first-class boundaries introduced by
//! `docs/RFC-audio-source-model.md`. Backends no longer receive a bare
//! `&Path`; they receive an [`AudioSource`] that exposes both the
//! persisted artifact (the durable FLAC) and zero or more runtime PCM
//! views derived from it.
//!
//! The split is deliberate and load-bearing:
//!
//! - The **persisted FLAC artifact** ([`AudioArtifact`]) is the durable
//!   source of truth. Its parameters describe the *native capture*
//!   audio (rate, channel layout) — NOT the ASR-normalized rate. Until
//!   the M3 native-rate pipeline lands (Phase 4 of the RFC), capture
//!   still produces 16 kHz mono and so `AudioArtifact.sample_rate_hz`
//!   transiently equals the normalized rate; the boundary is adopted
//!   now so the abstraction is stable before the pipeline change.
//! - The **normalized PCM views** ([`PcmAvailability`]) are derived,
//!   droppable, runtime-only data for in-process ASR: mono, ASR rate
//!   (16 kHz by default), `f32` in `[-1.0, 1.0]`. They must always be
//!   reconstructable from the artifact via decode-and-normalize.
//!
//! Names mirror the RFC; see the "Core Concept: AudioSource" and
//! "PcmChunkSource" sections for the rationale.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

/// Container codec of the persisted [`AudioArtifact`]. Only FLAC ships
/// today (the recorder writes FLAC); the enum exists so a future codec
/// is a single-variant patch rather than a `bool`/string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Flac,
}

impl AudioCodec {
    /// Best-effort codec inference from a file extension. Returns
    /// `None` for unknown extensions so callers fail loudly instead
    /// of silently assuming FLAC.
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("flac") => Some(Self::Flac),
            _ => None,
        }
    }
}

/// The persisted recording artifact — the durable source of truth.
///
/// `sample_rate_hz` / `channels` / `duration` describe the **native
/// capture** audio, deliberately distinct from the ASR-normalized
/// parameters carried by [`AudioMetadata`]. A value of `0`
/// (`sample_rate_hz` / `channels`) or [`Duration::ZERO`] means
/// "not yet probed": file-preferring backends (whisper.cpp, Deepgram)
/// never read these fields, so the compatibility path leaves them
/// unprobed; the decode path (Phase 2) and live capture (Phase 4)
/// populate them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioArtifact {
    pub path: PathBuf,
    pub codec: AudioCodec,
    /// Native capture sample rate. NOT the ASR-normalized rate.
    pub sample_rate_hz: u32,
    /// Native capture channel count.
    pub channels: u16,
    /// Native capture duration.
    pub duration: Duration,
}

impl AudioArtifact {
    /// Construct an artifact handle for a file whose native parameters
    /// have not been probed (the compatibility / file-backend path).
    pub fn unprobed(path: PathBuf, codec: AudioCodec) -> Self {
        Self {
            path,
            codec,
            sample_rate_hz: 0,
            channels: 0,
            duration: Duration::ZERO,
        }
    }

    /// `true` when the native parameters have been filled in (by a
    /// probe/decode or by live capture). Used by diagnostics and by
    /// the coordinator's cross-axis checks.
    pub fn is_probed(&self) -> bool {
        self.sample_rate_hz != 0 && self.channels != 0
    }
}

/// Normalized PCM parameters shared by every chunk a source yields.
/// The RFC pins `channels: 1` for the ASR branch; the struct keeps the
/// field explicit so a future multi-channel ASR engine is representable
/// without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate_hz: u32,
    pub channels: u16,
}

/// How runtime PCM is available for a given [`AudioSource`].
///
/// The coordinator inspects this to decide how to feed a PCM-preferring
/// backend (see `coordinator`): live PCM is used directly when present;
/// otherwise a [`PcmAvailability::DecodeFromArtifact`] source is built
/// on demand from the durable FLAC.
#[derive(Clone)]
pub enum PcmAvailability {
    /// Short recordings: a single contiguous normalized buffer.
    InMemory(Arc<[f32]>),
    /// Medium/long recordings: a pull-based chunk source (see
    /// [`PcmChunkSource`]). Bounds peak RSS for long sessions.
    Chunked(Arc<dyn PcmChunkSource>),
    /// No runtime PCM cache exists (e.g. retry after restart); the
    /// coordinator must decode normalized PCM from the artifact.
    DecodeFromArtifact,
    /// PCM cannot be produced at all (corrupt artifact, decode
    /// disabled). File-preferring backends may still run.
    Unavailable,
}

impl std::fmt::Debug for PcmAvailability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InMemory(buf) => f
                .debug_tuple("InMemory")
                .field(&format_args!("[{} frames]", buf.len()))
                .finish(),
            Self::Chunked(_) => f.write_str("Chunked(<dyn PcmChunkSource>)"),
            Self::DecodeFromArtifact => f.write_str("DecodeFromArtifact"),
            Self::Unavailable => f.write_str("Unavailable"),
        }
    }
}

/// ASR-normalized parameters of every [`PcmAvailability`] view, plus
/// capture provenance. Distinct from [`AudioArtifact`]'s native
/// parameters — this is the one numeric contract a PCM-preferring
/// backend reconciles against its model's expected rate (see the
/// coordinator's cross-axis reconciliation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioMetadata {
    /// ASR-normalized sample rate of every PCM view.
    pub normalized_sample_rate_hz: u32,
    /// ASR-normalized channel count of every PCM view (1 today).
    pub normalized_channels: u16,
    /// Frame count at the normalized rate, when known.
    pub frames: u64,
    pub captured_at: chrono::DateTime<chrono::Utc>,
}

impl AudioMetadata {
    /// The normalized format shared by the PCM views.
    pub fn format(&self) -> PcmFormat {
        PcmFormat {
            sample_rate_hz: self.normalized_sample_rate_hz,
            channels: self.normalized_channels,
        }
    }
}

/// Application-level audio source handed to a backend. Pairs the
/// durable artifact with whatever runtime PCM views exist plus the
/// normalized metadata describing those views.
#[derive(Debug, Clone)]
pub struct AudioSource {
    pub artifact: AudioArtifact,
    pub pcm: PcmAvailability,
    pub metadata: AudioMetadata,
}

impl AudioSource {
    /// Build a source for an encoded file when no runtime PCM cache
    /// exists — the compatibility path used by [`super::transcribe_file`]
    /// and by retry-after-restart. PCM, if a backend needs it, is
    /// decoded from the artifact on demand.
    ///
    /// `normalized_sample_rate_hz` is the ASR rate the decode path will
    /// target; it does not imply any decoding happens up front.
    pub fn from_encoded_file(
        path: &Path,
        codec: AudioCodec,
        normalized_sample_rate_hz: u32,
        captured_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            artifact: AudioArtifact::unprobed(path.to_path_buf(), codec),
            pcm: PcmAvailability::DecodeFromArtifact,
            metadata: AudioMetadata {
                normalized_sample_rate_hz,
                normalized_channels: 1,
                frames: 0,
                captured_at,
            },
        }
    }

    /// Convenience accessor for the artifact path (the most common
    /// thing a file-preferring backend asks for).
    pub fn artifact_path(&self) -> &Path {
        &self.artifact.path
    }
}

/// Pull-based source of normalized mono `f32` PCM chunks (RFC:
/// `PcmChunkSource`). The load-bearing abstraction for long recordings.
///
/// It is **pull-based**: the consumer drives [`Self::next_chunk`], which
/// keeps backpressure trivial and avoids unbounded buffering. The same
/// trait is implemented by the artifact-decode source (Phase 2), the
/// live in-memory/temp-backed sources (Phase 4), and a future streaming
/// source, so backends never branch on recording length.
#[async_trait]
pub trait PcmChunkSource: Send + Sync {
    /// Normalized parameters shared by every chunk this source yields
    /// (`channels == 1`).
    fn format(&self) -> PcmFormat;

    /// Total normalized frame count, when known ahead of time. The
    /// artifact-decode source knows it; a future live source may not.
    fn total_frames(&self) -> Option<u64>;

    /// Pull the next chunk of normalized mono `f32` samples. Returns
    /// `Ok(None)` at end of stream. The returned `Arc<[f32]>` is owned
    /// by the caller; the source must not mutate it afterwards.
    async fn next_chunk(&self) -> Result<Option<Arc<[f32]>>, PcmSourceError>;

    /// Rewind to the start so a failed transcription can be retried
    /// from the same source. Sources that cannot seek return
    /// [`PcmSourceError::NotSeekable`]; the coordinator then falls back
    /// to a fresh [`PcmAvailability::DecodeFromArtifact`] source.
    async fn reset(&self) -> Result<(), PcmSourceError>;
}

/// Errors a [`PcmChunkSource`] can surface while pulling chunks.
#[derive(Debug, Error)]
pub enum PcmSourceError {
    #[error("PCM decode failed: {0}")]
    Decode(String),
    #[error("PCM source I/O error: {0}")]
    Io(String),
    #[error("PCM source is not seekable; rebuild a fresh decode source instead")]
    NotSeekable,
    /// The configured PCM memory/temp budget was exceeded mid-stream.
    #[error("PCM budget exceeded: limit {limit_bytes} bytes")]
    BudgetExceeded { limit_bytes: u64 },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn codec_from_path_is_case_insensitive_and_strict() {
        assert_eq!(
            AudioCodec::from_path(Path::new("/a/b.flac")),
            Some(AudioCodec::Flac)
        );
        assert_eq!(
            AudioCodec::from_path(Path::new("/a/b.FLAC")),
            Some(AudioCodec::Flac)
        );
        assert_eq!(AudioCodec::from_path(Path::new("/a/b.wav")), None);
        assert_eq!(AudioCodec::from_path(Path::new("/a/b")), None);
    }

    #[test]
    fn unprobed_artifact_reports_not_probed() {
        let a = AudioArtifact::unprobed(PathBuf::from("/x.flac"), AudioCodec::Flac);
        assert!(!a.is_probed());
        assert_eq!(a.sample_rate_hz, 0);
        assert_eq!(a.duration, Duration::ZERO);
    }

    #[test]
    fn from_encoded_file_uses_decode_from_artifact() {
        let now = chrono::Utc::now();
        let src =
            AudioSource::from_encoded_file(Path::new("/clip.flac"), AudioCodec::Flac, 16_000, now);
        assert!(matches!(src.pcm, PcmAvailability::DecodeFromArtifact));
        assert_eq!(src.metadata.normalized_sample_rate_hz, 16_000);
        assert_eq!(src.metadata.normalized_channels, 1);
        assert_eq!(src.artifact_path(), Path::new("/clip.flac"));
    }

    #[test]
    fn pcm_availability_debug_is_bounded() {
        let buf: Arc<[f32]> = Arc::from(vec![0.0_f32; 3].into_boxed_slice());
        let s = format!("{:?}", PcmAvailability::InMemory(buf));
        assert!(s.contains("3 frames"), "{s}");
        assert_eq!(
            format!("{:?}", PcmAvailability::DecodeFromArtifact),
            "DecodeFromArtifact"
        );
    }
}
