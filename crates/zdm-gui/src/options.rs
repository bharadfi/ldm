//! The Options window, laid out like a classic download manager's configuration dialog: one tab per topic
//! (General, File types, Save to, Downloads, Connection, Proxy / Socks, Sites Logins, Sounds), OK and Cancel.
//! Nothing is applied until OK; then the daemon settings, the window preferences, the start-at-login entry and the
//! browser registrations are updated together.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use zdm_proto::{Request, Settings, SiteLogin};

use crate::helpers::{self, ProxyParts};
use crate::prefs;
use crate::sound::{self, SoundEvent};
use crate::{send, Ui};

/// A tab: a grid of label + control rows with a few headings and notes.
struct Form {
    grid: gtk::Grid,
    row: i32,
}

impl Form {
    fn new() -> Form {
        Form { grid: gtk::Grid::builder().row_spacing(6).column_spacing(10).margin_top(12).margin_bottom(12).margin_start(14).margin_end(14).build(), row: 0 }
    }
    fn heading(&mut self, text: &str) {
        let l = gtk::Label::builder().label(text).xalign(0.0).css_classes(["heading"]).margin_top(if self.row == 0 { 0 } else { 8 }).build();
        self.grid.attach(&l, 0, self.row, 2, 1);
        self.row += 1;
    }
    fn note(&mut self, text: &str) {
        let l = gtk::Label::builder().label(text).xalign(0.0).wrap(true).css_classes(["dim-label"]).build();
        self.grid.attach(&l, 0, self.row, 2, 1);
        self.row += 1;
    }
    fn row(&mut self, label: &str, w: &impl IsA<gtk::Widget>) {
        let l = gtk::Label::builder().label(label).xalign(0.0).build();
        w.set_hexpand(true);
        self.grid.attach(&l, 0, self.row, 1, 1);
        self.grid.attach(w, 1, self.row, 1, 1);
        self.row += 1;
    }
    fn wide(&mut self, w: &impl IsA<gtk::Widget>) {
        self.grid.attach(w, 0, self.row, 2, 1);
        self.row += 1;
    }
    fn page(self) -> gtk::ScrolledWindow {
        gtk::ScrolledWindow::builder().child(&self.grid).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build()
    }
}

fn check(label: &str, on: bool) -> gtk::CheckButton {
    gtk::CheckButton::builder().label(label).active(on).build()
}

fn spin(value: u64, lo: f64, hi: f64) -> gtk::SpinButton {
    let s = gtk::SpinButton::with_range(lo, hi, 1.0);
    s.set_value(value as f64);
    s.set_numeric(true);
    s
}

/// Entry + Browse button picking a folder (or, with `file`, a file).
fn path_row(win: &adw::Window, entry: &gtk::Entry, file: bool) -> gtk::Box {
    let b = gtk::Button::with_label("Browse…");
    let (win, entry2) = (win.clone(), entry.clone());
    b.connect_clicked(move |_| {
        let e = entry2.clone();
        let done = move |res: Result<gio::File, glib::Error>| {
            if let Some(p) = res.ok().and_then(|f| f.path()) {
                e.set_text(&p.to_string_lossy());
            }
        };
        let dialog = gtk::FileDialog::builder().title(if file { "Choose a sound file" } else { "Choose a folder" }).build();
        if file {
            dialog.open(Some(&win), gio::Cancellable::NONE, done);
        } else {
            dialog.select_folder(Some(&win), gio::Cancellable::NONE, done);
        }
    });
    let row = gtk::Box::builder().spacing(6).build();
    entry.set_hexpand(true);
    row.append(entry);
    row.append(&b);
    row
}

fn nmhost() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("zdm-nmhost"))).filter(|p| p.exists())
}

/// Program that starts each browser.
fn browser_program(name: &str) -> &'static str {
    match name {
        "chrome" => "google-chrome",
        "chromium" => "chromium",
        "brave" => "brave-browser",
        "brave-origin" => "brave-origin",
        "edge" => "microsoft-edge",
        _ => {
            // Developer Edition and Nightly are the Firefox builds that accept this add-on
            let path = std::env::var_os("PATH").unwrap_or_default();
            ["firefox-developer-edition", "firefox-nightly", "firefox-dev", "firefox"].into_iter().find(|p| std::env::split_paths(&path).any(|d| d.join(p).is_file())).unwrap_or("firefox")
        }
    }
}

/// Extension ID that the browser shows in its extensions page (the setting pages live at chrome-extension://ID/popup.html).
const EXT_ID: &str = zdm_nmhost::CHROME_EXTENSION_ID;

/// The address scheme of the browser's own pages (`brave://extensions`).
fn browser_scheme(name: &str) -> &'static str {
    match name {
        "brave" | "brave-origin" => "brave",
        "edge" => "edge",
        _ => "chrome",
    }
}

fn launch_browser(name: &str, url: &str) {
    let _ = std::process::Command::new(browser_program(name)).arg(url).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

/// (browser, registered, found, extension state) from `zdm-nmhost status`.
fn browser_status() -> Vec<(String, bool, bool, String, String)> {
    let Some(exe) = nmhost() else { return vec![] };
    let out = std::process::Command::new(exe).arg("status").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()? == "registered", it.next()? == "found", it.next().unwrap_or("missing").to_string(), it.next().unwrap_or(EXT_ID).to_string()))
        })
        .collect()
}

fn browser_title(name: &str) -> &str {
    match name {
        "chrome" => "Google Chrome",
        "chromium" => "Chromium",
        "brave" => "Brave",
        "brave-origin" => "Brave Origin",
        "edge" => "Microsoft Edge",
        "firefox" => "Mozilla Firefox",
        other => other,
    }
}

fn login_dialog(parent: &adw::Window, existing: Option<SiteLogin>, done: impl Fn(SiteLogin) + 'static) {
    let host = gtk::Entry::builder().placeholder_text("Site, for example files.example.com").text(existing.as_ref().map_or("", |l| &l.host)).activates_default(true).build();
    let user = gtk::Entry::builder().placeholder_text("User name").text(existing.as_ref().map_or("", |l| &l.user)).activates_default(true).build();
    let pass = gtk::PasswordEntry::builder().placeholder_text("Password").show_peek_icon(true).activates_default(true).build();
    pass.set_text(existing.as_ref().map_or("", |l| &l.pass));
    let form = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    form.append(&host);
    form.append(&user);
    form.append(&pass);
    let dialog = adw::AlertDialog::builder().heading("Site login").body("Used for downloads from this site and its subdomains that come without a login of their own.").extra_child(&form).build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "OK");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, r| {
        let h = host.text().trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_ascii_lowercase();
        if r == "ok" && !h.is_empty() && !user.text().trim().is_empty() {
            done(SiteLogin { host: h, user: user.text().trim().to_string(), pass: pass.text().to_string() });
        }
    });
    dialog.present(Some(parent));
}

pub fn open(ui: &Rc<Ui>) {
    let Some(cur) = ui.settings.borrow().clone() else {
        ui.toasts.add_toast(adw::Toast::new("Not connected to the download service yet."));
        return;
    };
    let p0 = ui.prefs.borrow().clone();
    let notebook = gtk::Notebook::builder().vexpand(true).hexpand(true).build();
    let status = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["error"]).wrap(true).build();
    let win = adw::Window::builder().transient_for(&ui.window).modal(false).title("Linux Download Manager Configuration").default_width(660).default_height(540).build();
    win.add_css_class("ldm-window");

    // ---------------- General ----------------
    let mut f = Form::new();
    f.heading("Browser and system integration");
    f.note("Tick a browser to let it talk to this program, then press Install: the browser opens and asks you to confirm (if it was already open, restart it first). Remove takes you to the extension's page in the browser, where you choose Remove there. ");
    f.note("Firefox: this add-on is not signed by Mozilla yet, so Install works only in Firefox Developer Edition or Nightly, and only after you open about:config and set xpinstall.signatures.required to false. Regular Firefox will refuse it.");
    let statuses = browser_status();
    let browser_checks: Vec<(String, bool, gtk::CheckButton)> = statuses
        .iter()
        .map(|(name, registered, found, ext, ext_id)| {
            let cb = check(&format!("Capture downloads from {} ({})", browser_title(name), if *found { "installed" } else { "not found" }), *registered);
            cb.set_hexpand(true);
            let text = if !*found {
                "Browser not installed"
            } else {
                match ext.as_str() {
                    "installed" => "Extension installed",
                    "disabled" => "Extension turned off",
                    _ => "Extension not installed",
                }
            };
            let state = gtk::Label::builder().label(text).css_classes(["dim-label"]).build();
            let firefox = name == "firefox";
            let install = gtk::Button::with_label(if firefox { "Install" } else if ext == "installed" { "Reinstall" } else { "Install" });
            install.set_sensitive(*found);
            let remove = gtk::Button::with_label("Remove");
            remove.set_sensitive(*found && ext != "missing");
            let (n, st) = (name.clone(), state.clone());
            install.connect_clicked(move |b| {
                if n == "firefox" {
                    // opening an .xpi makes Firefox ask whether to add it (Developer Edition / Nightly only)
                    let xpi = ["/usr/share/linux-download-manager/ldm.xpi", "extension/dist/ldm.xpi"].into_iter().find(|p| std::path::Path::new(p).is_file());
                    match xpi {
                        Some(x) => {
                            st.set_label("Confirm in Firefox");
                            launch_browser("firefox", x);
                        }
                        None => st.set_label("Add-on file not found"),
                    }
                    return;
                }
                let ok = nmhost().map(|e| std::process::Command::new(e).args(["ext-install", "--browser", &n]).output().is_ok_and(|o| o.status.success())).unwrap_or(false);
                if ok {
                    st.set_label("Restart the browser, then confirm");
                    b.set_sensitive(false);
                    launch_browser(&n, &format!("{}://extensions", browser_scheme(&n)));
                } else {
                    st.set_label("Could not prepare the install");
                }
            });
            // Removing is done in the browser itself: take the user to the extension's page there.
            let (n, id) = (name.clone(), ext_id.clone());
            remove.connect_clicked(move |_| {
                if n == "firefox" {
                    launch_browser("firefox", "about:addons");
                } else {
                    launch_browser(&n, &format!("{}://extensions/?id={id}", browser_scheme(&n)));
                }
            });
            let row = gtk::Box::builder().spacing(8).build();
            row.append(&cb);
            row.append(&state);
            row.append(&install);
            row.append(&remove);
            f.wide(&row);
            (name.clone(), *registered, cb)
        })
        .collect();
    if statuses.is_empty() {
        f.note("The browser helper (zdm-nmhost) was not found next to this program.");
    }
    let autostart0 = prefs::autostart_enabled();
    let autostart = check("Launch Linux Download Manager at login (hidden in the tray)", autostart0);
    let clip = check("Offer to download links I copy to the clipboard", p0.monitor_clipboard);
    let to_tray = check("Closing the window keeps the program running in the tray", p0.close_to_tray);
    let notes = check("Show a notification when a download finishes or fails", p0.notifications);
    f.heading("Program");
    for w in [&autostart, &clip, &to_tray, &notes] {
        f.wide(w);
    }
    f.heading("Browser buttons and keys");
    f.note("These are kept by the browser extension, so they open in the browser rather than here:\n•  the on-page “Download this video” button and its quality menu\n•  the keys that force or prevent a takeover (for example hold Alt to let the browser download)\n•  which file types are taken over");
    let ext_settings = gtk::Button::with_label("Open extension settings in the browser");
    {
        let first = statuses.iter().find(|s| s.2 && s.0 != "firefox").map(|s| s.0.clone());
        ext_settings.set_sensitive(first.is_some());
        ext_settings.connect_clicked(move |_| {
            if let Some(n) = &first {
                launch_browser(n, &format!("chrome-extension://{EXT_ID}/popup.html"));
            }
        });
    }
    f.wide(&ext_settings);
    notebook.append_page(&f.page(), Some(&gtk::Label::new(Some("General"))));

    // ---------------- File types ----------------
    let mut f = Form::new();
    f.heading("File types of each category");
    f.note("Extensions (without dots) separated by spaces. A file goes to the category whose list contains its extension; anything else goes to Other.");
    let type_rows: Vec<(String, gtk::Entry)> = zdm_proto::CATEGORY_TABLE
        .iter()
        .map(|(c, defaults)| {
            let e = gtk::Entry::builder().text(cur.category_exts.get(*c).map_or_else(|| defaults.join(" "), |v| v.join(" "))).build();
            f.row(c, &e);
            (c.to_string(), e)
        })
        .collect();
    let reset = gtk::Button::builder().label("Reset to the standard lists").halign(gtk::Align::Start).build();
    {
        let rows = type_rows.clone();
        reset.connect_clicked(move |_| {
            for (c, e) in &rows {
                if let Some((_, d)) = zdm_proto::CATEGORY_TABLE.iter().find(|(n, _)| n == c) {
                    e.set_text(&d.join(" "));
                }
            }
        });
    }
    f.wide(&reset);
    notebook.append_page(&f.page(), Some(&gtk::Label::new(Some("File types"))));

    // ---------------- Save to ----------------
    let mut f = Form::new();
    f.heading("Where downloads are saved");
    let root = gtk::Entry::builder().text(&cur.download_root).build();
    f.row("Main folder", &path_row(&win, &root, false));
    f.note("Each category gets a sub-folder of the main folder unless you choose another folder below.");
    let dir_rows: Vec<(String, gtk::Entry)> = zdm_proto::CATEGORY_TABLE
        .iter()
        .map(|(c, _)| *c)
        .chain(std::iter::once(zdm_proto::OTHER_CATEGORY))
        .map(|c| {
            let e = gtk::Entry::builder().text(cur.category_dirs.get(c).cloned().unwrap_or_default()).placeholder_text("(sub-folder of the main folder)").build();
            f.row(c, &path_row(&win, &e, false));
            (c.to_string(), e)
        })
        .collect();
    let exists_modes = ["rename", "overwrite", "skip"];
    let exists = gtk::DropDown::from_strings(&["Add a number to the new name", "Replace the old file", "Skip the download"]);
    exists.set_selected(exists_modes.iter().position(|m| *m == cur.on_exists).unwrap_or(0) as u32);
    f.heading("When a file with the same name exists");
    f.wide(&exists);
    notebook.append_page(&f.page(), Some(&gtk::Label::new(Some("Save to"))));

    // ---------------- Downloads ----------------
    let mut f = Form::new();
    f.heading("While downloading");
    let auto = check("Show the progress window when a download starts", p0.auto_progress);
    f.wide(&auto);
    let retries = spin(cur.auto_retries as u64, 0.0, 20.0);
    f.row("Automatic retries after a network error", &retries);
    f.heading("After each download");
    f.note("A shell command, for example a virus scan: clamscan \"$ZDM_FILE\". The finished file is in $ZDM_FILE. If it fails, the download is kept and a note is added.");
    let post = gtk::Entry::builder().text(cur.post_command.clone().unwrap_or_default()).placeholder_text("(none)").build();
    f.wide(&post);
    f.heading("Video sites");
    f.note("YouTube and some other sites ask for a sign-in or a \"not a robot\" check when a program (not a browser) asks for a video. The browser button avoids this by sending the page's own cookies. For “Download video from a web page” in this program, you can let yt-dlp use the cookies of one of your browsers. Only the cookies of the video site are used.");
    let cookie_browsers: Vec<Option<&str>> = std::iter::once(None).chain(zdm_proto::COOKIE_BROWSERS.iter().copied().map(Some)).collect();
    let cookie_dd = gtk::DropDown::from_strings(&["Do not use browser cookies", "Brave", "Google Chrome", "Chromium", "Firefox", "Microsoft Edge", "Vivaldi", "Opera"]);
    cookie_dd.set_selected(cookie_browsers.iter().position(|b| *b == cur.ytdlp_cookies_browser.as_deref()).unwrap_or(0) as u32);
    f.row("Use the cookies of this browser", &cookie_dd);
    notebook.append_page(&f.page(), Some(&gtk::Label::new(Some("Downloads"))));

    // ---------------- Connection ----------------
    let mut f = Form::new();
    f.heading("Connections and speed");
    let conns = spin(cur.connections as u64, 1.0, 32.0);
    f.row("Connections per download (1–32)", &conns);
    let limit = spin(cur.speed_limit_kib, 0.0, 10_000_000.0);
    f.row("Speed limit per download, KiB/s (0 = none)", &limit);
    let global = spin(cur.global_limit_kib, 0.0, 10_000_000.0);
    f.row("Speed limit for all downloads, KiB/s (0 = none)", &global);
    f.note("More connections usually finish large files sooner; a few servers refuse many. A queue can have its own limit in the Scheduler.");
    notebook.append_page(&f.page(), Some(&gtk::Label::new(Some("Connection"))));

    // ---------------- Proxy / Socks ----------------
    let mut f = Form::new();
    f.heading("Proxy / Socks");
    let pp = helpers::parse_proxy(cur.proxy.as_deref());
    let kinds = ["none", "http", "https", "socks5"];
    let kind = gtk::DropDown::from_strings(&["No proxy (use the system's settings)", "HTTP proxy", "HTTPS proxy", "SOCKS5 proxy"]);
    kind.set_selected(kinds.iter().position(|k| *k == pp.kind).unwrap_or(0) as u32);
    f.row("Type", &kind);
    let host = gtk::Entry::builder().text(&pp.host).build();
    let port = gtk::Entry::builder().text(&pp.port).max_width_chars(6).build();
    let puser = gtk::Entry::builder().text(&pp.user).build();
    let ppass = gtk::PasswordEntry::builder().show_peek_icon(true).build();
    ppass.set_text(&pp.pass);
    f.row("Host", &host);
    f.row("Port", &port);
    f.row("User name (optional)", &puser);
    f.row("Password (optional)", &ppass);
    {
        let sens: Vec<gtk::Widget> = vec![host.clone().upcast(), port.clone().upcast(), puser.clone().upcast(), ppass.clone().upcast()];
        let update = move |k: &gtk::DropDown| {
            for w in &sens {
                w.set_sensitive(k.selected() != 0);
            }
        };
        update(&kind);
        kind.connect_selected_notify(update);
    }
    notebook.append_page(&f.page(), Some(&gtk::Label::new(Some("Proxy / Socks"))));

    // ---------------- Sites Logins ----------------
    let logins: Rc<RefCell<Vec<SiteLogin>>> = Rc::new(RefCell::new(cur.site_logins.clone()));
    let store = gtk::ListStore::new(&[String::static_type(), String::static_type(), String::static_type()]);
    let refill = {
        let (store, logins) = (store.clone(), logins.clone());
        Rc::new(move || {
            store.clear();
            for l in logins.borrow().iter() {
                store.insert_with_values(None, &[(0, &l.host), (1, &l.user), (2, &"••••••••")]);
            }
        })
    };
    refill();
    let table = gtk::TreeView::with_model(&store);
    table.add_css_class("ldm-table");
    for (i, (title, w)) in [("Site", 240), ("User name", 160), ("Password", 100)].into_iter().enumerate() {
        let cell = gtk::CellRendererText::new();
        cell.set_padding(4, 1);
        let col = gtk::TreeViewColumn::builder().title(title).resizable(true).min_width(w).build();
        col.pack_start(&cell, true);
        col.add_attribute(&cell, "text", i as i32);
        table.append_column(&col);
    }
    let add = gtk::Button::with_label("Add…");
    let edit = gtk::Button::with_label("Edit…");
    let remove = gtk::Button::with_label("Remove");
    let side = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).valign(gtk::Align::Start).build();
    for b in [&add, &edit, &remove] {
        side.append(b);
    }
    let selected_index = {
        let table = table.clone();
        move || table.selection().selected_rows().0.first().and_then(|p| p.indices().first().copied()).map(|i| i as usize)
    };
    {
        let (win, logins, refill) = (win.clone(), logins.clone(), refill.clone());
        add.connect_clicked(move |_| {
            let (logins, refill) = (logins.clone(), refill.clone());
            login_dialog(&win, None, move |l| {
                logins.borrow_mut().retain(|x| x.host != l.host);
                logins.borrow_mut().push(l);
                refill();
            });
        });
    }
    {
        let (win, logins, refill, sel) = (win.clone(), logins.clone(), refill.clone(), selected_index.clone());
        edit.connect_clicked(move |_| {
            let Some(i) = sel() else { return };
            let Some(old) = logins.borrow().get(i).cloned() else { return };
            let (logins, refill) = (logins.clone(), refill.clone());
            login_dialog(&win, Some(old), move |l| {
                if let Some(slot) = logins.borrow_mut().get_mut(i) {
                    *slot = l;
                }
                refill();
            });
        });
    }
    {
        let (logins, refill, sel) = (logins.clone(), refill.clone(), selected_index.clone());
        remove.connect_clicked(move |_| {
            if let Some(i) = sel() {
                if i < logins.borrow().len() {
                    logins.borrow_mut().remove(i);
                }
                refill();
            }
        });
    }
    let scroll = gtk::ScrolledWindow::builder().child(&table).vexpand(true).hexpand(true).has_frame(true).build();
    let body = gtk::Box::builder().spacing(8).margin_top(12).margin_bottom(12).margin_start(14).margin_end(14).build();
    body.append(&scroll);
    body.append(&side);
    let logins_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let hint = gtk::Label::builder().label("Logins used automatically for downloads from these sites (and their subdomains) that do not carry a login of their own. Basic and Digest are supported.").xalign(0.0).wrap(true).css_classes(["dim-label"]).margin_top(10).margin_start(14).margin_end(14).build();
    logins_page.append(&hint);
    logins_page.append(&body);
    notebook.append_page(&logins_page, Some(&gtk::Label::new(Some("Sites Logins"))));

    // ---------------- Sounds ----------------
    let mut f = Form::new();
    f.heading("Sounds");
    f.note("Leave the file empty for the desktop's standard sound for that event.");
    let sound_rows: Vec<(SoundEvent, gtk::CheckButton, gtk::Entry)> = [
        (SoundEvent::Complete, "Download complete", &p0.sound_complete),
        (SoundEvent::Failed, "Download failed", &p0.sound_failed),
        (SoundEvent::AllDone, "All downloads finished", &p0.sound_all_done),
    ]
    .into_iter()
    .map(|(ev, title, saved)| {
        let on = check(title, !saved.is_empty());
        let e = gtk::Entry::builder().text(if saved == "default" { "" } else { saved }).placeholder_text(sound::default_file(ev)).build();
        let play = gtk::Button::with_label("Play");
        {
            let e = e.clone();
            play.connect_clicked(move |_| {
                let f = if e.text().trim().is_empty() { sound::default_file(ev).to_string() } else { e.text().trim().to_string() };
                sound::play(&f);
            });
        }
        let line = gtk::Box::builder().spacing(6).build();
        line.append(&path_row(&win, &e, true));
        line.append(&play);
        f.wide(&on);
        f.wide(&line);
        (ev, on, e)
    })
    .collect();
    notebook.append_page(&f.page(), Some(&gtk::Label::new(Some("Sounds"))));

    // ---------------- buttons and applying ----------------
    let ok = gtk::Button::builder().label("OK").css_classes(["suggested-action"]).build();
    let cancel = gtk::Button::with_label("Cancel");
    let buttons = gtk::Box::builder().spacing(8).margin_top(8).margin_bottom(10).margin_start(14).margin_end(14).build();
    buttons.append(&status);
    buttons.append(&cancel);
    buttons.append(&ok);
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    content.append(&notebook);
    content.append(&buttons);
    let header = adw::HeaderBar::new();
    header.add_css_class("ldm-header");
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&content));
    win.set_content(Some(&tv));

    {
        let w = win.clone();
        cancel.connect_clicked(move |_| w.close());
    }
    {
        let (u, w, nb) = (ui.clone(), win.clone(), notebook.clone());
        ok.connect_clicked(move |_| {
            // proxy first: a mistake here keeps the window open on its tab
            let proxy = match helpers::compose_proxy(&ProxyParts { kind: kinds[(kind.selected() as usize).min(3)].into(), host: host.text().to_string(), port: port.text().trim().to_string(), user: puser.text().to_string(), pass: ppass.text().to_string() }) {
                Ok(p) => p,
                Err(e) => {
                    status.set_text(&e);
                    nb.set_current_page(Some(5));
                    return;
                }
            };
            let mut s: Settings = cur.clone();
            s.download_root = root.text().trim().to_string();
            s.on_exists = exists_modes[(exists.selected() as usize).min(2)].to_string();
            s.category_dirs = dir_rows.iter().filter_map(|(c, e)| Some((c.clone(), e.text().trim().to_string())).filter(|(_, d)| !d.is_empty())).collect();
            let mut exts = cur.category_exts.clone();
            for (cat, e) in &type_rows {
                let typed = helpers::parse_extensions(&e.text());
                let default: Vec<String> = zdm_proto::CATEGORY_TABLE.iter().find(|(c, _)| c == cat).map(|(_, d)| d.iter().map(|x| x.to_string()).collect()).unwrap_or_default();
                if typed.is_empty() || typed == default {
                    exts.remove(cat);
                } else {
                    exts.insert(cat.clone(), typed);
                }
            }
            s.category_exts = exts;
            for sp in [&retries, &conns, &limit, &global] {
                sp.update();
            }
            s.auto_retries = retries.value() as u32;
            s.connections = conns.value() as u32;
            s.speed_limit_kib = limit.value() as u64;
            s.global_limit_kib = global.value() as u64;
            let command = post.text().trim().to_string();
            s.post_command = (!command.is_empty()).then_some(command);
            s.proxy = proxy;
            s.ytdlp_cookies_browser = cookie_browsers.get(cookie_dd.selected() as usize).copied().flatten().map(str::to_string);
            s.site_logins = logins.borrow().clone();
            if let Err(e) = s.validate() {
                status.set_text(&e);
                return;
            }
            if s != cur {
                send(&u, Request::SetSettings(s));
            }
            // window preferences
            let sound_setting = |on: &gtk::CheckButton, e: &gtk::Entry| if !on.is_active() { String::new() } else if e.text().trim().is_empty() { "default".to_string() } else { e.text().trim().to_string() };
            let (sc, sf, sa) = (sound_setting(&sound_rows[0].1, &sound_rows[0].2), sound_setting(&sound_rows[1].1, &sound_rows[1].2), sound_setting(&sound_rows[2].1, &sound_rows[2].2));
            crate::look::change(&u, |p| {
                p.auto_progress = auto.is_active();
                p.close_to_tray = to_tray.is_active();
                p.notifications = notes.is_active();
                p.monitor_clipboard = clip.is_active();
                (p.sound_complete, p.sound_failed, p.sound_all_done) = (sc, sf, sa);
            });
            // start at login
            if autostart.is_active() != autostart0 {
                if let Err(e) = prefs::set_autostart(autostart.is_active()) {
                    u.toasts.add_toast(adw::Toast::new(&format!("Could not change the start-at-login entry: {e}")));
                }
            }
            // browsers whose box changed
            if let Some(exe) = nmhost() {
                for (name, was, cb) in &browser_checks {
                    if cb.is_active() != *was {
                        let cmd = if cb.is_active() { "install" } else { "uninstall" };
                        let _ = std::process::Command::new(&exe).args([cmd, "--browser", name]).output();
                    }
                }
            }
            u.toasts.add_toast(adw::Toast::new("Options saved. Speed, connection and folder settings apply to downloads started from now on."));
            w.close();
        });
    }
    if let Some(n) = std::env::var("LDM_OPTIONS_TAB").ok().and_then(|n| n.parse::<u32>().ok()) {
        notebook.set_current_page(Some(n)); // debugging aid: open on a given tab
    }
    win.present();
}
