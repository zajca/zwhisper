//! M6 — Hotkey toggle support for zwhisper.
//!
//! Shared between the tray (full portal flow + listener task)
//! and the CLI (`zwhisper toggle`, `zwhisper hotkey {…}`).
//! See `docs/M6-plan.md` for architecture and `DoD`.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

pub mod active_session;
pub mod config;
pub mod toggle;

#[cfg(feature = "portal")]
pub mod portal;

#[cfg(feature = "portal")]
pub mod probe;
