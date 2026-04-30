use std::fmt;

use uuid::Uuid;

/// Session identifier generated per `Recorder::start`. M3 will surface
/// this on the D-Bus `StartRecording` reply and `StateChanged` /
/// `RecordingComplete` signals (IDEA.md § 2). Generated inside the
/// audio façade so callers cannot diverge on its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SessionId(Uuid);

impl SessionId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Canonical recorder lifecycle state. The string mapping (`Display`)
/// is the wire format for the M3 `GetStatus` D-Bus method — keep these
/// names stable across the M0 → M3 split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Most variants are observed only after Phase 4 wiring.
pub(crate) enum RecorderState {
    Idle,
    Starting,
    Recording,
    Stopping,
    Failed,
}

impl fmt::Display for RecorderState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Recording => "recording",
            Self::Stopping => "stopping",
            Self::Failed => "failed",
        };
        f.write_str(s)
    }
}

/// Reason that triggered a stop transition. Multi-producer: the bus
/// watchdog, the duration timer, and the Ctrl+C handler all write
/// these into the shared `tokio::sync::watch` channel; the recorder's
/// stop path consumes the latest value to decide between a clean
/// `RecordingReport` and a typed `RecordingError`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // DeviceLost wiring lands in Phase 4.
pub(crate) enum StopReason {
    Running,
    DurationElapsed,
    UserRequested,
    DeviceLost { node: String },
    BusError { stage: String, message: String },
    EosObserved,
}

impl StopReason {
    #[allow(dead_code)] // Used by the M3 reason→exit-code mapper and by tests.
    pub(crate) fn is_error(&self) -> bool {
        matches!(self, Self::DeviceLost { .. } | Self::BusError { .. })
    }
}
