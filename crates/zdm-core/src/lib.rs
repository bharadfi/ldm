//! zdm-core: segmented, resumable HTTP download engine.

mod engine;
pub mod grab;
pub mod hls;
mod probe;
#[cfg(any(test, feature = "testserver"))]
pub mod testserver;

pub use engine::{download, download_probed, Cancelled, Options};
pub use tokio_util::sync::CancellationToken;
pub use probe::{build_client, filename_from_disposition, probe, Probe};
