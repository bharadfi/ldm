//! Appearance switches of the View menu: dark mode, font size, toolbar style and which table columns show.

use std::rc::Rc;

use adw::prelude::*;

use crate::prefs;
use crate::Ui;

/// Titles of the table's columns, in order (the user can hide any of them but the first).
pub const COLUMNS: [&str; 8] = ["File name", "Queue", "Size", "Status", "Time left", "Transfer rate", "Date added", "Link"];

pub const FONT_SIZES: [(&str, u32); 3] = [("Small", 8), ("Normal", 9), ("Large", 11)];

/// Apply the saved preferences to the window.
pub fn apply(ui: &Ui) {
    let p = ui.prefs.borrow().clone();
    adw::StyleManager::default().set_color_scheme(if p.dark_mode { adw::ColorScheme::ForceDark } else { adw::ColorScheme::ForceLight });
    if p.dark_mode {
        ui.window.add_css_class("ldm-dark");
    } else {
        ui.window.remove_css_class("ldm-dark");
    }
    let pt = if FONT_SIZES.iter().any(|(_, s)| *s == p.font_pt) { p.font_pt } else { 9 };
    ui.font_provider.load_from_string(&format!("window.ldm-window {{ font-size: {pt}pt; }} window.ldm-window treeview.view {{ font-size: {pt}pt; }}"));
    for (img, label) in &ui.toolbar_parts {
        img.set_pixel_size(if p.toolbar_small { 16 } else { 22 });
        label.set_visible(p.toolbar_labels);
    }
    for col in ui.table.columns() {
        let title = col.title().to_string();
        col.set_visible(title == COLUMNS[0] || !p.hidden_columns.contains(&title));
    }
}

/// Change one preference, save it and apply it.
pub fn change(ui: &Ui, f: impl FnOnce(&mut prefs::GuiPrefs)) {
    {
        let mut p = ui.prefs.borrow_mut();
        f(&mut p);
        prefs::save(&p);
    }
    apply(ui);
}

/// "Customize list…": choose which columns of the download table are shown.
pub fn columns_dialog(ui: &Rc<Ui>) {
    let hidden = ui.prefs.borrow().hidden_columns.clone();
    let boxed = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    let checks: Vec<(String, gtk::CheckButton)> = COLUMNS
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let cb = gtk::CheckButton::builder().label(*c).active(!hidden.contains(&c.to_string())).sensitive(i != 0).build();
            boxed.append(&cb);
            (c.to_string(), cb)
        })
        .collect();
    let dialog = adw::AlertDialog::builder().heading("Customize the download list").body("Tick the columns to show.").extra_child(&boxed).build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "OK");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    let u = ui.clone();
    dialog.connect_response(None, move |_, r| {
        if r == "ok" {
            let off: Vec<String> = checks.iter().filter(|(_, cb)| !cb.is_active()).map(|(t, _)| t.clone()).collect();
            change(&u, |p| p.hidden_columns = off);
        }
    });
    dialog.present(Some(&ui.window));
}
