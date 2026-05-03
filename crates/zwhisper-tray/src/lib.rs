//! `zwhisper-tray` — M4 milestone library surface.
//!
//! The binary lives in `src/main.rs`; this `lib.rs` exists so
//! integration tests (and the binary itself) can share the modules
//! without re-importing through the binary target. See
//! `docs/M4-plan.md` § "Crate dependency graph".

pub mod cmd;
pub mod config;
pub mod dbus;
pub mod hotkey;
pub mod icon;
pub mod pump;
pub mod session_env;
pub mod single_instance;
pub mod sink;
pub mod state;
pub mod supervisor;
pub mod tray;
pub(crate) mod version;
