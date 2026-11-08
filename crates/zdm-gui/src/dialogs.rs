//! Options, Scheduler, batch add, import/export, move-to-queue and About dialogs.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use zdm_proto::{AddRequest, AfterAll, QueueInfo, Request, Settings, Window};

use crate::helpers::{self, parse_urls};
use crate::prefs::{self, GuiPrefs};
use crate::{selected, send, State, Ui, APP_ID, APP_NAME};

fn toast(ui: &Ui, msg: &str) {
    ui.toasts.add_toast(adw::Toast::new(msg));
}

fn add_urls(ui: &Ui, urls: &[String]) {
    for u in urls {
        send(ui, Request::Add(AddRequest { url: u.clone(), start: true, ..Default::default() }));
    }
    toast(ui, &format!("Added {} download(s)", urls.len()));
}

fn spin(value: f64, lo: f64, hi: f64, step: f64, title: &str) -> adw::SpinRow {
    let row = adw::SpinRow::new(Some(&gtk::Adjustment::new(value, lo, hi, step, step * 5.0, 0.0)), 1.0, 0);
    row.set_title(title);
    row
}

/// Options: general, connection and browser pages. Daemon settings are sent when the dialog closes.
pub fn options_dialog(ui: &Rc<Ui>) {
    let Some(cur) = ui.settings.borrow().clone() else {
        toast(ui, "Not connected to the download service yet.");
        return;
    };
    let prefs0 = ui.prefs.borrow().clone();
    let dialog = adw::PreferencesDialog::builder().title("Options").build();

    // General
    let page = adw::PreferencesPage::builder().title("General").icon_name("preferences-system-symbolic").build();
    let g = adw::PreferencesGroup::builder().title("Downloads").description("Applies to downloads started from now on.").build();
    let root = adw::EntryRow::builder().title("Save downloads to (category folders are created inside)").text(&cur.download_root).build();
    let auto = adw::SwitchRow::builder().title("Show the progress window when a download starts").active(prefs0.auto_progress).build();
    let exists_modes = ["rename", "overwrite", "skip"];
    let exists = adw::ComboRow::builder()
        .title("When a file with the same name already exists")
        .model(&gtk::StringList::new(&["Add a number to the new name", "Replace the old file", "Skip the download"]))
        .selected(exists_modes.iter().position(|m| *m == cur.on_exists).unwrap_or(0) as u32)
        .build();
    g.add(&root);
    g.add(&exists);
    g.add(&auto);
    page.add(&g);
    let gf = adw::PreferencesGroup::builder().title("Folder for each category").description("Leave empty to use a sub-folder of the folder above.").build();
    let cat_rows: Vec<(String, adw::EntryRow)> = crate::view::CATEGORIES
        .iter()
        .map(|c| {
            let row = adw::EntryRow::builder().title(*c).text(cur.category_dirs.get(*c).cloned().unwrap_or_default()).build();
            gf.add(&row);
            (c.to_string(), row)
        })
        .collect();
    page.add(&gf);
    let gt = adw::PreferencesGroup::builder().title("File types of each category").description("Which file extensions go to which category folder, separated by spaces. Anything else goes to Other.").build();
    let type_rows: Vec<(String, adw::EntryRow)> = zdm_proto::CATEGORY_TABLE
        .iter()
        .map(|(c, defaults)| {
            let shown = cur.category_exts.get(*c).map_or_else(|| defaults.join(" "), |v| v.join(" "));
            let row = adw::EntryRow::builder().title(*c).text(shown).build();
            gt.add(&row);
            (c.to_string(), row)
        })
        .collect();
    page.add(&gt);
    let gp = adw::PreferencesGroup::builder().title("After each download").description("A shell command, for example a virus scan: clamscan \"$ZDM_FILE\". The finished file is in $ZDM_FILE. If it fails, the download is kept and a note is added.").build();
    let post = adw::EntryRow::builder().title("Command (empty = none)").text(cur.post_command.clone().unwrap_or_default()).build();
    gp.add(&post);
    page.add(&gp);
    let gi = adw::PreferencesGroup::builder().title("Interface").build();
    let tray_note = if ui.tray.borrow().is_some() { "" } else { "No tray was found on this desktop, so closing the window will quit the app." };
    let to_tray = adw::SwitchRow::builder().title("Closing the window keeps the app running in the tray").subtitle(tray_note).active(prefs0.close_to_tray).build();
    let notes = adw::SwitchRow::builder().title("Show a notification when a download finishes or fails").active(prefs0.notifications).build();
    let autostart0 = prefs::autostart_enabled();
    let autostart = adw::SwitchRow::builder().title("Start with the computer (hidden in the tray)").active(autostart0).build();
    let clip = adw::SwitchRow::builder().title("Offer to download links I copy").subtitle("Shows a button when you copy a link to a file, a video stream or an FTP site while this window is open.").active(prefs0.monitor_clipboard).build();
    gi.add(&to_tray);
    gi.add(&notes);
    gi.add(&clip);
    gi.add(&autostart);
    page.add(&gi);
    dialog.add(&page);

    // Connection
    let page = adw::PreferencesPage::builder().title("Connection").icon_name("network-wired-symbolic").build();
    let g = adw::PreferencesGroup::builder().title("Speed").description("More connections usually finish large files sooner; a few servers refuse many.").build();
    let conns = spin(cur.connections as f64, 1.0, 32.0, 1.0, "Connections per download");
    let limit = spin(cur.speed_limit_kib as f64, 0.0, 10_000_000.0, 50.0, "Speed limit per download in KiB/s (0 = no limit)");
    g.add(&conns);
    g.add(&limit);
    let gl = spin(cur.global_limit_kib as f64, 0.0, 10_000_000.0, 50.0, "Speed limit for all downloads together in KiB/s (0 = no limit)");
    g.add(&gl);
    let g2 = adw::PreferencesGroup::builder().title("Proxy").build();
    let proxy = adw::EntryRow::builder().title("Proxy address, for example socks5://127.0.0.1:1080 (empty = system settings)").text(cur.proxy.clone().unwrap_or_default()).build();
    g2.add(&proxy);
    page.add(&g);
    let g3 = adw::PreferencesGroup::builder().title("Reliability").description("A download that fails because of a network problem is tried again by itself after a growing pause.").build();
    let retries = spin(cur.auto_retries as f64, 0.0, 20.0, 1.0, "Automatic retries");
    g3.add(&retries);
    page.add(&g2);
    page.add(&g3);
    dialog.add(&page);

    // Browser
    let page = adw::PreferencesPage::builder().title("Browser").icon_name("web-browser-symbolic").build();
    let g = adw::PreferencesGroup::builder().title("Browser integration").description("Install the helper once, then load the extension from the project's extension/dist folder (see extension/README.md).").build();
    let row = adw::ActionRow::builder().title("Install the browser helper").subtitle("Lets the extension in Firefox, Chrome, Brave and Edge hand downloads to this app. Writes small files in your home folder only.").subtitle_lines(0).build();
    let btn = gtk::Button::builder().label("Install").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
    row.add_suffix(&btn);
    g.add(&row);
    page.add(&g);
    dialog.add(&page);
    {
        let u = ui.clone();
        btn.connect_clicked(move |_| {
            let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("zdm-nmhost")));
            let msg = match exe.filter(|p| p.exists()).map(|p| std::process::Command::new(p).arg("install").output()) {
                Some(Ok(o)) if o.status.success() => "Browser helper installed. Restart your browser.".to_string(),
                Some(Ok(o)) => format!("Helper install failed: {}", String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("unknown error")),
                Some(Err(e)) => format!("Could not run the helper installer: {e}"),
                None => "zdm-nmhost was not found next to this program.".to_string(),
            };
            toast(&u, &msg);
        });
    }

    let u = ui.clone();
    dialog.connect_closed(move |_| {
        conns.update();
        limit.update();
        gl.update();
        retries.update();
        let p = proxy.text().trim().to_string();
        let category_dirs = cat_rows.iter().filter_map(|(c, r)| Some((c.clone(), r.text().trim().to_string())).filter(|(_, d)| !d.is_empty())).collect();
        let command = post.text().trim().to_string();
        // only keep a category's list when it differs from the built-in one
        let mut category_exts = cur.category_exts.clone();
        for (cat, row) in &type_rows {
            let typed = helpers::parse_extensions(&row.text());
            let default: Vec<String> = zdm_proto::CATEGORY_TABLE.iter().find(|(c, _)| c == cat).map(|(_, d)| d.iter().map(|e| e.to_string()).collect()).unwrap_or_default();
            if typed.is_empty() || typed == default {
                category_exts.remove(cat);
            } else {
                category_exts.insert(cat.clone(), typed);
            }
        }
        let s = Settings {
            on_exists: exists_modes[(exists.selected() as usize).min(2)].to_string(),
            category_exts,
            connections: conns.value() as u32,
            speed_limit_kib: limit.value() as u64,
            global_limit_kib: gl.value() as u64,
            auto_retries: retries.value() as u32,
            proxy: (!p.is_empty()).then_some(p),
            download_root: root.text().trim().to_string(),
            category_dirs,
            post_command: (!command.is_empty()).then_some(command),
        };
        if s != cur {
            send(&u, Request::SetSettings(s));
            toast(&u, "Options saved. They apply to downloads started from now on.");
        }
        let gp = GuiPrefs { auto_progress: auto.is_active(), close_to_tray: to_tray.is_active(), notifications: notes.is_active(), monitor_clipboard: clip.is_active(), ..u.prefs.borrow().clone() };
        if autostart.is_active() != autostart0 {
            if let Err(e) = prefs::set_autostart(autostart.is_active()) {
                toast(&u, &format!("Could not change the autostart entry: {e}"));
            }
        }
        if gp != prefs0 {
            prefs::save(&gp);
            *u.prefs.borrow_mut() = gp;
        }
    });
    dialog.present(Some(&ui.window));
}

/// Scheduler: pick a queue, set how many downloads run at once and when it may run; create/delete queues.
pub fn scheduler_dialog(ui: &Rc<Ui>) {
    let infos: Vec<QueueInfo> = ui.queue_infos.borrow().clone();
    if infos.is_empty() {
        toast(ui, "Not connected to the download service yet.");
        return;
    }
    let dialog = adw::PreferencesDialog::builder().title("Scheduler and queues").build();
    let page = adw::PreferencesPage::builder().title("Queues").icon_name("appointment-soon-symbolic").build();

    let names: Vec<&str> = infos.iter().map(|q| q.name.as_str()).collect();
    let combo = adw::ComboRow::builder().title("Queue").model(&gtk::StringList::new(&names)).build();
    let g1 = adw::PreferencesGroup::builder().title("Choose a queue").build();
    g1.add(&combo);

    let g2 = adw::PreferencesGroup::builder().title("Rules").build();
    let parallel = spin(1.0, 1.0, 16.0, 1.0, "Downloads running at the same time");
    let qlimit = spin(0.0, 0.0, 10_000_000.0, 50.0, "Speed limit for this queue in KiB/s (0 = no limit)");
    let use_window = adw::SwitchRow::builder().title("Only download during a time window").build();
    let sh = spin(0.0, 0.0, 23.0, 1.0, "Start hour");
    let sm = spin(0.0, 0.0, 59.0, 5.0, "Start minute");
    let eh = spin(0.0, 0.0, 23.0, 1.0, "Stop hour");
    let em = spin(0.0, 0.0, 59.0, 5.0, "Stop minute");
    let days_row = adw::ActionRow::builder().title("Days").build();
    let day_box = gtk::Box::builder().spacing(4).valign(gtk::Align::Center).build();
    let day_btns: Vec<gtk::ToggleButton> = helpers::DAY_NAMES.iter().map(|n| gtk::ToggleButton::builder().label(*n).build()).collect();
    for b in &day_btns {
        day_box.append(b);
    }
    days_row.add_suffix(&day_box);
    g2.add(&parallel);
    g2.add(&qlimit);
    g2.add(&use_window);
    for r in [sh.upcast_ref::<gtk::Widget>(), sm.upcast_ref(), eh.upcast_ref(), em.upcast_ref(), days_row.upcast_ref()] {
        use_window.bind_property("active", r, "sensitive").sync_create().build();
    }
    g2.add(&sh);
    g2.add(&sm);
    g2.add(&eh);
    g2.add(&em);
    g2.add(&days_row);

    let g3 = adw::PreferencesGroup::builder().build();
    let save_row = adw::ActionRow::builder().title("Save the rules of this queue").build();
    let save = gtk::Button::builder().label("Save").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
    save_row.add_suffix(&save);
    let del_row = adw::ActionRow::builder().title("Delete this queue").subtitle("The default queue “Main” cannot be deleted.").build();
    let del = gtk::Button::builder().label("Delete").valign(gtk::Align::Center).css_classes(["destructive-action"]).build();
    del_row.add_suffix(&del);
    g3.add(&save_row);
    g3.add(&del_row);

    let g4 = adw::PreferencesGroup::builder().title("New queue").build();
    let new_name = adw::EntryRow::builder().title("Name of the new queue").show_apply_button(true).build();
    g4.add(&new_name);

    page.add(&g1);
    page.add(&g2);
    page.add(&g3);
    page.add(&g4);
    dialog.add(&page);

    // fill the widgets from the chosen queue
    let infos = Rc::new(infos);
    let day_btns = Rc::new(day_btns);
    let fill = {
        let (infos, day_btns) = (infos.clone(), day_btns.clone());
        let (combo, parallel, use_window, sh, sm, eh, em, del, qlimit) = (combo.clone(), parallel.clone(), use_window.clone(), sh.clone(), sm.clone(), eh.clone(), em.clone(), del.clone(), qlimit.clone());
        move || {
            let Some(q) = infos.get(combo.selected() as usize) else { return };
            parallel.set_value(q.max_parallel as f64);
            qlimit.set_value(q.limit_kib as f64);
            use_window.set_active(q.window.is_some());
            let w = q.window.unwrap_or(Window { start_min: 0, end_min: 0, days: 0x7f });
            let ((a, b), (c, d)) = (helpers::hm(w.start_min), helpers::hm(w.end_min));
            sh.set_value(a as f64);
            sm.set_value(b as f64);
            eh.set_value(c as f64);
            em.set_value(d as f64);
            for (i, btn) in day_btns.iter().enumerate() {
                btn.set_active(w.days & (1 << i) != 0);
            }
            del.set_sensitive(q.name != "Main");
        }
    };
    fill();
    {
        let fill = fill.clone();
        combo.connect_selected_notify(move |_| fill());
    }
    {
        let (u, d, infos, combo) = (ui.clone(), dialog.clone(), infos.clone(), combo.clone());
        let day_btns = day_btns.clone();
        save.connect_clicked(move |_| {
            let Some(q) = infos.get(combo.selected() as usize) else { return };
            for r in [&parallel, &qlimit, &sh, &sm, &eh, &em] {
                r.update();
            }
            let days = day_btns.iter().enumerate().filter(|(_, b)| b.is_active()).fold(0u8, |a, (i, _)| a | (1 << i));
            let window = if use_window.is_active() {
                if days == 0 {
                    toast(&u, "Pick at least one day.");
                    return;
                }
                Some(Window { start_min: helpers::minutes(sh.value() as u32, sm.value() as u32), end_min: helpers::minutes(eh.value() as u32, em.value() as u32), days })
            } else {
                None
            };
            send(&u, Request::QueueSet(QueueInfo { name: q.name.clone(), max_parallel: parallel.value() as u32, window, limit_kib: qlimit.value() as u64 }));
            toast(&u, &format!("Queue “{}” saved ({}).", q.name, helpers::window_label(&window)));
            d.close();
        });
    }
    {
        let (u, d, infos, combo) = (ui.clone(), dialog.clone(), infos.clone(), combo.clone());
        del.connect_clicked(move |_| {
            if let Some(q) = infos.get(combo.selected() as usize) {
                send(&u, Request::QueueRemove { name: q.name.clone() });
                toast(&u, &format!("Deleting queue “{}”…", q.name));
                d.close();
            }
        });
    }
    {
        let (u, d) = (ui.clone(), dialog.clone());
        new_name.connect_apply(move |row| {
            let name = row.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            send(&u, Request::QueueSet(QueueInfo { name: name.clone(), max_parallel: 1, window: None, limit_kib: 0 }));
            toast(&u, &format!("Queue “{name}” created. Open the scheduler again to set its rules."));
            d.close();
        });
    }
    dialog.present(Some(&ui.window));
}

/// Paste several links at once; the clipboard is read to prefill the box.
pub fn batch_dialog(ui: &Rc<Ui>) {
    let tv = gtk::TextView::builder().wrap_mode(gtk::WrapMode::WordChar).top_margin(6).bottom_margin(6).left_margin(6).right_margin(6).build();
    let sc = gtk::ScrolledWindow::builder().min_content_height(170).min_content_width(480).has_frame(true).child(&tv).build();
    let dialog = adw::AlertDialog::builder().heading("Add batch download").body("One link per line. Anything that is not an http(s) link is ignored.").extra_child(&sc).build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("add", "Download");
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    {
        let (tv, cb) = (tv.clone(), ui.window.clipboard());
        glib::spawn_future_local(async move {
            if let Ok(Some(t)) = cb.read_text_future().await {
                let urls = parse_urls(&t);
                if !urls.is_empty() {
                    tv.buffer().set_text(&urls.join("\n"));
                }
            }
        });
    }
    let u = ui.clone();
    dialog.connect_response(None, move |_, id| {
        if id == "add" {
            let b = tv.buffer();
            let urls = parse_urls(&b.text(&b.start_iter(), &b.end_iter(), false));
            if urls.is_empty() {
                toast(&u, "No http(s) links found.");
            } else {
                add_urls(&u, &urls);
            }
        }
    });
    dialog.present(Some(&ui.window));
}

pub fn import_list(ui: &Rc<Ui>) {
    let u = ui.clone();
    gtk::FileDialog::builder().title("Import a list of links").build().open(Some(&ui.window), gio::Cancellable::NONE, move |res| {
        let Ok(file) = res else { return };
        let Some(path) = file.path() else { return };
        match std::fs::read_to_string(&path) {
            Ok(t) => {
                let urls = parse_urls(&t);
                if urls.is_empty() {
                    toast(&u, "No http(s) links found in that file.");
                } else {
                    add_urls(&u, &urls);
                }
            }
            Err(e) => toast(&u, &format!("Cannot read the file: {e}")),
        }
    });
}

pub fn export_list(ui: &Rc<Ui>) {
    let urls: Vec<String> = ui.latest.borrow().iter().map(|t| t.url.clone()).collect();
    if urls.is_empty() {
        toast(ui, "There are no downloads to export.");
        return;
    }
    let u = ui.clone();
    gtk::FileDialog::builder().title("Export the list of links").initial_name("ldm-downloads.txt").build().save(Some(&ui.window), gio::Cancellable::NONE, move |res| {
        let Ok(file) = res else { return };
        let Some(path) = file.path() else { return };
        match std::fs::write(&path, urls.join("\n") + "\n") {
            Ok(()) => toast(&u, &format!("Exported {} link(s).", urls.len())),
            Err(e) => toast(&u, &format!("Cannot write the file: {e}")),
        }
    });
}

/// Move the selected (not running) downloads to another queue.
pub fn move_dialog(ui: &Rc<Ui>) {
    let picked: Vec<_> = selected(ui).into_iter().filter(|p| p.state != State::Running).collect();
    if picked.is_empty() {
        toast(ui, "Select downloads that are not running (pause them first).");
        return;
    }
    let names: Vec<String> = ui.queue_infos.borrow().iter().map(|q| q.name.clone()).collect();
    if names.is_empty() {
        return;
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let dd = gtk::DropDown::from_strings(&refs);
    let dialog = adw::AlertDialog::builder().heading(format!("Move {} download(s) to queue", picked.len())).extra_child(&dd).build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("move", "Move");
    dialog.set_response_appearance("move", adw::ResponseAppearance::Suggested);
    let u = ui.clone();
    dialog.connect_response(None, move |_, id| {
        if id == "move" {
            if let Some(q) = names.get(dd.selected() as usize) {
                for p in &picked {
                    send(&u, Request::MoveTask { id: p.id, queue: q.clone() });
                }
            }
        }
    });
    dialog.present(Some(&ui.window));
}

pub fn about_dialog(ui: &Rc<Ui>) {
    let d = adw::AboutDialog::builder()
        .application_name(APP_NAME)
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("Linux Download Manager project")
        .comments("Fast, resumable, multi-connection downloads with browser integration, queues and a scheduler.")
        .license_type(gtk::License::Custom)
        .license("MIT OR Apache-2.0")
        .build();
    d.present(Some(&ui.window));
}

/// "When downloads finish": pick what happens once everything is done (asks first for shutdown and sleep).
pub fn after_all_choice(ui: &Rc<Ui>, kind: &str) {
    match kind {
        "sleep" | "shutdown" => {
            let (what, action) = if kind == "sleep" { ("put the computer to sleep", AfterAll::Sleep) } else { ("shut the computer down", AfterAll::Shutdown) };
            let dialog = adw::AlertDialog::builder()
                .heading(format!("{} when downloads finish?", if kind == "sleep" { "Sleep" } else { "Shut down" }))
                .body(format!("About 30 seconds after the last download finishes, the computer will {what}. You can change your mind in Downloads > When downloads finish."))
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("ok", "Yes");
            dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let u = ui.clone();
            dialog.connect_response(None, move |_, r| {
                if r == "ok" {
                    send(&u, Request::SetAfterAll(action.clone()));
                } else {
                    crate::menus::sync_after_all(&u, &u.after_all.borrow().clone()); // put the radio button back
                }
            });
            dialog.present(Some(&ui.window));
        }
        "command" => {
            let entry = gtk::Entry::builder().placeholder_text("for example: notify-send \"Downloads finished\"").width_chars(48).activates_default(true).build();
            let dialog = adw::AlertDialog::builder().heading("Run a command when downloads finish").body("Runs once, about 30 seconds after the last download finishes.").extra_child(&entry).build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("ok", "Arm");
            dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("ok"));
            dialog.set_close_response("cancel");
            let u = ui.clone();
            dialog.connect_response(None, move |_, r| {
                let command = entry.text().trim().to_string();
                if r == "ok" && !command.is_empty() {
                    send(&u, Request::SetAfterAll(AfterAll::Command { command }));
                } else {
                    crate::menus::sync_after_all(&u, &u.after_all.borrow().clone());
                }
            });
            dialog.present(Some(&ui.window));
        }
        _ => send(ui, Request::SetAfterAll(AfterAll::Nothing)),
    }
}

/// "Refresh link": a new address for a download that is stopped or failed (what is already there is kept).
pub fn refresh_dialog(ui: &Rc<Ui>) {
    let Some(p) = selected(ui).into_iter().next() else { return };
    let Some(t) = crate::task_by_id(ui, p.id) else { return };
    if matches!(t.state, State::Running | State::Completed) {
        toast(ui, "Pause the download first (finished downloads cannot be refreshed).");
        return;
    }
    let entry = gtk::Entry::builder().text(&t.url).width_chars(52).activates_default(true).build();
    let dialog = adw::AlertDialog::builder()
        .heading("Refresh link")
        .body("Paste the new address of the same file, for example after the old link expired. What is already downloaded is kept when the server confirms it is the same file.")
        .extra_child(&entry)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "Use this link");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    let u = ui.clone();
    dialog.connect_response(None, move |_, r| {
        let url = entry.text().trim().to_string();
        if r == "ok" && !url.is_empty() {
            send(&u, Request::SetUrl { id: p.id, url });
            toast(&u, "Link updated.");
        }
    });
    dialog.present(Some(&ui.window));
}

/// "Speed limiter > Other…": a limit for all downloads together.
pub fn speed_dialog(ui: &Rc<Ui>) {
    let Some(cur) = ui.settings.borrow().clone() else {
        toast(ui, "Not connected to the download service yet.");
        return;
    };
    let entry = gtk::Entry::builder().text(cur.global_limit_kib.to_string()).input_purpose(gtk::InputPurpose::Digits).activates_default(true).build();
    let dialog = adw::AlertDialog::builder().heading("Speed limiter").body("Limit for all downloads together, in KiB per second. 0 means no limit.").extra_child(&entry).build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "OK");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    let u = ui.clone();
    dialog.connect_response(None, move |_, r| {
        match (r, entry.text().trim().parse::<u64>()) {
            ("ok", Ok(kib)) => {
                let mut s = cur.clone();
                s.global_limit_kib = kib;
                crate::menus::sync_speed_limit(&u, kib);
                send(&u, Request::SetSettings(s));
            }
            ("ok", Err(_)) => toast(&u, "Type a whole number of KiB per second."),
            _ => crate::menus::sync_speed_limit(&u, cur.global_limit_kib), // put the radio button back
        }
    });
    dialog.present(Some(&ui.window));
}

/// A small window to drop links on (from a browser address bar or a page).
pub fn show_drop_target(ui: &Rc<Ui>) {
    if ui.drop_window.borrow().is_some() {
        return;
    }
    let logo = gtk::Image::from_paintable(gtk::gdk::Texture::from_bytes(&glib::Bytes::from_static(crate::LOGO_64)).ok().as_ref());
    logo.set_pixel_size(48);
    let label = gtk::Label::builder().label("Drop links here").css_classes(["dim-label"]).build();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_top(12).margin_bottom(12).margin_start(14).margin_end(14).build();
    content.append(&logo);
    content.append(&label);
    let header = adw::HeaderBar::new();
    header.add_css_class("ldm-header");
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&content));
    let win = adw::Window::builder().title("Drop target").default_width(170).default_height(130).resizable(false).content(&tv).css_classes(["ldm-window"]).build();
    let drop = gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::COPY);
    let u = ui.clone();
    drop.connect_drop(move |_, value, _, _| {
        let Ok(text) = value.get::<String>() else { return false };
        let urls = parse_urls(&text);
        if urls.is_empty() {
            return false;
        }
        add_urls(&u, &urls);
        true
    });
    win.add_controller(drop);
    let u = ui.clone();
    win.connect_close_request(move |_| {
        *u.drop_window.borrow_mut() = None;
        if let Some(a) = u.window.lookup_action("drop-target").and_then(|x| x.downcast::<gio::SimpleAction>().ok()) {
            a.set_state(&false.to_variant());
        }
        glib::Propagation::Proceed
    });
    *ui.drop_window.borrow_mut() = Some(win.clone());
    win.present();
}
