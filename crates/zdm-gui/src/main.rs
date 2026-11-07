//! Linux Download Manager window, laid out like a classic download manager: toolbar, category tree, table of downloads.
//!
//! The daemon is polled once a second on a background thread (`worker`); the GTK side only applies the
//! latest snapshot to the table, so nothing here blocks the UI.
//!
//! GtkTreeView is deprecated since GTK 4.10 but still supported; it keeps selection and scroll position
//! while rows are updated in place, which the newer ColumnView needs much more code for.
#![allow(deprecated)]

mod fmt;
mod view;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use view::Section;
use zdm_proto::{AddRequest, Request, Response, State, TaskInfo};

const APP_ID: &str = "org.linuxdm.Manager";
const APP_NAME: &str = "Linux Download Manager";
const LOGO_64: &[u8] = include_bytes!("../assets/logo-64.png");
const STYLE: &str = include_str!("../assets/style.css");

// Table columns.
const C_ID: u32 = 0;
const C_NAME: u32 = 1;
const C_QUEUE: u32 = 2;
const C_SIZE: u32 = 3;
const C_PCT: u32 = 4;
const C_STATUS: u32 = 5;
const C_LEFT: u32 = 6;
const C_RATE: u32 = 7;
const C_ADDED: u32 = 8;
const C_URL: u32 = 9;
const C_SIZE_N: u32 = 10;
const C_ADDED_N: u32 = 11;
const C_STATE: u32 = 12;
const C_CAT: u32 = 13;
const C_DEST: u32 = 14;

enum Event {
    Tasks(Vec<TaskInfo>),
    Queues(Vec<String>),
    Offline(String),
    Failure(String),
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        // The look is a bright classic one; do not follow a dark system theme.
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
        let css = gtk::CssProvider::new();
        css.load_from_string(STYLE);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        gtk::Window::set_default_icon_name(APP_ID);
    });
    app.connect_activate(build_ui);
    app.run()
}

/// Background thread: poll the daemon, run requests sent from the UI, start the daemon if it is missing.
fn worker(events: async_channel::Sender<Event>, mut cmds: tokio::sync::mpsc::UnboundedReceiver<Request>) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last_spawn: Option<Instant> = None;
        loop {
            let req = tokio::select! {
                _ = tick.tick() => None,
                c = cmds.recv() => match c { Some(c) => Some(c), None => return },
            };
            if let Some(req) = req {
                if let Err(e) = zdm_proto::call(&req).await {
                    let _ = events.send(Event::Failure(e.to_string())).await;
                }
            }
            match zdm_proto::call(&Request::List).await {
                Ok(Response::Tasks { tasks }) => {
                    let _ = events.send(Event::Tasks(tasks)).await;
                    if let Ok(Response::Queues { queues }) = zdm_proto::call(&Request::QueueList).await {
                        let _ = events.send(Event::Queues(queues.into_iter().map(|q| q.name).collect())).await;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    if last_spawn.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
                        last_spawn = Some(Instant::now());
                        start_daemon();
                    }
                    let _ = events.send(Event::Offline(e.to_string())).await;
                }
            }
        }
    });
}

/// Start `zdmd` (next to this executable first, then from PATH). It outlives the window on purpose.
fn start_daemon() {
    let sibling = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("zdmd")));
    let prog = sibling.filter(|p| p.exists()).unwrap_or_else(|| "zdmd".into());
    let _ = std::process::Command::new(prog)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

struct Ui {
    window: adw::ApplicationWindow,
    store: gtk::ListStore,
    table: gtk::TreeView,
    sidebar_store: gtk::TreeStore,
    sidebar: gtk::TreeView,
    queues_parent: gtk::TreeIter,
    filter: gtk::TreeModelFilter,
    section: Rc<RefCell<Section>>,
    banner: adw::Banner,
    toasts: adw::ToastOverlay,
    status: gtk::Label,
    cmd_tx: UnboundedSender<Request>,
    speeds: RefCell<HashMap<i64, (u64, Instant, f64)>>,
    queue_names: RefCell<Vec<String>>,
}

#[derive(Clone)]
struct Picked {
    id: i64,
    state: State,
    dest: Option<String>,
}

fn build_ui(app: &adw::Application) {
    let (ev_tx, ev_rx) = async_channel::unbounded::<Event>();
    let (cmd_tx, cmd_rx) = unbounded_channel::<Request>();
    std::thread::spawn(move || worker(ev_tx, cmd_rx));

    let window = adw::ApplicationWindow::builder().application(app).title(APP_NAME).default_width(1080).default_height(600).build();

    // ---- download table ----
    let store = gtk::ListStore::new(&[
        i64::static_type(),
        String::static_type(),
        String::static_type(),
        String::static_type(),
        i32::static_type(),
        String::static_type(),
        String::static_type(),
        String::static_type(),
        String::static_type(),
        String::static_type(),
        u64::static_type(),
        i64::static_type(),
        String::static_type(),
        String::static_type(),
        String::static_type(),
    ]);
    let section = Rc::new(RefCell::new(Section::All));
    let filter = gtk::TreeModelFilter::new(&store, None);
    {
        let section = section.clone();
        filter.set_visible_func(move |m, it| {
            // A row can be visited before its values are set (None): show it, it is re-filtered on change.
            let text = |c: u32| m.get::<Option<String>>(it, c as i32).unwrap_or_default();
            let state = State::parse(&text(C_STATE)).unwrap_or(State::Queued);
            section.borrow().matches(state, &text(C_CAT), &text(C_QUEUE))
        });
    }
    let sorted = gtk::TreeModelSort::with_model(&filter);
    sorted.set_sort_column_id(gtk::SortColumn::Index(C_ADDED_N), gtk::SortType::Descending);
    let table = gtk::TreeView::with_model(&sorted);
    table.add_css_class("ldm-table");
    table.set_headers_clickable(true);
    table.set_enable_search(true);
    table.set_search_column(C_NAME as i32);
    table.set_rubber_banding(true);
    table.selection().set_mode(gtk::SelectionMode::Multiple);

    let add_text = |title: &str, col: u32, width: i32, sort: u32, right: bool, expand: bool| {
        let cell = gtk::CellRendererText::new();
        cell.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        if right {
            cell.set_xalign(1.0);
        }
        let c = gtk::TreeViewColumn::builder().title(title).resizable(true).sizing(gtk::TreeViewColumnSizing::Fixed).fixed_width(width).expand(expand).sort_column_id(sort as i32).build();
        c.pack_start(&cell, true);
        c.add_attribute(&cell, "text", col as i32);
        table.append_column(&c);
    };
    add_text("File name", C_NAME, 280, C_NAME, false, true);
    add_text("Queue", C_QUEUE, 70, C_QUEUE, false, false);
    add_text("Size", C_SIZE, 90, C_SIZE_N, true, false);
    {
        let cell = gtk::CellRendererProgress::new();
        let c = gtk::TreeViewColumn::builder().title("Status").resizable(true).sizing(gtk::TreeViewColumnSizing::Fixed).fixed_width(150).sort_column_id(C_PCT as i32).build();
        c.pack_start(&cell, true);
        c.add_attribute(&cell, "value", C_PCT as i32);
        c.add_attribute(&cell, "text", C_STATUS as i32);
        table.append_column(&c);
    }
    add_text("Time left", C_LEFT, 100, C_LEFT, true, false);
    add_text("Transfer rate", C_RATE, 100, C_RATE, true, false);
    add_text("Date added", C_ADDED, 130, C_ADDED_N, false, false);
    add_text("Link", C_URL, 240, C_URL, false, false);
    let table_scroll = gtk::ScrolledWindow::builder().vexpand(true).hexpand(true).child(&table).build();

    // ---- category tree ----
    let sidebar_store = gtk::TreeStore::new(&[String::static_type(), String::static_type(), String::static_type()]);
    let row = |parent: Option<&gtk::TreeIter>, label: &str, icon: &str, key: &str| {
        let it = sidebar_store.append(parent);
        sidebar_store.set(&it, &[(0, &label), (1, &icon), (2, &key)]);
        it
    };
    let all = row(None, "All Downloads", "folder-download", "all");
    for c in view::CATEGORIES {
        row(Some(&all), c, view::category_icon(c), &format!("cat:{c}"));
    }
    row(None, "Unfinished", "appointment-soon", "unfinished");
    row(None, "Finished", "emblem-default", "finished");
    let queues_parent = row(None, "Queues", "view-list-compact", "queues");
    let sidebar = gtk::TreeView::with_model(&sidebar_store);
    sidebar.set_headers_visible(false);
    {
        let col = gtk::TreeViewColumn::new();
        let icon = gtk::CellRendererPixbuf::new();
        let text = gtk::CellRendererText::new();
        col.pack_start(&icon, false);
        col.pack_start(&text, true);
        col.add_attribute(&icon, "icon-name", 1);
        col.add_attribute(&text, "text", 0);
        sidebar.append_column(&col);
    }
    sidebar.expand_all();
    if let Some(first) = sidebar_store.iter_first() {
        sidebar.selection().select_iter(&first);
    }
    sidebar.add_css_class("ldm-sidebar");
    let sidebar_scroll = gtk::ScrolledWindow::builder().vexpand(true).width_request(190).child(&sidebar).build();

    let paned = gtk::Paned::builder().start_child(&sidebar_scroll).end_child(&table_scroll).position(200).shrink_start_child(false).resize_start_child(false).build();

    // ---- chrome ----
    let status = gtk::Label::builder().xalign(0.0).margin_start(10).margin_end(10).margin_top(4).margin_bottom(4).css_classes(["ldm-statusbar"]).build();
    let banner = adw::Banner::builder().title("Starting the download service…").build();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&paned));

    let toolbar_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).css_classes(["ldm-toolbar"]).build();
    let make_btn = |icon: &str, label: &str, tip: &str, class: &str| {
        let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        let img = gtk::Image::from_icon_name(icon);
        img.set_pixel_size(28);
        b.append(&img);
        b.append(&gtk::Label::new(Some(label)));
        let btn = gtk::Button::builder().child(&b).css_classes(["flat", class]).tooltip_text(tip).build();
        toolbar_box.append(&btn);
        btn
    };
    let b_add = make_btn("list-add-symbolic", "Add URL", "Add a download link", "tb-add");
    let b_resume = make_btn("media-playback-start-symbolic", "Resume", "Resume the selected downloads", "tb-resume");
    let b_pause = make_btn("media-playback-pause-symbolic", "Pause", "Pause the selected downloads", "tb-pause");
    let b_pause_all = make_btn("media-playback-stop-symbolic", "Pause All", "Pause every running download", "tb-pauseall");
    toolbar_box.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    let b_delete = make_btn("user-trash-symbolic", "Delete", "Remove the selected downloads", "tb-delete");
    let b_delete_done = make_btn("edit-clear-all-symbolic", "Delete Completed", "Remove all finished downloads from the list", "tb-done");
    toolbar_box.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    let b_folder = make_btn("folder-open-symbolic", "Open Folder", "Show the selected file in its folder", "tb-folder");

    let tv = adw::ToolbarView::new();
    let top = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let header = adw::HeaderBar::new();
    header.add_css_class("ldm-header");
    {
        let logo = gtk::Image::from_paintable(gtk::gdk::Texture::from_bytes(&glib::Bytes::from_static(LOGO_64)).ok().as_ref());
        logo.set_pixel_size(26);
        let name = gtk::Label::builder().label(APP_NAME).css_classes(["title"]).build();
        let title = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        title.append(&logo);
        title.append(&name);
        header.set_title_widget(Some(&title));
    }
    top.append(&header);
    top.append(&toolbar_box);
    top.append(&banner);
    tv.add_top_bar(&top);
    tv.set_content(Some(&toasts));
    tv.add_bottom_bar(&status);
    window.set_content(Some(&tv));

    let (queues_parent_it, filter_c) = (queues_parent, filter.clone());
    let ui = Rc::new(Ui {
        window: window.clone(),
        store,
        table: table.clone(),
        sidebar_store,
        sidebar: sidebar.clone(),
        queues_parent: queues_parent_it,
        filter: filter_c,
        section,
        banner,
        toasts,
        status,
        cmd_tx,
        speeds: RefCell::default(),
        queue_names: RefCell::default(),
    });
    banner_hide(&ui);

    // sidebar selection -> filter
    {
        let ui2 = ui.clone();
        sidebar.selection().connect_changed(move |sel| {
            if let Some((m, it)) = sel.selected() {
                let key = m.get::<String>(&it, 2);
                if key == "queues" {
                    return;
                }
                *ui2.section.borrow_mut() = Section::parse(&key);
                ui2.filter.refilter();
                update_status(&ui2);
            }
        });
    }

    // toolbar actions
    {
        let u = ui.clone();
        b_add.connect_clicked(move |_| add_dialog(&u));
    }
    {
        let u = ui.clone();
        b_resume.connect_clicked(move |_| {
            for p in selected(&u).iter().filter(|p| matches!(p.state, State::Paused | State::Failed)) {
                send(&u, Request::Resume { id: p.id });
            }
        });
    }
    {
        let u = ui.clone();
        b_pause.connect_clicked(move |_| {
            for p in selected(&u).iter().filter(|p| matches!(p.state, State::Running | State::Queued)) {
                send(&u, Request::Pause { id: p.id });
            }
        });
    }
    {
        let u = ui.clone();
        b_pause_all.connect_clicked(move |_| {
            for p in all_rows(&u).iter().filter(|p| matches!(p.state, State::Running | State::Queued)) {
                send(&u, Request::Pause { id: p.id });
            }
        });
    }
    {
        let u = ui.clone();
        b_delete.connect_clicked(move |_| delete_dialog(&u));
    }
    {
        let u = ui.clone();
        b_delete_done.connect_clicked(move |_| delete_completed_dialog(&u));
    }
    {
        let u = ui.clone();
        b_folder.connect_clicked(move |_| {
            if let Some(d) = selected(&u).into_iter().find_map(|p| p.dest) {
                open_folder(&u.window, &d);
            }
        });
    }
    {
        let u = ui.clone();
        table.connect_row_activated(move |tv, path, _| {
            let Some(m) = tv.model() else { return };
            let Some(it) = m.iter(path) else { return };
            let dest = m.get::<String>(&it, C_DEST as i32);
            if dest.is_empty() {
                return;
            }
            if m.get::<String>(&it, C_STATE as i32) == "completed" {
                gtk::FileLauncher::new(Some(&gio::File::for_path(&dest))).launch(Some(&u.window), gio::Cancellable::NONE, |_| {});
            } else {
                open_folder(&u.window, &dest);
            }
        });
    }

    // daemon events
    {
        let ui = ui.clone();
        glib::spawn_future_local(async move {
            while let Ok(ev) = ev_rx.recv().await {
                match ev {
                    Event::Tasks(tasks) => {
                        banner_hide(&ui);
                        apply_tasks(&ui, &tasks);
                    }
                    Event::Queues(names) => apply_queues(&ui, names),
                    Event::Offline(msg) => {
                        ui.banner.set_title(&format!("Starting the download service… ({msg})"));
                        ui.banner.set_revealed(true);
                    }
                    Event::Failure(msg) => ui.toasts.add_toast(adw::Toast::new(&msg)),
                }
            }
        });
    }
    window.present();
}

fn banner_hide(ui: &Ui) {
    ui.banner.set_revealed(false);
}

fn send(ui: &Ui, r: Request) {
    let _ = ui.cmd_tx.send(r);
}

fn open_folder(window: &adw::ApplicationWindow, dest: &str) {
    gtk::FileLauncher::new(Some(&gio::File::for_path(dest))).open_containing_folder(Some(window), gio::Cancellable::NONE, |_| {});
}

fn pick_from(m: &gtk::TreeModel, it: &gtk::TreeIter) -> Picked {
    let dest = m.get::<String>(it, C_DEST as i32);
    Picked { id: m.get::<i64>(it, C_ID as i32), state: State::parse(&m.get::<String>(it, C_STATE as i32)).unwrap_or(State::Queued), dest: (!dest.is_empty()).then_some(dest) }
}

fn selected(ui: &Ui) -> Vec<Picked> {
    let (paths, model) = ui.table.selection().selected_rows();
    paths.iter().filter_map(|p| model.iter(p).map(|it| pick_from(&model, &it))).collect()
}

/// Every row in the table, ignoring the sidebar filter.
fn all_rows(ui: &Ui) -> Vec<Picked> {
    let m = ui.store.upcast_ref::<gtk::TreeModel>();
    let mut out = vec![];
    if let Some(it) = m.iter_first() {
        loop {
            out.push(pick_from(m, &it));
            if !m.iter_next(&it) {
                break;
            }
        }
    }
    out
}

fn add_dialog(ui: &Rc<Ui>) {
    let entry = gtk::Entry::builder().placeholder_text("https://example.com/file.zip").activates_default(true).width_chars(46).build();
    let dialog = adw::AlertDialog::builder().heading("Add download").body("Paste the link to download.").extra_child(&entry).build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("add", "Download");
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("add"));
    let u = ui.clone();
    dialog.connect_response(None, move |_, id| {
        let url = entry.text().trim().to_string();
        if id == "add" && !url.is_empty() {
            send(&u, Request::Add(AddRequest { url, start: true, ..Default::default() }));
        }
    });
    dialog.present(Some(&ui.window));
}

fn delete_dialog(ui: &Rc<Ui>) {
    let picked = selected(ui);
    if picked.is_empty() {
        return;
    }
    let dialog = adw::AlertDialog::builder().heading(format!("Delete {} download(s)?", picked.len())).body("“Remove from list” keeps the files on disk. “Delete files too” also removes the downloaded and partial files.").build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("list", "Remove from list");
    dialog.add_response("files", "Delete files too");
    dialog.set_response_appearance("files", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let u = ui.clone();
    dialog.connect_response(None, move |_, id| {
        if id == "list" || id == "files" {
            for p in &picked {
                send(&u, Request::Remove { id: p.id, delete_files: id == "files" });
            }
        }
    });
    dialog.present(Some(&ui.window));
}

fn delete_completed_dialog(ui: &Rc<Ui>) {
    let done: Vec<i64> = all_rows(ui).iter().filter(|p| p.state == State::Completed).map(|p| p.id).collect();
    if done.is_empty() {
        ui.toasts.add_toast(adw::Toast::new("There are no finished downloads."));
        return;
    }
    let dialog = adw::AlertDialog::builder().heading("Remove finished downloads?").body(format!("{} finished download(s) will be removed from the list. The files stay on disk.", done.len())).build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "Remove");
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let u = ui.clone();
    dialog.connect_response(None, move |_, id| {
        if id == "ok" {
            for i in &done {
                send(&u, Request::Remove { id: *i, delete_files: false });
            }
        }
    });
    dialog.present(Some(&ui.window));
}

fn apply_queues(ui: &Ui, names: Vec<String>) {
    if *ui.queue_names.borrow() == names {
        return;
    }
    let m = ui.sidebar_store.upcast_ref::<gtk::TreeModel>();
    while let Some(child) = m.iter_children(Some(&ui.queues_parent)) {
        ui.sidebar_store.remove(&child);
    }
    for n in &names {
        let it = ui.sidebar_store.append(Some(&ui.queues_parent));
        ui.sidebar_store.set(&it, &[(0, n), (1, &"media-playlist-consecutive"), (2, &format!("q:{n}"))]);
    }
    ui.sidebar.expand_all();
    if let Section::Queue(q) = &*ui.section.borrow() {
        if !names.contains(q) {
            if let Some(first) = ui.sidebar_store.iter_first() {
                ui.sidebar.selection().select_iter(&first);
            }
        }
    }
    *ui.queue_names.borrow_mut() = names;
}

fn apply_tasks(ui: &Ui, tasks: &[TaskInfo]) {
    let store = &ui.store;
    let m = store.upcast_ref::<gtk::TreeModel>();
    let mut index: HashMap<i64, gtk::TreeIter> = HashMap::new();
    if let Some(it) = m.iter_first() {
        loop {
            index.insert(m.get::<i64>(&it, C_ID as i32), it);
            if !m.iter_next(&it) {
                break;
            }
        }
    }
    let present: HashSet<i64> = tasks.iter().map(|t| t.id).collect();
    for (id, it) in &index {
        if !present.contains(id) {
            store.remove(it);
        }
    }
    ui.speeds.borrow_mut().retain(|id, _| present.contains(id));

    let now = Instant::now();
    for t in tasks {
        let speed = {
            let mut sp = ui.speeds.borrow_mut();
            let e = sp.entry(t.id).or_insert((t.downloaded, now, 0.0));
            let dt = now.duration_since(e.1).as_secs_f64();
            if t.state == State::Running {
                if dt > 0.2 && t.downloaded >= e.0 {
                    let inst = (t.downloaded - e.0) as f64 / dt;
                    e.2 = if e.2 == 0.0 { inst } else { 0.6 * e.2 + 0.4 * inst };
                    e.0 = t.downloaded;
                    e.1 = now;
                }
            } else {
                *e = (t.downloaded, now, 0.0);
            }
            e.2
        };
        let name = t.dest.as_deref().and_then(|d| std::path::Path::new(d).file_name()).map(|n| n.to_string_lossy().into_owned()).or_else(|| t.filename.clone()).unwrap_or_else(|| t.url.clone());
        let added = glib::DateTime::from_unix_local(t.added_at).ok().and_then(|d| d.format("%Y-%m-%d %H:%M").ok()).map(|s| s.to_string()).unwrap_or_default();
        let cols: [(u32, &dyn ToValue); 15] = [
            (C_ID, &t.id),
            (C_NAME, &name),
            (C_QUEUE, &t.queue),
            (C_SIZE, &fmt::size(t)),
            (C_PCT, &fmt::percent(t)),
            (C_STATUS, &fmt::status_label(t)),
            (C_LEFT, &fmt::time_left(t, speed)),
            (C_RATE, &fmt::rate(t, speed)),
            (C_ADDED, &added),
            (C_URL, &t.url),
            (C_SIZE_N, &t.size.unwrap_or(0)),
            (C_ADDED_N, &t.added_at),
            (C_STATE, &t.state.as_str()),
            (C_CAT, &t.category.clone().unwrap_or_else(|| "Other".into())),
            (C_DEST, &t.dest.clone().unwrap_or_default()),
        ];
        match index.get(&t.id) {
            Some(it) => store.set(it, &cols),
            None => {
                store.insert_with_values(None, &cols);
            }
        }
    }
    update_status(ui);
}

fn update_status(ui: &Ui) {
    let shown = ui.filter.iter_n_children(None);
    let sp = ui.speeds.borrow();
    let active = sp.values().filter(|e| e.2 > 0.0).count();
    let total: f64 = sp.values().map(|e| e.2).sum();
    let rate = if total > 1.0 { format!(" · {}/s", fmt::bytes(total as u64)) } else { String::new() };
    ui.status.set_text(&format!("{shown} download(s) shown · {active} active{rate}"));
}
