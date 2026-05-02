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
pub mod transcribe;
