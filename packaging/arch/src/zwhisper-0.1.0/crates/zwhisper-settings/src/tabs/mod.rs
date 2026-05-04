//! M7 — tab modules.
//!
//! Each tab owns a single FLTK `Group` placed inside the parent
//! `Tabs` widget by `app::App`. Cross-tab communication flows
//! through `UiMessage` (defined in `crate::app`); tabs never call
//! into each other directly.

pub(crate) mod hotkey;
pub(crate) mod models;
pub(crate) mod profile;
pub(crate) mod whisper_cli;
