//! Linux Download Manager window, laid out like a classic download manager: toolbar, category tree, table of downloads.
//!
//! The daemon is polled once a second on a background thread (`worker`); the GTK side only applies the
//! latest snapshot to the table, so nothing here blocks the UI.
//!
//! GtkTreeView is deprecated since GTK 4.10 but still supported; it keeps selection and scroll position
//! while rows are updated in place, which the newer ColumnView needs much more code for.
#![allow(deprecated)]

mod dialogs;
mod fmt;
mod grabber;
mod helpers;
mod menus;
mod prefs;
mod progress;
mod segments;
mod tray;
mod view;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use progress::ProgressWin;
use view::Section;
use zdm_proto::{AddRequest, GrabLink, QueueInfo, Request, Response, Settings, State, TaskInfo};

static BACKGROUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
    Queues(Vec<QueueInfo>),
    Settings(Settings),
    Links(Vec<GrabLink>),
    GrabFailed(String),
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
    app.connect_activate(|app| {
        let ui = build_ui(app);
        if let Ok(base) = std::env::var("LDM_SELFTEST") {
            selftest(&ui, base);
        }
    });
    // `--background` (used by the autostart entry) starts hidden in the tray; GApplication must not parse it.
    BACKGROUND.store(std::env::args().any(|a| a == "--background"), std::sync::atomic::Ordering::Relaxed);
    app.run_with_args(&["zdm-gui"])
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
            if let Some(req @ Request::Grab(_)) = req {
                // a crawl can take a minute: run it beside the polling instead of in front of it
                let events = events.clone();
                tokio::spawn(async move {
                    let ev = match zdm_proto::call(&req).await {
                        Ok(Response::Links { links }) => Event::Links(links),
                        Ok(_) => Event::GrabFailed("unexpected answer from the daemon".into()),
                        Err(e) => Event::GrabFailed(e.to_string()),
                    };
                    let _ = events.send(ev).await;
                });
            } else if let Some(req) = req {
                if let Err(e) = zdm_proto::call(&req).await {
                    let _ = events.send(Event::Failure(e.to_string())).await;
                }
            }
            match zdm_proto::call(&Request::List).await {
                Ok(Response::Tasks { tasks }) => {
                    let _ = events.send(Event::Tasks(tasks)).await;
                    if let Ok(Response::Queues { queues }) = zdm_proto::call(&Request::QueueList).await {
                        let _ = events.send(Event::Queues(queues)).await;
                    }
                    if let Ok(Response::Settings(s)) = zdm_proto::call(&Request::GetSettings).await {
                        let _ = events.send(Event::Settings(s)).await;
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

pub struct Ui {
    pub window: adw::ApplicationWindow,
    pub store: gtk::ListStore,
    pub table: gtk::TreeView,
    pub sidebar_store: gtk::TreeStore,
    pub sidebar: gtk::TreeView,
    pub queues_parent: gtk::TreeIter,
    pub filter: gtk::TreeModelFilter,
    pub section: Rc<RefCell<Section>>,
    pub banner: adw::Banner,
    pub toasts: adw::ToastOverlay,
    pub status: gtk::Label,
    pub cmd_tx: UnboundedSender<Request>,
    pub speeds: RefCell<HashMap<i64, (u64, Instant, f64)>>,
    pub queue_names: RefCell<Vec<String>>,
    pub queue_infos: RefCell<Vec<QueueInfo>>,
    pub settings: RefCell<Option<Settings>>,
    pub prefs: RefCell<prefs::GuiPrefs>,
    pub seen_first: std::cell::Cell<bool>,
    pub tray: RefCell<Option<ksni::blocking::Handle<tray::LdmTray>>>,
    pub last_states: RefCell<HashMap<i64, State>>,
    pub notify_ready: std::cell::Cell<bool>,
    pub last_tip: RefCell<String>,
    pub grab: RefCell<Option<Rc<grabber::GrabWin>>>,
    pub toolbar_box: gtk::Box,
    pub sidebar_scroll: gtk::ScrolledWindow,
    pub latest: RefCell<Vec<TaskInfo>>,
    pub progress: Rc<RefCell<HashMap<i64, Rc<ProgressWin>>>>,
}

#[derive(Clone)]
struct Picked {
    id: i64,
    state: State,
    dest: Option<String>,
}

fn build_ui(app: &adw::Application) -> Rc<Ui> {
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
    let b_grab = make_btn("system-search-symbolic", "Grabber", "Find files on a web page or site", "tb-grab");
    let b_options = make_btn("preferences-system-symbolic", "Options", "Connections, proxy, folders and browser helper", "tb-options");
    let b_sched = make_btn("alarm-symbolic", "Scheduler", "Queues and time windows", "tb-sched");
    let b_qstart = make_btn("media-seek-forward-symbolic", "Start Queue", "Resume every download of the selected queue", "tb-qstart");
    let b_qstop = make_btn("media-record-symbolic", "Stop Queue", "Pause every download of the selected queue", "tb-qstop");
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
    let menubar_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    top.append(&menubar_slot);
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
        queue_infos: RefCell::default(),
        settings: RefCell::default(),
        prefs: RefCell::new(prefs::load()),
        seen_first: std::cell::Cell::new(false),
        tray: RefCell::default(),
        last_states: RefCell::default(),
        notify_ready: std::cell::Cell::new(false),
        last_tip: RefCell::default(),
        grab: RefCell::default(),
        toolbar_box: toolbar_box.clone(),
        sidebar_scroll: sidebar_scroll.clone(),
        latest: RefCell::default(),
        progress: Rc::default(),
    });
    install_menu(&ui);
    menus::install(&ui, app, &menubar_slot);
    for (btn, action) in [(&b_grab, "win.grabber"), (&b_options, "win.options"), (&b_sched, "win.scheduler"), (&b_qstart, "win.queue-start"), (&b_qstop, "win.queue-stop")] {
        let w = window.clone();
        let action = action.to_string();
        btn.connect_clicked(move |_| {
            let _ = gtk::prelude::WidgetExt::activate_action(&w, &action, None);
        });
    }
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
            if m.get::<String>(&it, C_STATE as i32) != "completed" {
                show_progress(&u, m.get::<i64>(&it, C_ID as i32));
                return;
            }
            if dest.is_empty() {
                return;
            }
            open_file(&u, &dest);
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
                        let fresh = apply_tasks(&ui, &tasks);
                        menus::refresh_actions(&ui);
                        notify_and_tray(&ui, &tasks);
                        let first = !ui.seen_first.replace(true);
                        if !first && ui.prefs.borrow().auto_progress {
                            let now = glib::real_time() / 1_000_000;
                            for t in tasks.iter().filter(|t| fresh.contains(&t.id) && matches!(t.state, State::Running | State::Queued) && now - t.added_at < 30) {
                                if ui.progress.borrow().len() < 3 {
                                    show_progress(&ui, t.id);
                                }
                            }
                        }
                    }
                    Event::Queues(infos) => {
                        let names: Vec<String> = infos.iter().map(|q| q.name.clone()).collect();
                        *ui.queue_infos.borrow_mut() = infos;
                        apply_queues(&ui, names);
                    }
                    Event::Settings(s) => *ui.settings.borrow_mut() = Some(s),
                    Event::Links(l) => grabber::show_results(&ui, l),
                    Event::GrabFailed(m) => grabber::show_error(&ui, &m),
                    Event::Offline(msg) => {
                        ui.banner.set_title(&format!("Starting the download service… ({msg})"));
                        ui.banner.set_revealed(true);
                    }
                    Event::Failure(msg) => ui.toasts.add_toast(adw::Toast::new(&msg)),
                }
            }
        });
    }
    let tray_ok = install_tray(&ui, app);
    if !(tray_ok && BACKGROUND.load(std::sync::atomic::Ordering::Relaxed)) {
        window.present();
    }
    ui
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
            remove_picked(&u, &picked, id == "files");
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

/// Map task id -> row of the backing store. Row handles are only valid until the store changes
/// (removing rows invalidates them), so build this right before it is used.
fn row_index(store: &gtk::ListStore) -> HashMap<i64, gtk::TreeIter> {
    let m = store.upcast_ref::<gtk::TreeModel>();
    let mut index = HashMap::new();
    if let Some(it) = m.iter_first() {
        loop {
            index.insert(m.get::<i64>(&it, C_ID as i32), it);
            if !m.iter_next(&it) {
                break;
            }
        }
    }
    index
}

fn apply_tasks(ui: &Ui, tasks: &[TaskInfo]) -> Vec<i64> {
    let mut fresh: Vec<i64> = vec![];
    let store = &ui.store;
    let present: HashSet<i64> = tasks.iter().map(|t| t.id).collect();
    // Remove vanished rows one at a time, looking each row up again: a removal invalidates other handles.
    loop {
        let stale = row_index(store).into_iter().find(|(id, _)| !present.contains(id));
        match stale {
            Some((_, it)) => {
                store.remove(&it);
            }
            None => break,
        }
    }
    let index = row_index(store);
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
                fresh.push(t.id);
            }
        }
    }
    *ui.latest.borrow_mut() = tasks.to_vec();
    let open: Vec<(i64, Rc<ProgressWin>)> = ui.progress.borrow().iter().map(|(i, w)| (*i, w.clone())).collect();
    for (id, pw) in open {
        match tasks.iter().find(|t| t.id == id) {
            Some(t) => {
                let speed = ui.speeds.borrow().get(&id).map_or(0.0, |e| e.2);
                if pw.update(t, speed) {
                    pw.win.close();
                }
            }
            None => pw.win.close(),
        }
    }
    update_status(ui);
    fresh
}

fn update_status(ui: &Ui) {
    let shown = ui.filter.iter_n_children(None);
    let sp = ui.speeds.borrow();
    let active = sp.values().filter(|e| e.2 > 0.0).count();
    let total: f64 = sp.values().map(|e| e.2).sum();
    let rate = if total > 1.0 { format!(" · {}/s", fmt::bytes(total as u64)) } else { String::new() };
    ui.status.set_text(&format!("{shown} download(s) shown · {active} active{rate}"));
}

fn task_by_id(ui: &Ui, id: i64) -> Option<TaskInfo> {
    ui.latest.borrow().iter().find(|t| t.id == id).cloned()
}

fn show_progress(ui: &Rc<Ui>, id: i64) {
    if let Some(pw) = ui.progress.borrow().get(&id) {
        pw.win.present();
        return;
    }
    let map = ui.progress.clone();
    let pw = ProgressWin::new(&ui.window, id, ui.cmd_tx.clone(), move || {
        // Deferred: the map is borrowed while a window closes itself from `apply_tasks`.
        let map = map.clone();
        glib::idle_add_local_once(move || {
            map.borrow_mut().remove(&id);
        });
    });
    if let Some(t) = task_by_id(ui, id) {
        let speed = ui.speeds.borrow().get(&id).map_or(0.0, |e| e.2);
        pw.update(&t, speed);
    }
    ui.progress.borrow_mut().insert(id, pw.clone());
    pw.win.present();
}

fn open_file(ui: &Ui, dest: &str) {
    gtk::FileLauncher::new(Some(&gio::File::for_path(dest))).launch(Some(&ui.window), gio::Cancellable::NONE, |_| {});
}

fn properties_dialog(ui: &Rc<Ui>, id: i64) {
    let Some(t) = task_by_id(ui, id) else { return };
    let size = t.size.map(|s| format!("{} ({s} bytes)", fmt::bytes(s))).unwrap_or_else(|| "unknown".into());
    let added = glib::DateTime::from_unix_local(t.added_at).ok().and_then(|d| d.format("%Y-%m-%d %H:%M:%S").ok()).map(|s| s.to_string()).unwrap_or_default();
    let body = format!(
        "Link: {}\nSaved to: {}\nCategory: {}\nQueue: {}\nStatus: {}\nSize: {}\nDownloaded: {}\nAdded: {}{}",
        t.url,
        t.dest.as_deref().unwrap_or("(not started yet)"),
        t.category.as_deref().unwrap_or("Other"),
        t.queue,
        fmt::status_label(&t),
        size,
        fmt::bytes(t.downloaded),
        added,
        t.error.as_deref().map(|e| format!("\nError: {e}")).unwrap_or_default(),
    );
    let name = t.dest.as_deref().and_then(|d| std::path::Path::new(d).file_name()).map(|n| n.to_string_lossy().into_owned()).or(t.filename.clone()).unwrap_or_else(|| "Download".into());
    let dialog = adw::AlertDialog::builder().heading(name).body(body).build();
    dialog.add_response("copy", "Copy link");
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    let u = ui.clone();
    dialog.connect_response(None, move |_, r| {
        if r == "copy" {
            u.window.clipboard().set_text(&t.url);
        }
    });
    dialog.present(Some(&ui.window));
}

/// Right-click menu of the download table.
type RowAction = Box<dyn Fn(&Rc<Ui>, Vec<Picked>)>;

fn install_menu(ui: &Rc<Ui>) {
    let menu = gio::Menu::new();
    let first = gio::Menu::new();
    first.append(Some("Open"), Some("win.open"));
    first.append(Some("Open folder"), Some("win.folder"));
    first.append(Some("Show progress"), Some("win.show"));
    let second = gio::Menu::new();
    second.append(Some("Pause"), Some("win.pause"));
    second.append(Some("Resume"), Some("win.resume"));
    second.append(Some("Download again"), Some("win.again"));
    let third = gio::Menu::new();
    third.append(Some("Copy link"), Some("win.copy"));
    third.append(Some("Properties"), Some("win.props"));
    third.append(Some("Delete…"), Some("win.delete"));
    for m in [&first, &second, &third] {
        menu.append_section(None, m);
    }

    let mk = |name: &str, f: RowAction| {
        let a = gio::SimpleAction::new(name, None);
        let u = ui.clone();
        a.connect_activate(move |_, _| {
            let sel = selected(&u);
            if !sel.is_empty() {
                f(&u, sel);
            }
        });
        ui.window.add_action(&a);
    };
    mk("open", Box::new(|u, sel| {
        if let Some(d) = sel.iter().find(|p| p.state == State::Completed).and_then(|p| p.dest.clone()) {
            open_file(u, &d);
        }
    }));
    mk("folder", Box::new(|u, sel| {
        if let Some(d) = sel.into_iter().find_map(|p| p.dest) {
            open_folder(&u.window, &d);
        }
    }));
    mk("show", Box::new(|u, sel| show_progress(u, sel[0].id)));
    mk("pause", Box::new(|u, sel| {
        for p in sel.iter().filter(|p| matches!(p.state, State::Running | State::Queued)) {
            send(u, Request::Pause { id: p.id });
        }
    }));
    mk("resume", Box::new(|u, sel| {
        for p in sel.iter().filter(|p| matches!(p.state, State::Paused | State::Failed)) {
            send(u, Request::Resume { id: p.id });
        }
    }));
    mk("again", Box::new(|u, sel| {
        for p in &sel {
            if let Some(t) = task_by_id(u, p.id) {
                send(u, Request::Add(AddRequest { url: t.url, start: true, ..Default::default() }));
            }
        }
    }));
    mk("copy", Box::new(|u, sel| {
        let urls: Vec<String> = sel.iter().filter_map(|p| task_by_id(u, p.id)).map(|t| t.url).collect();
        u.window.clipboard().set_text(&urls.join("\n"));
    }));
    mk("props", Box::new(|u, sel| properties_dialog(u, sel[0].id)));
    mk("delete", Box::new(|u, _| delete_dialog(u)));

    let click = gtk::GestureClick::builder().button(3).build();
    let u = ui.clone();
    click.connect_pressed(move |_, _, x, y| {
        let table = &u.table;
        let (bx, by) = table.convert_widget_to_bin_window_coords(x as i32, y as i32);
        let Some((Some(path), _, _, _)) = table.path_at_pos(bx, by) else { return };
        let sel = table.selection();
        if !sel.path_is_selected(&path) {
            sel.unselect_all();
            sel.select_path(&path);
        }
        menus::refresh_actions(&u);
        let pop = gtk::PopoverMenu::from_model(Some(&menu));
        pop.set_parent(table);
        pop.set_has_arrow(false);
        pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
    });
    ui.table.add_controller(click);
}

/// What the "Delete" dialog does after the user confirmed.
fn remove_picked(ui: &Ui, picked: &[Picked], delete_files: bool) {
    for p in picked {
        send(ui, Request::Remove { id: p.id, delete_files });
    }
}

/// `LDM_SELFTEST=<base url of tools/rangesrv.py>`: add and delete downloads the way the UI does, in several
/// rounds, and exit 0 only if the window survived and the table ends up empty (headless verification).
type UiOnce = Box<dyn FnOnce(&Rc<Ui>)>;

fn selftest(ui: &Rc<Ui>, base: String) {
    let at = |secs: u64, f: UiOnce| {
        let u = ui.clone();
        glib::timeout_add_local_once(Duration::from_secs(secs), move || f(&u));
    };
    let add = |base: String, n: usize| -> UiOnce {
        Box::new(move |u| {
            for i in 0..n {
                let name = ["vslow.bin", "file.bin", "slow.bin"][i % 3];
                send(u, Request::Add(AddRequest { url: format!("{base}/{name}"), start: true, ..Default::default() }));
            }
        })
    };
    at(2, add(base.clone(), 6));
    for (i, name) in ["options", "scheduler", "batch", "about", "move"].iter().enumerate() {
        let name = format!("win.{name}");
        at(3 + i as u64 / 2, Box::new(move |u| {
            if u.table.selection().count_selected_rows() == 0 {
                u.table.selection().select_all();
            }
            let r = gtk::prelude::WidgetExt::activate_action(&u.window, &name, None);
            eprintln!("SELFTEST opened {name}: {r:?}");
        }));
    }
    {
        let b = base.clone();
        at(3, Box::new(move |u| {
            let _ = gtk::prelude::WidgetExt::activate_action(&u.window, "win.grabber", None);
            send(u, Request::Grab(zdm_proto::GrabRequest { url: format!("{b}/site/"), depth: 1, same_host: true, extensions: vec!["bin".into()], max_pages: 0 }));
        }));
        at(9, Box::new(|u| {
            let n = grabber::result_count(u);
            eprintln!("SELFTEST grabber rows: {n}");
            if n != 4 {
                std::process::exit(4);
            }
        }));
    }
    at(4, Box::new(|u| {
        // open a progress window for every download, as double-clicking would
        let ids: Vec<i64> = u.latest.borrow().iter().map(|t| t.id).collect();
        for id in ids {
            show_progress(u, id);
        }
    }));
    at(5, Box::new(|u| {
        // delete the first three rows only
        let sel = u.table.selection();
        sel.unselect_all();
        for i in 0..3 {
            sel.select_path(&gtk::TreePath::from_indices(&[i]));
        }
        let picked = selected(u);
        eprintln!("SELFTEST round 1 deleting {} rows", picked.len());
        remove_picked(u, &picked, true);
    }));
    at(8, add(base, 4));
    at(11, Box::new(|u| {
        u.table.selection().select_all();
        let picked = selected(u);
        eprintln!("SELFTEST round 2 deleting {} rows", picked.len());
        remove_picked(u, &picked, true);
    }));
    at(17, Box::new(|u| {
        let left = u.store.iter_n_children(None);
        eprintln!("SELFTEST rows left: {left}");
        std::process::exit(if left == 0 { 0 } else { 3 });
    }));
}

/// Create the tray icon. Returns false when the desktop has no tray (the window then behaves normally).
fn install_tray(ui: &Rc<Ui>, app: &adw::Application) -> bool {
    // Clicking a notification raises the window.
    let show = gio::SimpleAction::new("show-window", None);
    let w = ui.window.clone();
    show.connect_activate(move |_, _| {
        w.set_visible(true);
        w.present();
    });
    app.add_action(&show);

    if std::env::var_os("LDM_NO_TRAY").is_some() || std::env::var_os("LDM_SELFTEST").is_some() {
        return false;
    }
    let (tx, rx) = async_channel::unbounded();
    let Some(handle) = tray::spawn(tx) else { return false };
    *ui.tray.borrow_mut() = Some(handle);
    std::mem::forget(app.hold()); // keep running in the tray after the window is closed

    let u = ui.clone();
    glib::spawn_future_local(async move {
        while let Ok(cmd) = rx.recv().await {
            match cmd {
                tray::TrayCmd::Show => {
                    u.window.set_visible(true);
                    u.window.present();
                }
                tray::TrayCmd::PauseAll => {
                    for t in u.latest.borrow().iter().filter(|t| matches!(t.state, State::Running | State::Queued)) {
                        send(&u, Request::Pause { id: t.id });
                    }
                }
                tray::TrayCmd::ResumeAll => {
                    for t in u.latest.borrow().iter().filter(|t| matches!(t.state, State::Paused | State::Failed)) {
                        send(&u, Request::Resume { id: t.id });
                    }
                }
                tray::TrayCmd::Quit => {
                    if let Some(a) = u.window.application() {
                        a.quit();
                    }
                }
            }
        }
    });
    let u = ui.clone();
    ui.window.connect_close_request(move |w| {
        if u.prefs.borrow().close_to_tray && u.tray.borrow().is_some() {
            w.set_visible(false);
            glib::Propagation::Stop
        } else {
            if let Some(a) = w.application() {
                a.quit(); // the tray keeps an application hold; without this the process would linger
            }
            glib::Propagation::Proceed
        }
    });
    true
}

/// After each snapshot: desktop notifications for finished/failed downloads and the tray tooltip.
fn notify_and_tray(ui: &Rc<Ui>, tasks: &[TaskInfo]) {
    let now = glib::real_time() / 1_000_000;
    if ui.notify_ready.replace(true) && ui.prefs.borrow().notifications {
        if let Some(app) = ui.window.application() {
            for n in helpers::transitions(&ui.last_states.borrow(), tasks, now) {
                let (id, title, body) = match n {
                    helpers::Notice::Done { id, name } => (format!("done-{id}"), "Download complete".to_string(), name),
                    helpers::Notice::Failed { id, name, error } => (format!("failed-{id}"), "Download failed".to_string(), format!("{name}: {error}")),
                };
                let note = gio::Notification::new(&title);
                note.set_body(Some(&body));
                note.set_icon(&gio::ThemedIcon::new(APP_ID));
                note.set_default_action("app.show-window");
                app.send_notification(Some(&id), &note);
            }
        }
    }
    *ui.last_states.borrow_mut() = tasks.iter().map(|t| (t.id, t.state)).collect();

    if let Some(handle) = &*ui.tray.borrow() {
        let (active, total) = {
            let sp = ui.speeds.borrow();
            (sp.values().filter(|e| e.2 > 0.0).count(), sp.values().map(|e| e.2).sum::<f64>())
        };
        let waiting = tasks.iter().filter(|t| t.state == State::Queued).count();
        let tip = if active > 0 { format!("{active} downloading · {}/s", fmt::bytes(total as u64)) } else if waiting > 0 { format!("{waiting} waiting") } else { "Idle".into() };
        if *ui.last_tip.borrow() != tip {
            tray::set_tooltip(handle, &tip);
            *ui.last_tip.borrow_mut() = tip;
        }
    }
}
