pub(crate) mod devices;
pub mod error;
pub(crate) mod pipeline;
pub mod recorder;
pub mod state;
pub(crate) mod watchdog;

#[allow(unused_imports)]
pub use error::{DeviceError, RecordingError};
#[allow(unused_imports)]
pub use recorder::{RecordOptions, RecordingReport, record_blocking};
