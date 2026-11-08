//! Site grabber window: explore a page/site for files, tick the ones you want, download them.

use std::rc::Rc;

use adw::prelude::*;
use zdm_proto::{AddRequest, GrabLink, GrabRequest, Request};

use crate::helpers::{preset_extensions, GRAB_PRESETS};
use crate::{send, Ui};

const C_ON: u32 = 0;
const C_NAME: u32 = 1;
const C_EXT: u32 = 2;
const C_URL: u32 = 3;
const C_FROM: u32 = 4;

pub struct GrabWin {
    win: adw::Window,
    store: gtk::ListStore,
    status: gtk::Label,
    spinner: gtk::Spinner,
    explore: gtk::Button,
    download: gtk::Button,
}

pub fn open(ui: &Rc<Ui>) {
    if let Some(g) = &*ui.grab.borrow() {
        g.win.present();
        return;
    }
    let url = adw::EntryRow::builder().title("Web page address (https://…)").build();
    let depth = adw::SpinRow::new(Some(&gtk::Adjustment::new(0.0, 0.0, 5.0, 1.0, 1.0, 0.0)), 1.0, 0);
    depth.set_title("Follow links this many pages deep (0 = only this page)");
    let same = adw::SwitchRow::builder().title("Stay on the same site").active(true).build();
    let types = adw::ComboRow::builder().title("File types").model(&gtk::StringList::new(&GRAB_PRESETS)).build();
    let custom = adw::EntryRow::builder().title("Extensions, for example: pdf zip mp3").sensitive(false).build();
    types.connect_selected_notify({
        let custom = custom.clone();
        move |c| custom.set_sensitive(c.selected() as usize == GRAB_PRESETS.len() - 1)
    });
    let include = adw::EntryRow::builder().title("Only addresses matching (optional), for example */2024/* or .iso").build();
    let exclude = adw::EntryRow::builder().title("Skip addresses matching (optional), for example thumb *-old.*").build();
    let group = adw::PreferencesGroup::new();
    for r in [url.upcast_ref::<gtk::Widget>(), depth.upcast_ref(), same.upcast_ref(), types.upcast_ref(), custom.upcast_ref(), include.upcast_ref(), exclude.upcast_ref()] {
        group.add(r);
    }

    let explore = gtk::Button::builder().label("Explore").css_classes(["suggested-action"]).valign(gtk::Align::Center).build();
    let spinner = gtk::Spinner::new();
    let status = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["dim-label"]).label("Enter an address and press Explore.").build();
    let bar = gtk::Box::builder().spacing(8).build();
    bar.append(&spinner);
    bar.append(&status);
    bar.append(&explore);

    let store = gtk::ListStore::new(&[bool::static_type(), String::static_type(), String::static_type(), String::static_type(), String::static_type()]);
    let table = gtk::TreeView::with_model(&store);
    table.set_tooltip_column(C_URL as i32);
    let toggle = gtk::CellRendererToggle::new();
    {
        let store = store.clone();
        toggle.connect_toggled(move |_, path| {
            if let Some(it) = store.iter(&path) {
                let on = store.get::<bool>(&it, C_ON as i32);
                store.set_value(&it, C_ON, &(!on).to_value());
            }
        });
    }
    let col = gtk::TreeViewColumn::builder().title("Get").build();
    col.pack_start(&toggle, false);
    col.add_attribute(&toggle, "active", C_ON as i32);
    table.append_column(&col);
    for (title, c, width) in [("File name", C_NAME, 300), ("Type", C_EXT, 70), ("Found on", C_FROM, 280)] {
        let cell = gtk::CellRendererText::new();
        cell.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let col = gtk::TreeViewColumn::builder().title(title).resizable(true).sizing(gtk::TreeViewColumnSizing::Fixed).fixed_width(width).build();
        col.pack_start(&cell, true);
        col.add_attribute(&cell, "text", c as i32);
        table.append_column(&col);
    }
    let scroll = gtk::ScrolledWindow::builder().vexpand(true).min_content_height(220).has_frame(true).child(&table).build();

    let all = gtk::Button::with_label("Select all");
    let none = gtk::Button::with_label("Select none");
    let download = gtk::Button::builder().label("Download selected").css_classes(["suggested-action"]).sensitive(false).build();
    let foot = gtk::Box::builder().spacing(8).build();
    foot.append(&all);
    foot.append(&none);
    foot.append(&gtk::Box::builder().hexpand(true).build());
    foot.append(&download);

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).margin_top(12).margin_bottom(12).margin_start(14).margin_end(14).build();
    content.append(&group);
    content.append(&bar);
    content.append(&scroll);
    content.append(&foot);
    let header = adw::HeaderBar::new();
    header.add_css_class("ldm-header");
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&content));
    let win = adw::Window::builder().transient_for(&ui.window).title("Site grabber").default_width(820).default_height(640).content(&tv).build();
    win.add_css_class("ldm-window");

    let set_all = |store: &gtk::ListStore, on: bool| {
        if let Some(it) = store.iter_first() {
            loop {
                store.set_value(&it, C_ON, &on.to_value());
                if !store.iter_next(&it) {
                    break;
                }
            }
        }
    };
    {
        let s = store.clone();
        all.connect_clicked(move |_| set_all(&s, true));
        let s = store.clone();
        none.connect_clicked(move |_| set_all(&s, false));
    }
    {
        let (u, store, status, spinner, explore2) = (ui.clone(), store.clone(), status.clone(), spinner.clone(), explore.clone());
        let download = download.clone();
        explore.connect_clicked(move |_| {
            let address = url.text().trim().to_string();
            let address = if address.contains("://") { address } else { format!("https://{address}") };
            if address.len() < 9 {
                status.set_text("Type the address of a web page first.");
                return;
            }
            store.clear();
            download.set_sensitive(false);
            status.set_text("Exploring… this can take a while for deep searches.");
            spinner.start();
            explore2.set_sensitive(false);
            send(&u, Request::Grab(GrabRequest { url: address, depth: depth.value() as u32, same_host: same.is_active(), extensions: preset_extensions(types.selected() as usize, &custom.text()), max_pages: 0, include: crate::helpers::split_patterns(&include.text()), exclude: crate::helpers::split_patterns(&exclude.text()) }));
        });
    }
    {
        let (u, store, win2) = (ui.clone(), store.clone(), win.clone());
        download.connect_clicked(move |_| {
            let mut n = 0;
            if let Some(it) = store.iter_first() {
                loop {
                    if store.get::<bool>(&it, C_ON as i32) {
                        send(&u, Request::Add(AddRequest { url: store.get::<String>(&it, C_URL as i32), referer: Some(store.get::<String>(&it, C_FROM as i32)), start: true, ..Default::default() }));
                        n += 1;
                    }
                    if !store.iter_next(&it) {
                        break;
                    }
                }
            }
            u.toasts.add_toast(adw::Toast::new(&format!("Added {n} download(s) from the grabber")));
            win2.close();
        });
    }
    let u = ui.clone();
    win.connect_close_request(move |_| {
        *u.grab.borrow_mut() = None;
        gtk::glib::Propagation::Proceed
    });
    let gw = Rc::new(GrabWin { win, store, status, spinner, explore, download });
    *ui.grab.borrow_mut() = Some(gw.clone());
    gw.win.present();
}

pub fn show_results(ui: &Ui, links: Vec<GrabLink>) {
    let Some(g) = &*ui.grab.borrow() else { return };
    g.store.clear();
    for l in &links {
        g.store.insert_with_values(None, &[(C_ON, &true), (C_NAME, &l.name), (C_EXT, &l.ext), (C_URL, &l.url), (C_FROM, &l.from)]);
    }
    g.spinner.stop();
    g.explore.set_sensitive(true);
    g.download.set_sensitive(!links.is_empty());
    g.status.set_text(&if links.is_empty() { "Nothing found. Try another file type or a deeper search.".to_string() } else { format!("Found {} file(s). Untick what you do not want.", links.len()) });
}

pub fn show_error(ui: &Ui, msg: &str) {
    if let Some(g) = &*ui.grab.borrow() {
        g.spinner.stop();
        g.explore.set_sensitive(true);
        g.status.set_text(&format!("Could not explore: {msg}"));
    }
}

/// Rows currently listed in the results (self-test hook).
pub fn result_count(ui: &Ui) -> usize {
    ui.grab.borrow().as_ref().map_or(0, |g| g.store.iter_n_children(None) as usize)
}
