//! zdm-core: segmented, resumable HTTP download engine.

mod engine;
mod probe;
#[cfg(test)]
mod testserver;

pub use engine::{download, download_probed, Options};
pub use probe::{build_client, filename_from_disposition, probe, Probe};
