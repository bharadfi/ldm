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
    /// Closing the window hides it to the tray (only when a tray is available).
    #[serde(default = "yes")]
    pub close_to_tray: bool,
    /// Desktop notification when a download finishes or fails.
    #[serde(default = "yes")]
    pub notifications: bool,
    /// Offer to download a link when one is copied (off by default).
    #[serde(default)]
    pub monitor_clipboard: bool,
}

impl Default for GuiPrefs {
    fn default() -> Self {
        GuiPrefs { auto_progress: true, close_to_tray: true, notifications: true, monitor_clipboard: false }
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

fn autostart_path() -> PathBuf {
    glib::user_config_dir().join("autostart").join("org.linuxdm.Manager.desktop")
}

pub fn autostart_enabled() -> bool {
    autostart_path().exists()
}

/// Content of the XDG autostart entry: start hidden in the tray when you log in.
pub fn autostart_entry(exe: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName=Linux Download Manager\nExec={exe} --background\nIcon=org.linuxdm.Manager\nX-GNOME-Autostart-enabled=true\nTerminal=false\n")
}

pub fn set_autostart(on: bool) -> std::io::Result<()> {
    let path = autostart_path();
    if !on {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, autostart_entry(&exe.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_use_defaults() {
        assert!(serde_json::from_str::<GuiPrefs>("{}").unwrap().auto_progress);
        assert!(!serde_json::from_str::<GuiPrefs>(r#"{"auto_progress":false}"#).unwrap().auto_progress);
        let p = serde_json::from_str::<GuiPrefs>(r#"{"auto_progress":false}"#).unwrap();
        assert!(p.close_to_tray && p.notifications && !p.monitor_clipboard);
    }

    #[test]
    fn autostart_entry_starts_in_background() {
        let e = autostart_entry("/opt/ldm/zdm-gui");
        assert!(e.contains("Exec=/opt/ldm/zdm-gui --background\n") && e.starts_with("[Desktop Entry]"));
    }
}
