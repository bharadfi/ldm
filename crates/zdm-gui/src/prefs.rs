//! Window-only preferences (the daemon keeps its own settings): `~/.config/linux-download-manager/gui.json`.

use std::path::PathBuf;

use gtk::glib;
use serde::{Deserialize, Serialize};

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GuiPrefs {
    /// Open the progress window when a new download starts.
    #[serde(default = "yes")]
    pub auto_progress: bool,
}

impl Default for GuiPrefs {
    fn default() -> Self {
        GuiPrefs { auto_progress: true }
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("linux-download-manager").join("gui.json")
}

pub fn load() -> GuiPrefs {
    std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(p: &GuiPrefs) {
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, serde_json::to_vec_pretty(p).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_use_defaults() {
        assert!(serde_json::from_str::<GuiPrefs>("{}").unwrap().auto_progress);
        assert!(!serde_json::from_str::<GuiPrefs>(r#"{"auto_progress":false}"#).unwrap().auto_progress);
    }
}
