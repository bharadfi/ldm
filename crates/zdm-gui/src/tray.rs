//! Tray icon (StatusNotifierItem, shown by GNOME's AppIndicator support, KDE, XFCE...) and its menu.
//! The tray runs on its own thread; clicks are forwarded to the GTK main loop through a channel.

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};
use ksni::ToolTip;

use crate::{APP_ID, APP_NAME};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCmd {
    Show,
    PauseAll,
    ResumeAll,
    Quit,
}

pub struct LdmTray {
    tx: async_channel::Sender<TrayCmd>,
    pub tooltip: String,
}

impl LdmTray {
    fn item(label: &str, icon: &str, cmd: TrayCmd) -> MenuItem<Self> {
        StandardItem { label: label.into(), icon_name: icon.into(), activate: Box::new(move |t: &mut Self| { let _ = t.tx.send_blocking(cmd); }), ..Default::default() }.into()
    }
}

impl ksni::Tray for LdmTray {
    fn id(&self) -> String {
        APP_ID.into()
    }
    fn title(&self) -> String {
        APP_NAME.into()
    }
    fn icon_name(&self) -> String {
        APP_ID.into()
    }
    fn icon_theme_path(&self) -> String {
        gtk::glib::user_data_dir().join("icons").to_string_lossy().into_owned()
    }
    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: APP_NAME.into(), description: self.tooltip.clone(), ..Default::default() }
    }
    /// Left click.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send_blocking(TrayCmd::Show);
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            Self::item("Open Linux Download Manager", "folder-download", TrayCmd::Show),
            MenuItem::Separator,
            Self::item("Resume all", "media-playback-start", TrayCmd::ResumeAll),
            Self::item("Pause all", "media-playback-pause", TrayCmd::PauseAll),
            MenuItem::Separator,
            Self::item("Quit", "application-exit", TrayCmd::Quit),
        ]
    }
}

/// Start the tray. `None` when the desktop has no tray host (then the window must not hide itself).
pub fn spawn(tx: async_channel::Sender<TrayCmd>) -> Option<Handle<LdmTray>> {
    LdmTray { tx, tooltip: "Idle".into() }.spawn().ok()
}

pub fn set_tooltip(h: &Handle<LdmTray>, text: &str) {
    let text = text.to_string();
    let _ = h.update(move |t| t.tooltip = text);
}
