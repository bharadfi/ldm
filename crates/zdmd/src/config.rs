use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    /// SQLite database file.
    pub db_path: PathBuf,
    /// Parent of the per-category download folders.
    pub download_root: PathBuf,
    /// Connections per download.
    pub connections: usize,
    /// Per-download rate cap in bytes/second.
    pub limit_bps: Option<u64>,
    /// Scheduler wake-up interval.
    pub tick: std::time::Duration,
    /// Pause before the first automatic retry of a download that failed on the network (grows with each try).
    pub retry_delay: std::time::Duration,
    /// How long everything must stay idle before the "after all downloads" action runs (time to cancel it).
    pub after_grace: std::time::Duration,
}

impl Config {
    /// Defaults for the current user: data in the XDG data dir, downloads in `~/Downloads/ZDM`.
    /// `ZDM_DATA_DIR` and `ZDM_DOWNLOAD_DIR` override them.
    pub fn from_env() -> Self {
        let data = std::env::var_os("ZDM_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("zdm"));
        let root = std::env::var_os("ZDM_DOWNLOAD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")).join("ZDM"));
        Self { db_path: data.join("zdm.db"), download_root: root, connections: 8, limit_bps: None, tick: std::time::Duration::from_secs(1), retry_delay: std::time::Duration::from_secs(20), after_grace: std::time::Duration::from_secs(30) }
    }
}
