//! The window shown when a download finishes: what was fetched, from where, where it was saved, and
//! buttons to open the file, open it with another program, or open its folder.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use zdm_proto::TaskInfo;

use crate::{fmt, prefs, Ui};

pub fn show(ui: &Rc<Ui>, t: &TaskInfo) {
    let dest = t.dest.clone().unwrap_or_default();
    let name = std::path::Path::new(&dest).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| t.url.clone());
    let size = t.size.unwrap_or(t.downloaded);

    let heading = gtk::Label::builder().label("Download complete").xalign(0.0).css_classes(["title-3"]).build();
    let detail = gtk::Label::builder().label(format!("{name}\nDownloaded {} ({size} bytes)", fmt::bytes(size))).xalign(0.0).wrap(true).selectable(true).build();
    let icon = gtk::Image::builder().icon_name("emblem-ok-symbolic").pixel_size(48).build();
    icon.add_css_class("success");
    let top = gtk::Box::builder().spacing(14).build();
    let texts = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).build();
    texts.append(&heading);
    texts.append(&detail);
    top.append(&icon);
    top.append(&texts);

    let field = |caption: &str, value: &str| {
        let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(3).build();
        b.append(&gtk::Label::builder().label(caption).xalign(0.0).css_classes(["dim-label", "caption"]).build());
        let e = gtk::Entry::builder().text(value).editable(false).hexpand(true).build();
        b.append(&e);
        b
    };

    let open = gtk::Button::with_label("Open");
    let open_with = gtk::Button::with_label("Open with…");
    let folder = gtk::Button::with_label("Open folder");
    let close = gtk::Button::with_label("Close");
    open.add_css_class("suggested-action");
    let buttons = gtk::Box::builder().spacing(8).halign(gtk::Align::End).build();
    for b in [&open, &open_with, &folder, &close] {
        buttons.append(b);
    }
    let never = gtk::CheckButton::with_label("Don't show this window again");

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(16).margin_bottom(14).margin_start(18).margin_end(18).build();
    content.append(&top);
    content.append(&field("Address", &t.url));
    content.append(&field("The file was saved as", &dest));
    content.append(&buttons);
    content.append(&never);

    let header = adw::HeaderBar::new();
    header.add_css_class("ldm-header");
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&content));
    let win = adw::Window::builder().transient_for(&ui.window).title("Download complete").default_width(620).content(&tv).build();
    win.add_css_class("ldm-window");
    if !ui.window.is_visible() {
        win.set_transient_for(None::<&gtk::Window>);
    }

    let file = gio::File::for_path(&dest);
    {
        let (w, f) = (win.clone(), file.clone());
        open.connect_clicked(move |_| {
            gtk::FileLauncher::new(Some(&f)).launch(Some(&w), gio::Cancellable::NONE, |_| {});
        });
    }
    {
        let (w, f) = (win.clone(), file.clone());
        open_with.connect_clicked(move |_| {
            let l = gtk::FileLauncher::new(Some(&f));
            l.set_always_ask(true);
            l.launch(Some(&w), gio::Cancellable::NONE, |_| {});
        });
    }
    {
        let (w, f) = (win.clone(), file.clone());
        folder.connect_clicked(move |_| {
            gtk::FileLauncher::new(Some(&f)).open_containing_folder(Some(&w), gio::Cancellable::NONE, |_| {});
        });
    }
    {
        let w = win.clone();
        close.connect_clicked(move |_| w.close());
    }
    {
        let ui = ui.clone();
        never.connect_toggled(move |c| {
            let mut p = ui.prefs.borrow_mut();
            p.show_complete = !c.is_active();
            prefs::save(&p);
        });
    }
    win.present();
    let _ = glib::MainContext::default();
}
