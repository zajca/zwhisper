//! zwhisper-core — shared library for the zwhisper daemon and CLI.
//!
//! Phase 1 of M3 lifted the `audio`, `profile`, and `transcribe`
//! modules out of `zwhisper-cli` into this crate so the daemon
//! (`zwhisperd`) can call them without duplicating the implementation.
//! Module-level docs live next to the relocated code; this file only
//! gates the modules behind their cargo features so a downstream
//! consumer that does not need `GStreamer` (e.g. the CLI's profile-only
//! code path) can opt out via `default-features = false`.

#[cfg(feature = "audio")]
pub mod audio;
#[cfg(feature = "profile")]
pub mod profile;
#[cfg(feature = "transcribe")]
pub mod secrets;
#[cfg(feature = "transcribe")]
pub mod transcribe;

/// Single source of truth for dB↔linear gain conversion and the input
/// gain range. Shared by the profile schema (range validation), the
/// profile writer (`input_gain_db` bounds), the audio pipeline clamp,
/// and the `setup` calibration math, so they cannot drift apart.
#[cfg(any(feature = "audio", feature = "setup", feature = "profile"))]
pub(crate) mod gain;

/// Single source of truth for `PipeWire` node-name validation. The
/// GStreamer device resolver (`audio`), the `setup` calibration layer,
/// and the `profile` comment-preserving `[sources]` writer all validate
/// node names against the same allow-list, so the rules live here rather
/// than being duplicated per feature.
#[cfg(any(feature = "audio", feature = "setup", feature = "profile"))]
pub(crate) mod node_name;

/// Guided microphone setup & calibration (RFC-mic-setup, Phase 0).
/// GStreamer-free: parses `pw-dump` / `wpctl` / `pw-cat` output behind a
/// mockable [`setup::PipewireControl`] trait so the analysis is fully
/// unit-testable without a running `PipeWire` daemon.
#[cfg(feature = "setup")]
pub mod setup;
