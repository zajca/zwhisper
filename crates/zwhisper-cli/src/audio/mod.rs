pub(crate) mod devices;
pub(crate) mod error;
pub(crate) mod pipeline;
pub(crate) mod recorder;
pub(crate) mod state;
pub(crate) mod watchdog;

#[allow(unused_imports)]
pub(crate) use error::{DeviceError, RecordingError};
#[allow(unused_imports)]
pub(crate) use recorder::{RecordOptions, RecordingReport, record_blocking};
