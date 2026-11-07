//! The classic menu bar (Tasks / File / Downloads / View / Help), keyboard shortcuts and action states.

use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use zdm_proto::Request;

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
    tasks.append_section(None, &section(&[("Add new download…", "win.add"), ("Add batch download from text…", "win.batch"), ("Site grabber…", "win.grabber")]));
    tasks.append_section(None, &section(&[("Import list from file…", "win.import"), ("Export list to file…", "win.export")]));
    tasks.append_section(None, &section(&[("Exit", "win.quit")]));
    bar.append_submenu(Some("_Tasks"), &tasks);

    let file = gio::Menu::new();
    file.append_section(None, &section(&[("Open", "win.open"), ("Open folder", "win.folder"), ("Show progress", "win.show")]));
    file.append_section(None, &section(&[("Properties", "win.props"), ("Delete…", "win.delete")]));
    bar.append_submenu(Some("_File"), &file);

    let dl = gio::Menu::new();
    dl.append_section(None, &section(&[("Resume", "win.resume"), ("Pause", "win.pause"), ("Pause all", "win.pause-all"), ("Download again", "win.again"), ("Move to queue…", "win.move")]));
    dl.append_section(None, &section(&[("Start queue", "win.queue-start"), ("Stop queue", "win.queue-stop"), ("Scheduler…", "win.scheduler")]));
    dl.append_section(None, &section(&[("Select all", "win.select-all"), ("Delete completed", "win.delete-done")]));
    dl.append_section(None, &section(&[("Options…", "win.options")]));
    bar.append_submenu(Some("_Downloads"), &dl);

    let view = gio::Menu::new();
    view.append_section(None, &section(&[("Category tree", "win.view-sidebar"), ("Toolbar", "win.view-toolbar")]));
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
    mk("select-all", Box::new(|u| u.table.selection().select_all()));
    mk("options", Box::new(dialogs::options_dialog));
    mk("scheduler", Box::new(dialogs::scheduler_dialog));
    mk("about", Box::new(dialogs::about_dialog));
    mk("queue-start", Box::new(|u| {
        let q = current_queue(u);
        let ids: Vec<i64> = u.latest.borrow().iter().filter(|t| t.queue == q && matches!(t.state, State::Paused | State::Failed)).map(|t| t.id).collect();
        for id in &ids {
            send(u, Request::Resume { id: *id });
        }
        u.toasts.add_toast(adw::Toast::new(&format!("Queue “{q}”: resumed {} download(s)", ids.len())));
    }));
    mk("queue-stop", Box::new(|u| {
        let q = current_queue(u);
        let ids: Vec<i64> = u.latest.borrow().iter().filter(|t| t.queue == q && matches!(t.state, State::Running | State::Queued)).map(|t| t.id).collect();
        for id in &ids {
            send(u, Request::Pause { id: *id });
        }
        u.toasts.add_toast(adw::Toast::new(&format!("Queue “{q}”: paused {} download(s)", ids.len())));
    }));

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
