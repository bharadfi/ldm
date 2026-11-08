//! "Download video from a web page": paste a page address, see the formats (resolutions, audio, subtitles) that yt-dlp
//! finds, pick one. The same choices the browser button offers.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use zdm_proto::{AddRequest, FormatList, Request};

use crate::helpers::split_patterns;
use crate::{send, Ui};

const C_LABEL: u32 = 0;
const C_SIZE: u32 = 1;
const C_ID: u32 = 2;
const C_NAME: u32 = 3;
const C_KIND: u32 = 4;

pub struct VideoWin {
    win: adw::Window,
    store: gtk::ListStore,
    status: gtk::Label,
    spinner: gtk::Spinner,
    look_up: gtk::Button,
    download: gtk::Button,
    url: RefCell<String>,
}

fn size_text(bytes: Option<u64>) -> String {
    match bytes {
        Some(b) if b >= 1_000_000_000 => format!("{:.1} GB", b as f64 / 1e9),
        Some(b) if b >= 1_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        Some(b) if b > 0 => format!("{} KB", b / 1000),
        _ => String::new(),
    }
}

pub fn open(ui: &Rc<Ui>, prefill: Option<&str>) {
    if let Some(v) = &*ui.video.borrow() {
        v.win.present();
        return;
    }
    let entry = gtk::Entry::builder().placeholder_text("https://www.youtube.com/watch?v=…  (any page with a video)").hexpand(true).activates_default(true).build();
    if let Some(p) = prefill {
        entry.set_text(p);
    }
    let look_up = gtk::Button::builder().label("Look up").css_classes(["suggested-action"]).build();
    let top = gtk::Box::builder().spacing(6).build();
    top.append(&entry);
    top.append(&look_up);

    let spinner = gtk::Spinner::new();
    let status = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["dim-label"]).wrap(true).label("Paste the address of a page with a video and press Look up. This uses yt-dlp, which must be installed.").build();
    let status_row = gtk::Box::builder().spacing(8).build();
    status_row.append(&spinner);
    status_row.append(&status);

    let store = gtk::ListStore::new(&[String::static_type(), String::static_type(), String::static_type(), String::static_type(), String::static_type()]);
    let table = gtk::TreeView::with_model(&store);
    table.add_css_class("ldm-table");
    for (title, c, width, right) in [("What to download", C_LABEL, 360, false), ("Size", C_SIZE, 90, true)] {
        let cell = gtk::CellRendererText::new();
        cell.set_padding(4, 1);
        if right {
            cell.set_xalign(1.0);
        }
        let col = gtk::TreeViewColumn::builder().title(title).resizable(true).sizing(gtk::TreeViewColumnSizing::Fixed).fixed_width(width).build();
        col.pack_start(&cell, true);
        col.add_attribute(&cell, "text", c as i32);
        table.append_column(&col);
    }
    let scroll = gtk::ScrolledWindow::builder().vexpand(true).min_content_height(260).has_frame(true).child(&table).build();

    let download = gtk::Button::builder().label("Download selected").css_classes(["suggested-action"]).sensitive(false).build();
    let foot = gtk::Box::builder().spacing(8).halign(gtk::Align::End).build();
    foot.append(&download);

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).margin_top(10).margin_bottom(10).margin_start(12).margin_end(12).build();
    for w in [top.upcast_ref::<gtk::Widget>(), status_row.upcast_ref(), scroll.upcast_ref(), foot.upcast_ref()] {
        content.append(w);
    }
    let header = adw::HeaderBar::new();
    header.add_css_class("ldm-header");
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&content));
    let win = adw::Window::builder().transient_for(&ui.window).title("Download video from a web page").default_width(560).default_height(460).content(&tv).css_classes(["ldm-window"]).build();

    {
        let (u, status, spinner, look_up2, download, store) = (ui.clone(), status.clone(), spinner.clone(), look_up.clone(), download.clone(), store.clone());
        let entry2 = entry.clone();
        let go = move || {
            let address = entry2.text().trim().to_string();
            let address = split_patterns(&address).into_iter().next().unwrap_or_default();
            if !(address.starts_with("http://") || address.starts_with("https://")) {
                status.set_text("Type or paste the address of a web page (starting with http:// or https://).");
                return;
            }
            store.clear();
            download.set_sensitive(false);
            status.set_text("Looking for videos… this can take ten seconds or so.");
            spinner.start();
            look_up2.set_sensitive(false);
            if let Some(v) = &*u.video.borrow() {
                *v.url.borrow_mut() = address.clone();
            }
            send(&u, Request::Formats { url: address });
        };
        let go2 = go.clone();
        look_up.connect_clicked(move |_| go());
        entry.connect_activate(move |_| go2());
    }
    let pick = Rc::new({
        let (u, store, win2) = (ui.clone(), store.clone(), win.clone());
        move |path: &gtk::TreePath| {
            let Some(it) = store.iter(path) else { return };
            let id = store.get::<String>(&it, C_ID as i32);
            if id.is_empty() {
                return;
            }
            let page = u.video.borrow().as_ref().map(|v| v.url.borrow().clone()).unwrap_or_default();
            send(&u, Request::Add(AddRequest { url: page, ytdlp: Some(id), filename: Some(store.get::<String>(&it, C_NAME as i32)), start: true, ..Default::default() }));
            u.toasts.add_toast(adw::Toast::new(&format!("Added “{}”", store.get::<String>(&it, C_NAME as i32))));
            win2.close();
        }
    });
    {
        let pick2 = pick.clone();
        table.connect_row_activated(move |_, path, _| pick2(path));
        let table2 = table.clone();
        download.connect_clicked(move |_| {
            if let Some((paths, _)) = Some(table2.selection().selected_rows()) {
                if let Some(p) = paths.first() {
                    pick(p);
                }
            }
        });
    }
    {
        let (download, table2) = (download.clone(), table.clone());
        table.selection().connect_changed(move |_| download.set_sensitive(table2.selection().count_selected_rows() > 0));
    }
    let u = ui.clone();
    win.connect_close_request(move |_| {
        *u.video.borrow_mut() = None;
        gtk::glib::Propagation::Proceed
    });
    let vw = Rc::new(VideoWin { win, store, status, spinner, look_up, download, url: RefCell::new(String::new()) });
    *ui.video.borrow_mut() = Some(vw.clone());
    vw.win.present();
}

pub fn show_results(ui: &Ui, list: FormatList) {
    let Some(v) = &*ui.video.borrow() else { return };
    v.store.clear();
    let mut kind = String::new();
    for o in &list.options {
        if o.kind != kind {
            kind = o.kind.clone();
            let title = match kind.as_str() {
                "video" => "Video",
                "audio" => "Audio only",
                _ => "Subtitles",
            };
            v.store.insert_with_values(None, &[(C_LABEL, &format!("— {title} —")), (C_SIZE, &""), (C_ID, &""), (C_NAME, &""), (C_KIND, &"header")]);
        }
        v.store.insert_with_values(None, &[(C_LABEL, &format!("   {}", o.label)), (C_SIZE, &size_text(o.size)), (C_ID, &o.id), (C_NAME, &o.filename), (C_KIND, &o.kind)]);
    }
    v.spinner.stop();
    v.look_up.set_sensitive(true);
    v.status.set_text(&format!("{} — choose what to download (double-click a row).", list.title));
    let _ = &v.download;
}

pub fn show_error(ui: &Ui, msg: &str) {
    if let Some(v) = &*ui.video.borrow() {
        v.spinner.stop();
        v.look_up.set_sensitive(true);
        v.status.set_text(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size_text(None), "");
        assert_eq!(size_text(Some(0)), "");
        assert_eq!(size_text(Some(512_000)), "512 KB");
        assert_eq!(size_text(Some(14_600_000)), "14.6 MB");
        assert_eq!(size_text(Some(1_372_500_000)), "1.4 GB");
    }
}
