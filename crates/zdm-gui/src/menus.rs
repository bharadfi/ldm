//! The classic menu bar (Tasks / File / Downloads / View / Help), keyboard shortcuts and action states.

use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use zdm_proto::{AddRequest, AfterAll, Request};

use crate::look;
use crate::view::Section;
use crate::{
    add_dialog, delete_completed_dialog, dialogs, selected, send, State, Ui,
};

type UiAction = Box<dyn Fn(&Rc<Ui>)>;

/// Enable or disable the actions that need a selection, according to what is selected.
pub fn refresh_actions(ui: &Ui) {
    let picked = selected(ui);
    let any = |f: &dyn Fn(&crate::Picked) -> bool| picked.iter().any(f);
    let set = |name: &str, on: bool| {
        if let Some(a) = ui.window.lookup_action(name).and_then(|a| a.downcast::<gio::SimpleAction>().ok()) {
            a.set_enabled(on);
        }
    };
    set("open", any(&|p| p.state == State::Completed && p.dest.is_some()));
    set("folder", any(&|p| p.dest.is_some()));
    set("show", any(&|p| p.state != State::Completed));
    set("pause", any(&|p| matches!(p.state, State::Running | State::Queued)));
    set("resume", any(&|p| matches!(p.state, State::Paused | State::Failed)));
    set("again", !picked.is_empty());
    set("copy", !picked.is_empty());
    set("props", !picked.is_empty());
    set("delete", !picked.is_empty());
    set("move", any(&|p| p.state != State::Running));
    set("top", any(&|p| p.state != State::Completed));
    set("refresh", picked.len() == 1 && !matches!(picked[0].state, State::Running | State::Completed));
}

fn current_queue(ui: &Ui) -> String {
    match &*ui.section.borrow() {
        Section::Queue(q) => q.clone(),
        _ => "Main".into(),
    }
}

pub fn install(ui: &Rc<Ui>, app: &adw::Application, slot: &gtk::Box) {
    let section = |items: &[(&str, &str)]| {
        let m = gio::Menu::new();
        for (label, action) in items {
            m.append(Some(label), Some(action));
        }
        m
    };
    let bar = gio::Menu::new();

    let tasks = gio::Menu::new();
    tasks.append_section(None, &section(&[
        ("Add new download…", "win.add"),
        ("Add batch download…", "win.batch"),
        ("Add batch download from clipboard", "win.batch-clipboard"),
        ("Download video from a web page…", "win.video-page"),
        ("Run site grabber", "win.grabber"),
    ]));
    tasks.append_section(None, &section(&[("Show drop target", "win.drop-target")]));
    let export = gio::Menu::new();
    export.append(Some("List of links to a text file…"), Some("win.export"));
    let import = gio::Menu::new();
    import.append(Some("Links from a text file…"), Some("win.import"));
    let ei = gio::Menu::new();
    ei.append_submenu(Some("Export"), &export);
    ei.append_submenu(Some("Import"), &import);
    tasks.append_section(None, &ei);
    tasks.append_section(None, &section(&[("Exit", "win.quit")]));
    bar.append_submenu(Some("_Tasks"), &tasks);

    let file = gio::Menu::new();
    file.append_section(None, &section(&[("Open", "win.open"), ("Open folder", "win.folder"), ("Show progress", "win.show")]));
    file.append_section(None, &section(&[("Properties", "win.props"), ("Delete…", "win.delete")]));
    bar.append_submenu(Some("_File"), &file);

    let dl = gio::Menu::new();
    dl.append_section(None, &section(&[("Resume", "win.resume"), ("Stop", "win.pause"), ("Stop all", "win.pause-all"), ("Download again", "win.again"), ("Move to queue…", "win.move"), ("Move to top of queue", "win.top"), ("Refresh link…", "win.refresh")]));
    dl.append_section(None, &section(&[("Delete all completed", "win.delete-done")]));
    dl.append_section(None, &section(&[("Find (Ctrl+F)", "win.search"), ("Find next (F3)", "win.find-next")]));
    let sched = gio::Menu::new();
    sched.append(Some("Scheduler…"), Some("win.scheduler"));
    let start_sub = gio::Menu::new();
    start_sub.append_section(None, &section(&[("The selected queue", "win.queue-start")]));
    start_sub.append_section(None, &ui.queue_start_menu);
    let stop_sub = gio::Menu::new();
    stop_sub.append_section(None, &section(&[("The selected queue", "win.queue-stop")]));
    stop_sub.append_section(None, &ui.queue_stop_menu);
    let speed = gio::Menu::new();
    for (label, id) in [("Off (no limit)", "0"), ("50 KiB/s", "50"), ("100 KiB/s", "100"), ("250 KiB/s", "250"), ("500 KiB/s", "500"), ("1 MiB/s", "1024"), ("Other…", "custom")] {
        speed.append(Some(label), Some(&format!("win.speed-limit::{id}")));
    }
    let queues = gio::Menu::new();
    queues.append_submenu(Some("Start queue"), &start_sub);
    queues.append_submenu(Some("Stop queue"), &stop_sub);
    queues.append_submenu(Some("Speed limiter (all downloads)"), &speed);
    dl.append_section(None, &sched);
    dl.append_section(None, &queues);
    let after = gio::Menu::new();
    for (label, kind) in [("Do nothing", "nothing"), ("Sleep", "sleep"), ("Shut down", "shutdown"), ("Run a command…", "command")] {
        after.append(Some(label), Some(&format!("win.after-all::{kind}")));
    }
    let after_sec = gio::Menu::new();
    after_sec.append_submenu(Some("When downloads finish"), &after);
    dl.append_section(None, &after_sec);
    dl.append_section(None, &section(&[("Select all", "win.select-all")]));
    dl.append_section(None, &section(&[("Options…", "win.options")]));
    bar.append_submenu(Some("_Downloads"), &dl);

    let view = gio::Menu::new();
    view.append_section(None, &section(&[("Hide categories", "win.view-sidebar")]));
    let arrange = gio::Menu::new();
    for (label, key) in [("By name", "name"), ("By size", "size"), ("By status", "status"), ("By time left", "left"), ("By transfer rate", "rate"), ("By date added", "date")] {
        arrange.append(Some(label), Some(&format!("win.arrange::{key}")));
    }
    let toolbar = gio::Menu::new();
    toolbar.append(Some("Show toolbar"), Some("win.view-toolbar"));
    toolbar.append(Some("Show text under the buttons"), Some("win.toolbar-labels"));
    toolbar.append(Some("Small buttons"), Some("win.toolbar-small"));
    let tray = gio::Menu::new();
    tray.append(Some("Closing the window keeps the app in the tray"), Some("win.tray-close"));
    let font = gio::Menu::new();
    for (label, pt) in look::FONT_SIZES {
        font.append(Some(&format!("{label} ({pt} pt)")), Some(&format!("win.font::{pt}")));
    }
    let v2 = gio::Menu::new();
    v2.append_submenu(Some("Arrange files"), &arrange);
    v2.append_submenu(Some("Toolbar"), &toolbar);
    v2.append_submenu(Some("Tray icon"), &tray);
    v2.append(Some("Customize list…"), Some("win.columns"));
    v2.append(Some("Dark mode"), Some("win.dark-mode"));
    v2.append_submenu(Some("Font"), &font);
    view.append_section(None, &v2);
    bar.append_submenu(Some("_View"), &view);

    let help = gio::Menu::new();
    help.append_section(None, &section(&[("About Linux Download Manager", "win.about")]));
    bar.append_submenu(Some("_Help"), &help);

    let mk = |name: &str, f: UiAction| {
        let a = gio::SimpleAction::new(name, None);
        let u = ui.clone();
        a.connect_activate(move |_, _| f(&u));
        ui.window.add_action(&a);
    };
    mk("add", Box::new(add_dialog));
    mk("batch", Box::new(dialogs::batch_dialog));
    mk("grabber", Box::new(crate::grabber::open));
    mk("import", Box::new(dialogs::import_list));
    mk("export", Box::new(dialogs::export_list));
    mk("quit", Box::new(|u| {
        if let Some(a) = u.window.application() {
            a.quit();
        }
    }));
    mk("pause-all", Box::new(|u| {
        for t in u.latest.borrow().iter().filter(|t| matches!(t.state, State::Running | State::Queued)) {
            send(u, Request::Pause { id: t.id });
        }
    }));
    mk("delete-done", Box::new(delete_completed_dialog));
    mk("move", Box::new(dialogs::move_dialog));
    mk("refresh", Box::new(dialogs::refresh_dialog));
    mk("top", Box::new(|u| {
        // the last one moved ends up first, so go through them backwards to keep their order
        for p in selected(u).iter().rev().filter(|p| p.state != State::Completed) {
            send(u, Request::MoveToTop { id: p.id });
        }
    }));
    mk("search", Box::new(|u| {
        u.search_entry.grab_focus();
    }));
    mk("select-all", Box::new(|u| u.table.selection().select_all()));
    mk("options", Box::new(crate::options::open));
    mk("scheduler", Box::new(dialogs::scheduler_dialog));
    mk("about", Box::new(dialogs::about_dialog));
    mk("queue-start", Box::new(|u| queue_resume(u, &current_queue(u))));
    mk("queue-stop", Box::new(|u| queue_pause(u, &current_queue(u))));
    for (name, resume) in [("queue-start-named", true), ("queue-stop-named", false)] {
        let a = gio::SimpleAction::new(name, Some(&String::static_variant_type()));
        let u = ui.clone();
        a.connect_activate(move |_, p| {
            if let Some(q) = p.and_then(|p| p.get::<String>()) {
                if resume { queue_resume(&u, &q) } else { queue_pause(&u, &q) }
            }
        });
        ui.window.add_action(&a);
    }
    mk("find-next", Box::new(|u| {
        // the table only shows rows that match the search, so "next" is the next shown row
        let sel = u.table.selection();
        let model = u.table.model().expect("model");
        let n = model.iter_n_children(None);
        if n == 0 {
            return;
        }
        let cur = sel.selected_rows().0.first().and_then(|p| p.indices().first().copied()).unwrap_or(-1);
        let next = (cur + 1) % n;
        let path = gtk::TreePath::from_indices(&[next]);
        sel.unselect_all();
        sel.select_path(&path);
        u.table.scroll_to_cell(Some(&path), None::<&gtk::TreeViewColumn>, false, 0.0, 0.0);
    }));
    mk("batch-clipboard", Box::new(|u| {
        let (u, cb) = (u.clone(), u.window.clipboard());
        gtk::glib::spawn_future_local(async move {
            let urls = match cb.read_text_future().await {
                Ok(Some(t)) => crate::helpers::parse_urls(&t),
                _ => vec![],
            };
            if urls.is_empty() {
                u.toasts.add_toast(adw::Toast::new("There are no links on the clipboard."));
                return;
            }
            for url in &urls {
                send(&u, Request::Add(AddRequest { url: url.clone(), start: true, ..Default::default() }));
            }
            u.toasts.add_toast(adw::Toast::new(&format!("Added {} download(s) from the clipboard", urls.len())));
        });
    }));
    mk("video-page", Box::new(|u| crate::video::open(u, None)));
    mk("columns", Box::new(look::columns_dialog));

    // choose what the table is sorted by; choosing the same one again flips the direction
    {
        let a = gio::SimpleAction::new("arrange", Some(&String::static_variant_type()));
        let (u, last) = (ui.clone(), std::cell::RefCell::new((String::new(), false)));
        a.connect_activate(move |_, p| {
            let Some(key) = p.and_then(|p| p.get::<String>()) else { return };
            let col = match key.as_str() {
                "name" => crate::C_NAME,
                "size" => crate::C_SIZE_N,
                "status" => crate::C_PCT,
                "left" => crate::C_LEFT,
                "rate" => crate::C_RATE,
                _ => crate::C_ADDED_N,
            };
            let mut l = last.borrow_mut();
            let descending = if l.0 == key { !l.1 } else { key == "date" };
            *l = (key, descending);
            u.sorted.set_sort_column_id(gtk::SortColumn::Index(col), if descending { gtk::SortType::Descending } else { gtk::SortType::Ascending });
        });
        ui.window.add_action(&a);
    }
    // speed limiter for all downloads together: a few presets and a custom value
    {
        let a = gio::SimpleAction::new_stateful("speed-limit", Some(&String::static_variant_type()), &"0".to_variant());
        let u = ui.clone();
        a.connect_activate(move |act, p| {
            let Some(id) = p.and_then(|p| p.get::<String>()) else { return };
            if id == "custom" {
                dialogs::speed_dialog(&u);
                return;
            }
            if let (Ok(kib), Some(mut s)) = (id.parse::<u64>(), u.settings.borrow().clone()) {
                s.global_limit_kib = kib;
                act.set_state(&id.to_variant());
                send(&u, Request::SetSettings(s));
            }
        });
        ui.window.add_action(&a);
    }
    // look and feel switches (saved in the window preferences)
    {
        let toggle = |name: &str, get: fn(&crate::prefs::GuiPrefs) -> bool, set: fn(&mut crate::prefs::GuiPrefs, bool)| {
            let a = gio::SimpleAction::new_stateful(name, None, &get(&ui.prefs.borrow()).to_variant());
            let u = ui.clone();
            a.connect_activate(move |act, _| {
                let on = !act.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
                act.set_state(&on.to_variant());
                look::change(&u, |p| set(p, on));
            });
            ui.window.add_action(&a);
        };
        toggle("dark-mode", |p| p.dark_mode, |p, v| p.dark_mode = v);
        toggle("toolbar-labels", |p| p.toolbar_labels, |p, v| p.toolbar_labels = v);
        toggle("toolbar-small", |p| p.toolbar_small, |p, v| p.toolbar_small = v);
        toggle("tray-close", |p| p.close_to_tray, |p, v| p.close_to_tray = v);
        let a = gio::SimpleAction::new_stateful("font", Some(&String::static_variant_type()), &ui.prefs.borrow().font_pt.to_string().to_variant());
        let u = ui.clone();
        a.connect_activate(move |act, p| {
            if let Some(pt) = p.and_then(|p| p.get::<String>()).and_then(|s| s.parse::<u32>().ok()) {
                act.set_state(&pt.to_string().to_variant());
                look::change(&u, |p| p.font_pt = pt);
            }
        });
        ui.window.add_action(&a);
    }
    // the small floating box you can drop links on
    {
        let a = gio::SimpleAction::new_stateful("drop-target", None, &false.to_variant());
        let u = ui.clone();
        a.connect_activate(move |act, _| {
            let on = !act.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
            act.set_state(&on.to_variant());
            if on {
                dialogs::show_drop_target(&u);
            } else if let Some(w) = u.drop_window.borrow_mut().take() {
                w.close();
            }
        });
        ui.window.add_action(&a);
    }

    // "When downloads finish": radio items whose state mirrors what the daemon has armed
    {
        let a = gio::SimpleAction::new_stateful("after-all", Some(&String::static_variant_type()), &"nothing".to_variant());
        let u = ui.clone();
        a.connect_activate(move |_, p| {
            if let Some(kind) = p.and_then(|p| p.get::<String>()) {
                dialogs::after_all_choice(&u, &kind);
            }
        });
        ui.window.add_action(&a);
    }
    // the toast offered for a copied link
    {
        let a = gio::SimpleAction::new("add-clipboard", Some(&String::static_variant_type()));
        let u = ui.clone();
        a.connect_activate(move |_, p| {
            if let Some(url) = p.and_then(|p| p.get::<String>()) {
                send(&u, Request::Add(AddRequest { url, start: true, ..Default::default() }));
            }
        });
        ui.window.add_action(&a);
    }

    // View toggles (checkable menu items)
    for (name, which) in [("view-sidebar", 0), ("view-toolbar", 1)] {
        let a = gio::SimpleAction::new_stateful(name, None, &true.to_variant());
        let u = ui.clone();
        a.connect_activate(move |a, _| {
            let on = !a.state().and_then(|s| s.get::<bool>()).unwrap_or(true);
            a.set_state(&on.to_variant());
            if which == 0 {
                u.sidebar_scroll.set_visible(on);
            } else {
                u.toolbar_box.set_visible(on);
            }
        });
        ui.window.add_action(&a);
    }

    for (action, keys) in [
        ("win.add", &["<Ctrl>n"][..]),
        ("win.batch", &["<Ctrl><Shift>n"]),
        ("win.quit", &["<Ctrl>q"]),
        ("win.select-all", &["<Ctrl>a"]),
        ("win.search", &["<Ctrl>f"]),
        ("win.options", &["<Ctrl>comma"]),
        ("win.delete", &["Delete"]),
        ("win.props", &["<Alt>Return"]),
    ] {
        app.set_accels_for_action(action, keys);
    }

    let menubar = gtk::PopoverMenuBar::from_model(Some(&bar));
    menubar.add_css_class("ldm-menubar");
    slot.append(&menubar);

    let u = ui.clone();
    ui.table.selection().connect_changed(move |_| refresh_actions(&u));
    refresh_actions(ui);
}

/// Show what the daemon has armed in the "When downloads finish" menu and the status bar.
pub fn sync_after_all(ui: &Ui, a: &AfterAll) {
    *ui.after_all.borrow_mut() = a.clone();
    let kind = match a {
        AfterAll::Nothing => "nothing",
        AfterAll::Sleep => "sleep",
        AfterAll::Shutdown => "shutdown",
        AfterAll::Command { .. } => "command",
    };
    if let Some(act) = ui.window.lookup_action("after-all").and_then(|x| x.downcast::<gio::SimpleAction>().ok()) {
        act.set_state(&kind.to_variant());
    }
    crate::update_status(ui);
}

fn queue_resume(ui: &Ui, q: &str) {
    let ids: Vec<i64> = ui.latest.borrow().iter().filter(|t| t.queue == q && matches!(t.state, State::Paused | State::Failed)).map(|t| t.id).collect();
    for id in &ids {
        send(ui, Request::Resume { id: *id });
    }
    ui.toasts.add_toast(adw::Toast::new(&format!("Queue “{q}”: resumed {} download(s)", ids.len())));
}

fn queue_pause(ui: &Ui, q: &str) {
    let ids: Vec<i64> = ui.latest.borrow().iter().filter(|t| t.queue == q && matches!(t.state, State::Running | State::Queued)).map(|t| t.id).collect();
    for id in &ids {
        send(ui, Request::Pause { id: *id });
    }
    ui.toasts.add_toast(adw::Toast::new(&format!("Queue “{q}”: stopped {} download(s)", ids.len())));
}

/// Show the daemon's global limit in the Speed limiter menu.
pub fn sync_speed_limit(ui: &Ui, kib: u64) {
    let id = if [0, 50, 100, 250, 500, 1024].contains(&kib) { kib.to_string() } else { "custom".to_string() };
    if let Some(act) = ui.window.lookup_action("speed-limit").and_then(|x| x.downcast::<gio::SimpleAction>().ok()) {
        act.set_state(&id.to_variant());
    }
}
