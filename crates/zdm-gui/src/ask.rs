//! "Download File Info": shown when the browser hands over a download. Lets the user pick the category and the
//! save location, then start it, keep it for later, or cancel it.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use zdm_proto::{PreviewInfo, Request, TaskInfo, CATEGORY_TABLE, OTHER_CATEGORY};

use crate::{fmt, send, show_progress, Ui};

pub struct AskWin {
    win: adw::Window,
    id: i64,
    category: gtk::DropDown,
    save_as: gtk::Entry,
    size: gtk::Label,
    icon: gtk::Image,
    remember: gtk::CheckButton,
    remember_dir: gtk::Label,
    names: Vec<String>,
    root: String,
    settings_dirs: std::collections::BTreeMap<String, String>,
    acted: Cell<bool>,
    filename: RefCell<String>,
}

fn category_names() -> Vec<String> {
    CATEGORY_TABLE.iter().map(|(c, _)| c.to_string()).chain(std::iter::once(OTHER_CATEGORY.to_string())).collect()
}

fn dir_for(root: &str, dirs: &std::collections::BTreeMap<String, String>, cat: &str) -> String {
    dirs.get(cat).cloned().unwrap_or_else(|| PathBuf::from(root).join(cat).to_string_lossy().into_owned())
}

pub fn open(ui: &Rc<Ui>, t: &TaskInfo) {
    let names = category_names();
    let (root, dirs) = ui.settings.borrow().as_ref().map(|s| (s.download_root.clone(), s.category_dirs.clone())).unwrap_or_default();

    let url = gtk::Entry::builder().text(&t.url).hexpand(true).build();
    let category = gtk::DropDown::from_strings(&names.iter().map(String::as_str).collect::<Vec<_>>());
    category.set_selected(names.len() as u32 - 1);
    let save_as = gtk::Entry::builder().hexpand(true).build();
    let browse = gtk::Button::with_label("…");
    let remember = gtk::CheckButton::builder().active(true).build();
    let remember_dir = gtk::Label::builder().xalign(0.0).css_classes(["dim-label"]).ellipsize(gtk::pango::EllipsizeMode::Start).build();
    let size = gtk::Label::builder().label("Looking up…").css_classes(["dim-label"]).build();
    let icon = gtk::Image::builder().icon_name("text-x-generic").pixel_size(48).build();

    let grid = gtk::Grid::builder().row_spacing(10).column_spacing(12).hexpand(true).build();
    let label = |s: &str| gtk::Label::builder().label(s).xalign(1.0).build();
    grid.attach(&label("URL"), 0, 0, 1, 1);
    grid.attach(&url, 1, 0, 2, 1);
    grid.attach(&label("Category"), 0, 1, 1, 1);
    grid.attach(&category, 1, 1, 2, 1);
    grid.attach(&label("Save as"), 0, 2, 1, 1);
    grid.attach(&save_as, 1, 2, 1, 1);
    grid.attach(&browse, 2, 2, 1, 1);
    let remember_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_start(4).margin_end(4).margin_top(4).margin_bottom(4).build();
    remember_box.append(&remember);
    remember_box.append(&remember_dir);
    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&remember_box));
    grid.attach(&frame, 1, 3, 2, 1);

    let side = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).valign(gtk::Align::Start).margin_start(8).build();
    side.append(&icon);
    side.append(&size);
    let body = gtk::Box::builder().spacing(10).build();
    body.append(&grid);
    body.append(&side);

    let later = gtk::Button::with_label("Download later");
    let start = gtk::Button::with_label("Start download");
    let cancel = gtk::Button::with_label("Cancel");
    start.add_css_class("suggested-action");
    let buttons = gtk::Box::builder().spacing(10).homogeneous(true).build();
    for b in [&later, &start, &cancel] {
        buttons.append(b);
    }
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(16).margin_bottom(14).margin_start(18).margin_end(18).build();
    content.append(&body);
    content.append(&buttons);
    let header = adw::HeaderBar::new();
    header.add_css_class("ldm-header");
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&content));
    let win = adw::Window::builder().application(&ui.window.application().expect("application")).title("Download file info").default_width(700).content(&tv).build();
    win.add_css_class("ldm-window");

    let name0 = t.filename.clone().unwrap_or_default();
    let aw = Rc::new(AskWin { win, id: t.id, category, save_as, size, icon, remember, remember_dir, names, root, settings_dirs: dirs, acted: Cell::new(false), filename: RefCell::new(name0) });
    aw.refresh_path();
    ui.asks.borrow_mut().insert(t.id, aw.clone());
    send(ui, Request::Preview { id: t.id });

    // category -> that category's folder
    {
        let a = aw.clone();
        aw.category.connect_selected_notify(move |_| a.refresh_path());
    }
    {
        let a = aw.clone();
        aw.save_as.connect_changed(move |e| {
            let p = PathBuf::from(e.text().as_str());
            if let Some(n) = p.file_name() {
                *a.filename.borrow_mut() = n.to_string_lossy().into_owned();
            }
            a.remember_dir.set_text(&p.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default());
        });
    }
    {
        let a = aw.clone();
        browse.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder().title("Save as").build();
            let current = PathBuf::from(a.save_as.text().as_str());
            dialog.set_initial_file(Some(&gio::File::for_path(&current)));
            let a2 = a.clone();
            dialog.save(Some(&a.win), gio::Cancellable::NONE, move |r| {
                if let Ok(f) = r {
                    if let Some(p) = f.path() {
                        a2.save_as.set_text(&p.to_string_lossy());
                    }
                }
            });
        });
    }
    let finish = {
        let (ui, a, url) = (ui.clone(), aw.clone(), url.clone());
        Rc::new(move |go: bool| {
            let path = PathBuf::from(a.save_as.text().as_str());
            let (Some(dir), Some(name)) = (path.parent().filter(|d| d.is_absolute()), path.file_name()) else {
                a.size.set_text("Type a full file path.");
                return;
            };
            a.acted.set(true);
            let new_url = url.text().to_string();
            if new_url != ui.asks_url(a.id) {
                send(&ui, Request::SetUrl { id: a.id, url: new_url });
            }
            send(&ui, Request::SetTarget { id: a.id, filename: Some(name.to_string_lossy().into_owned()), dest_dir: Some(dir.to_string_lossy().into_owned()) });
            if a.remember.is_active() {
                let cat = a.names[a.category.selected() as usize].clone();
                if let Some(mut s) = ui.settings.borrow().clone() {
                    if s.category_dirs.get(&cat).map(String::as_str) != Some(&*dir.to_string_lossy()) {
                        s.category_dirs.insert(cat, dir.to_string_lossy().into_owned());
                        send(&ui, Request::SetSettings(s));
                    }
                }
            }
            if go {
                send(&ui, Request::Resume { id: a.id });
                if ui.prefs.borrow().auto_progress {
                    show_progress(&ui, a.id);
                }
            } else {
                send(&ui, Request::Pause { id: a.id });
            }
            a.win.close();
        })
    };
    {
        let f = finish.clone();
        start.connect_clicked(move |_| f(true));
    }
    {
        let f = finish.clone();
        later.connect_clicked(move |_| f(false));
    }
    {
        let (ui, a) = (ui.clone(), aw.clone());
        cancel.connect_clicked(move |_| {
            a.acted.set(true);
            send(&ui, Request::Remove { id: a.id, delete_files: false });
            a.win.close();
        });
    }
    {
        // closing the window with the corner button keeps the download for later
        let (ui, a) = (ui.clone(), aw.clone());
        aw.win.connect_close_request(move |_| {
            if !a.acted.replace(true) {
                send(&ui, Request::Pause { id: a.id });
            }
            ui.asks.borrow_mut().remove(&a.id);
            gtk::glib::Propagation::Proceed
        });
    }
    aw.win.present();
}

impl AskWin {
    /// Show `<folder of the chosen category>/<file name>`.
    fn refresh_path(&self) {
        let cat = &self.names[self.category.selected() as usize];
        let dir = dir_for(&self.root, &self.settings_dirs, cat);
        let name = self.filename.borrow().clone();
        self.save_as.set_text(&Path::new(&dir).join(if name.is_empty() { "download" } else { &name }).to_string_lossy());
        self.remember.set_label(Some(&format!("Remember this folder for the “{cat}” category")));
    }

    /// The server's answer about name, size and category.
    pub fn fill(&self, p: &PreviewInfo) {
        *self.filename.borrow_mut() = p.filename.clone();
        if let Some(i) = self.names.iter().position(|n| *n == p.category) {
            self.category.set_selected(i as u32);
        }
        self.save_as.set_text(&Path::new(&p.dir).join(&p.filename).to_string_lossy());
        self.size.set_text(&p.size.map(fmt::bytes).unwrap_or_else(|| "size unknown".into()));
        let (ct, _) = gio::content_type_guess(Some(&p.filename), &[]);
        self.icon.set_from_gicon(&gio::content_type_get_icon(&ct));
        self.remember.set_label(Some(&format!("Remember this folder for the “{}” category", p.category)));
    }

    pub fn failed(&self, why: &str) {
        self.size.set_text(&format!("No answer: {why}"));
    }
}
