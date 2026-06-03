//! Explicit, fail-fast configuration for the two transcription
//! boundaries (RFC: Configuration). No required configuration silently
//! defaults inside backend code — every threshold lives here.

use std::path::PathBuf;

/// ASR-normalized sample rate the PCM branch targets. 16 kHz is the
/// whisper/Parakeet input rate.
pub const DEFAULT_ASR_SAMPLE_RATE_HZ: u32 = 16_000;
/// ASR-normalized channel count (mono).
pub const DEFAULT_ASR_CHANNELS: u16 = 1;

/// Default budget for a single contiguous in-memory PCM buffer
/// (`InMemory`). Past it, the coordinator/recorder prefers a chunk
/// source. 16 kHz mono f32 → 64 KiB/s, so 64 MiB ≈ 17 minutes.
pub const DEFAULT_MAX_IN_MEMORY_PCM_BYTES: u64 = 64 * 1024 * 1024;

/// Default number of normalized frames per pulled PCM chunk (1 s at the
/// default ASR rate). Bounds per-chunk allocation for long recordings.
pub const DEFAULT_CHUNK_FRAMES: usize = 16_000;

/// PCM retention policy for live capture (Phase 4) and the decode path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmRetention {
    /// Keep short recordings fully in memory, spill longer ones to a
    /// chunk source bounded by [`AudioConfig::max_in_memory_pcm_bytes`].
    Adaptive,
    /// Never cache PCM at runtime; always decode from the FLAC artifact.
    /// Low-memory mode.
    DecodeOnly,
}

/// Audio-boundary configuration. Derived defaults are explicit and
/// validated; nothing is invented inside a backend.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub asr_sample_rate_hz: u32,
    pub asr_channels: u16,
    pub retention: PcmRetention,
    /// Cap on a single contiguous `InMemory` buffer (bytes).
    pub max_in_memory_pcm_bytes: u64,
    /// Normalized frames per pulled chunk.
    pub chunk_frames: usize,
    /// Directory for temp-backed chunk spill. `None` → use the system
    /// temp dir's zwhisper subdir at spill time.
    pub temp_chunk_dir: Option<PathBuf>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            asr_sample_rate_hz: DEFAULT_ASR_SAMPLE_RATE_HZ,
            asr_channels: DEFAULT_ASR_CHANNELS,
            retention: PcmRetention::Adaptive,
            max_in_memory_pcm_bytes: DEFAULT_MAX_IN_MEMORY_PCM_BYTES,
            chunk_frames: DEFAULT_CHUNK_FRAMES,
            temp_chunk_dir: None,
        }
    }
}

impl AudioConfig {
    /// Fail fast on a structurally invalid config rather than letting a
    /// zero rate / zero chunk size cause a divide-by-zero deep in the
    /// decode path.
    pub fn validate(&self) -> Result<(), String> {
        if self.asr_sample_rate_hz == 0 {
            return Err("audio.asr_sample_rate_hz must be > 0".to_owned());
        }
        if self.asr_channels == 0 {
            return Err("audio.asr_channels must be > 0".to_owned());
        }
        if self.chunk_frames == 0 {
            return Err("audio.chunk_frames must be > 0".to_owned());
        }
        if self.max_in_memory_pcm_bytes == 0 {
            return Err("audio.max_in_memory_pcm_bytes must be > 0".to_owned());
        }
        Ok(())
    }

    /// Frame budget for a single in-memory buffer at the configured
    /// rate (bytes / size_of::<f32>()).
    pub fn in_memory_max_frames(&self) -> u64 {
        self.max_in_memory_pcm_bytes / (std::mem::size_of::<f32>() as u64)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates() {
        AudioConfig::default().validate().unwrap();
    }

    #[test]
    fn zero_rate_rejected() {
        let cfg = AudioConfig {
            asr_sample_rate_hz: 0,
            ..AudioConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn in_memory_frame_budget_matches_byte_budget() {
        let cfg = AudioConfig {
            max_in_memory_pcm_bytes: 4 * 100,
            ..AudioConfig::default()
        };
        assert_eq!(cfg.in_memory_max_frames(), 100);
    }
}
