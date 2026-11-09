//! The per-download window: tabs for status, speed limiter and what to do on completion, a progress bar,
//! Pause / Cancel / Hide details, and (when expanded) one row per connection.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tokio::sync::mpsc::UnboundedSender;
use zdm_proto::{AfterAll, Request, State, TaskInfo};

use crate::fmt;
use crate::segments::Layout;

const PALETTE: [(f64, f64, f64); 8] =
    [(0.18, 0.66, 0.29), (0.08, 0.47, 0.84), (0.90, 0.54, 0.0), (0.56, 0.27, 0.68), (0.0, 0.62, 0.62), (0.84, 0.23, 0.16), (0.82, 0.60, 0.0), (0.85, 0.3, 0.6)];

/// Rows in the connections table.
const ROWS: usize = 8;

/// What the window may change in the daemon besides its own download.
pub struct Hooks {
    /// Current global speed limit in KiB/s (0 = none) and a way to set it.
    pub global_limit_kib: u64,
    pub set_limit: Rc<dyn Fn(u64)>,
    /// Current "when all downloads finish" action and a way to set it.
    pub after_all: AfterAll,
    pub set_after_all: Rc<dyn Fn(AfterAll)>,
}

/// What `update` found out.
pub struct Update {
    /// The download finished since the last update.
    pub just_completed: bool,
    /// The user wants the "download complete" window for this one.
    pub show_complete: bool,
}

pub struct ProgressWin {
    pub win: adw::Window,
    url: gtk::Label,
    status: gtk::Label,
    values: [gtk::Label; 5], // size, downloaded, rate, left, resume
    bar: gtk::ProgressBar,
    area: gtk::DrawingArea,
    layout: Rc<RefCell<Option<Layout>>>,
    full: Rc<Cell<bool>>,
    rows: Vec<(gtk::Label, gtk::Label, gtk::Label)>,
    toggle: gtk::Button,
    show_done: gtk::CheckButton,
    state: Cell<State>,
}

fn kv_grid(names: &[&str]) -> (gtk::Grid, Vec<gtk::Label>) {
    let grid = gtk::Grid::builder().row_spacing(6).column_spacing(20).margin_top(6).margin_bottom(6).margin_start(8).margin_end(8).build();
    let values = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            grid.attach(&gtk::Label::builder().label(*n).xalign(0.0).build(), 0, i as i32, 1, 1);
            let v = gtk::Label::builder().xalign(0.0).hexpand(true).selectable(true).build();
            grid.attach(&v, 1, i as i32, 1, 1);
            v
        })
        .collect();
    (grid, values)
}

impl ProgressWin {
    pub fn new(parent: &adw::ApplicationWindow, id: i64, cmd_tx: UnboundedSender<Request>, hooks: Hooks, show_complete_default: bool, on_close: impl Fn() + 'static) -> Rc<ProgressWin> {
        // ---- tab 1: status ----
        let url = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).selectable(true).margin_start(8).margin_end(8).margin_top(6).build();
        let status = gtk::Label::builder().xalign(0.0).css_classes(["accent"]).build();
        let (grid, vals) = kv_grid(&["Status", "File size", "Downloaded", "Transfer rate", "Time left", "Resume capability"]);
        // the first value row is the status label, kept separately so it can be coloured
        grid.remove(&vals[0]);
        grid.attach(&status, 1, 0, 1, 1);
        let values: [gtk::Label; 5] = std::array::from_fn(|i| vals[i + 1].clone());
        let status_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        status_page.append(&url);
        status_page.append(&grid);

        // ---- tab 2: speed limiter ----
        let limit_on = gtk::CheckButton::with_label("Use the speed limiter (applies to all downloads)");
        limit_on.set_active(hooks.global_limit_kib > 0);
        let limit_kib = gtk::SpinButton::with_range(8.0, 1_000_000.0, 8.0);
        limit_kib.set_value(if hooks.global_limit_kib > 0 { hooks.global_limit_kib as f64 } else { 1024.0 });
        limit_kib.set_sensitive(hooks.global_limit_kib > 0);
        let limit_row = gtk::Box::builder().spacing(8).margin_start(26).build();
        limit_row.append(&gtk::Label::new(Some("Maximum speed (KiB/s)")));
        limit_row.append(&limit_kib);
        let apply_limit = {
            let (on, kib, set) = (limit_on.clone(), limit_kib.clone(), hooks.set_limit.clone());
            move || set(if on.is_active() { kib.value() as u64 } else { 0 })
        };
        {
            let (kib, apply) = (limit_kib.clone(), apply_limit.clone());
            limit_on.connect_toggled(move |c| {
                kib.set_sensitive(c.is_active());
                apply();
            });
        }
        limit_kib.connect_value_changed(move |_| apply_limit());
        let limit_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).margin_top(14).margin_start(10).margin_end(10).build();
        limit_page.append(&limit_on);
        limit_page.append(&limit_row);

        // ---- tab 3: on completion ----
        let show_done = gtk::CheckButton::with_label("Show the “Download complete” window when this finishes");
        show_done.set_active(show_complete_default);
        let after_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_start(26).build();
        let after_title = gtk::Label::builder().label("When all downloads have finished:").xalign(0.0).build();
        let nothing = gtk::CheckButton::with_label("Do nothing");
        let sleep = gtk::CheckButton::with_label("Put the computer to sleep");
        let shutdown = gtk::CheckButton::with_label("Turn the computer off");
        sleep.set_group(Some(&nothing));
        shutdown.set_group(Some(&nothing));
        match hooks.after_all {
            AfterAll::Sleep => sleep.set_active(true),
            AfterAll::Shutdown => shutdown.set_active(true),
            _ => nothing.set_active(true),
        }
        for (b, a) in [(&nothing, AfterAll::Nothing), (&sleep, AfterAll::Sleep), (&shutdown, AfterAll::Shutdown)] {
            let set = hooks.set_after_all.clone();
            b.connect_toggled(move |c| {
                if c.is_active() {
                    set(a.clone());
                }
            });
        }
        for w in [&after_title as &gtk::Label].into_iter().map(|l| l.upcast_ref::<gtk::Widget>()).chain([nothing.upcast_ref(), sleep.upcast_ref(), shutdown.upcast_ref()]) {
            after_box.append(w);
        }
        let done_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(14).margin_start(10).margin_end(10).build();
        done_page.append(&show_done);
        done_page.append(&after_box);

        let notebook = gtk::Notebook::new();
        notebook.append_page(&status_page, Some(&gtk::Label::new(Some("Download status"))));
        notebook.append_page(&limit_page, Some(&gtk::Label::new(Some("Speed limiter"))));
        notebook.append_page(&done_page, Some(&gtk::Label::new(Some("Options on completion"))));

        // ---- progress bar and buttons ----
        let bar = gtk::ProgressBar::new();
        bar.add_css_class("ldm-green");
        bar.set_height_request(20);
        let details = gtk::Button::with_label("<< Hide details");
        let toggle = gtk::Button::with_label("Pause");
        let cancel = gtk::Button::with_label("Cancel");
        let spacer = gtk::Box::builder().hexpand(true).build();
        let buttons = gtk::Box::builder().spacing(10).build();
        for w in [details.upcast_ref::<gtk::Widget>(), spacer.upcast_ref(), toggle.upcast_ref(), cancel.upcast_ref()] {
            buttons.append(w);
        }

        // ---- connection details ----
        let layout: Rc<RefCell<Option<Layout>>> = Rc::default();
        let full = Rc::new(Cell::new(false));
        let area = gtk::DrawingArea::builder().content_height(16).hexpand(true).build();
        {
            let (layout, full) = (layout.clone(), full.clone());
            area.set_draw_func(move |_, cr, w, h| {
                let (w, h) = (w as f64, h as f64);
                cr.set_source_rgb(0.86, 0.90, 0.96);
                cr.rectangle(0.0, 0.0, w, h);
                let _ = cr.fill();
                if full.get() {
                    cr.set_source_rgb(0.18, 0.66, 0.29);
                    cr.rectangle(0.0, 0.0, w, h);
                    let _ = cr.fill();
                } else if let Some(l) = &*layout.borrow() {
                    let size = l.size.max(1) as f64;
                    for (i, s) in l.segs.iter().enumerate() {
                        let x0 = s.start as f64 / size * w;
                        let x1 = s.pos.clamp(s.start, s.end) as f64 / size * w;
                        let (r, g, b) = PALETTE[i % PALETTE.len()];
                        cr.set_source_rgb(r, g, b);
                        cr.rectangle(x0, 0.0, (x1 - x0).max(0.0), h);
                        let _ = cr.fill();
                        cr.set_source_rgb(0.75, 0.15, 0.15);
                        cr.rectangle(s.start as f64 / size * w, 0.0, 1.0, h);
                        let _ = cr.fill();
                    }
                }
            });
        }
        let caption = gtk::Label::builder().label("Start positions and download progress by connections").xalign(0.5).build();
        let table = gtk::Grid::builder().column_spacing(0).row_spacing(0).hexpand(true).build();
        table.add_css_class("ldm-conn-table");
        for (c, h) in ["N.", "Downloaded", "Info"].iter().enumerate() {
            let l = gtk::Label::builder().label(*h).xalign(0.0).css_classes(["heading"]).margin_start(8).margin_end(8).margin_top(4).margin_bottom(4).build();
            table.attach(&l, c as i32, 0, 1, 1);
        }
        let rows: Vec<(gtk::Label, gtk::Label, gtk::Label)> = (0..ROWS)
            .map(|r| {
                let mk = |c: i32, expand: bool| {
                    let l = gtk::Label::builder().xalign(0.0).margin_start(8).margin_end(8).margin_top(3).margin_bottom(3).hexpand(expand).build();
                    table.attach(&l, c, r as i32 + 1, 1, 1);
                    l
                };
                (mk(0, false), mk(1, false), mk(2, true))
            })
            .collect();
        let scroll = gtk::ScrolledWindow::builder().child(&table).min_content_height(190).hscrollbar_policy(gtk::PolicyType::Never).build();
        scroll.add_css_class("ldm-conn-scroll");
        let detail_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        detail_box.append(&caption);
        detail_box.append(&area);
        detail_box.append(&scroll);
        let reveal = gtk::Revealer::builder().child(&detail_box).reveal_child(true).transition_type(gtk::RevealerTransitionType::SlideDown).build();

        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(14).margin_bottom(14).margin_start(16).margin_end(16).build();
        for w in [notebook.upcast_ref::<gtk::Widget>(), bar.upcast_ref(), buttons.upcast_ref(), reveal.upcast_ref()] {
            content.append(w);
        }
        let header = adw::HeaderBar::new();
        header.add_css_class("ldm-header");
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        tv.set_content(Some(&content));
        let win = adw::Window::builder().transient_for(parent).destroy_with_parent(true).title("Download progress").default_width(620).content(&tv).build();
        win.add_css_class("ldm-window");

        let pw = Rc::new(ProgressWin { win, url, status, values, bar, area, layout, full, rows, toggle, show_done, state: Cell::new(State::Completed) });
        {
            let (r, d) = (reveal.clone(), details.clone());
            details.connect_clicked(move |_| {
                let open = !r.reveals_child();
                r.set_reveal_child(open);
                d.set_label(if open { "<< Hide details" } else { "Details >>" });
            });
        }
        {
            let (pw2, tx) = (pw.clone(), cmd_tx.clone());
            pw.toggle.connect_clicked(move |_| {
                let r = if matches!(pw2.state.get(), State::Running | State::Queued) { Request::Pause { id } } else { Request::Resume { id } };
                let _ = tx.send(r);
            });
        }
        {
            // Cancel stops the download (what was fetched is kept, so it can be resumed from the list) and closes the window
            let (w, tx) = (pw.win.clone(), cmd_tx);
            cancel.connect_clicked(move |_| {
                let _ = tx.send(Request::Pause { id });
                w.close();
            });
        }
        pw.win.connect_close_request(move |_| {
            on_close();
            glib::Propagation::Proceed
        });
        pw
    }

    pub fn show_complete_wanted(&self) -> bool {
        self.show_done.is_active()
    }

    pub fn update(&self, t: &TaskInfo, speed: f64) -> Update {
        let was = self.state.replace(t.state);
        let name = t.dest.as_deref().and_then(|d| std::path::Path::new(d).file_name()).map(|n| n.to_string_lossy().into_owned()).or_else(|| t.filename.clone()).unwrap_or_else(|| t.url.clone());
        self.win.set_title(Some(&format!("{} {}", fmt::status_label(t), name)));
        self.url.set_text(&t.url);
        self.status.set_text(match t.state {
            State::Running => "Receiving data…",
            State::Queued => "Waiting in queue",
            State::Paused => "Paused",
            State::Completed => "Download complete",
            State::Failed => "Download failed",
        });
        self.bar.set_fraction(fmt::percent(t) as f64 / 100.0);
        let layout = t.dest.as_deref().filter(|_| t.state != State::Completed).and_then(Layout::read_for);
        let pct = if t.size.unwrap_or(0) > 0 { format!("{} ({:.2} %)", fmt::bytes(t.downloaded), t.downloaded as f64 * 100.0 / t.size.unwrap_or(1) as f64) } else { fmt::bytes(t.downloaded) };
        self.values[0].set_text(&fmt::size(t));
        self.values[1].set_text(&pct);
        self.values[2].set_text(&fmt::rate(t, speed));
        self.values[3].set_text(&fmt::time_left(t, speed));
        self.values[4].set_text(match (&layout, t.size) {
            (Some(l), _) if l.segs.len() > 1 => "Yes",
            (_, Some(_)) if t.state == State::Completed => "",
            _ => "Unknown",
        });
        // one row per connection (the resume file lists them)
        let running = t.state == State::Running;
        for (i, (n, got, info)) in self.rows.iter().enumerate() {
            match layout.as_ref().and_then(|l| l.segs.get(i)) {
                Some(s) => {
                    n.set_text(&(i + 1).to_string());
                    got.set_text(&if s.pos > s.start { fmt::bytes(s.pos - s.start) } else { String::new() });
                    info.set_text(if s.pos >= s.end { "Finished" } else if running { "Receiving data…" } else { "" });
                }
                None => {
                    n.set_text(&if i < 8 { (i + 1).to_string() } else { String::new() });
                    got.set_text("");
                    info.set_text("");
                }
            }
        }
        self.full.set(t.state == State::Completed);
        *self.layout.borrow_mut() = layout;
        self.area.queue_draw();
        self.toggle.set_label(if matches!(t.state, State::Running | State::Queued) { "Pause" } else { "Resume" });
        self.toggle.set_sensitive(t.state != State::Completed);
        let just_completed = t.state == State::Completed && was != State::Completed;
        Update { just_completed, show_complete: just_completed && self.show_done.is_active() }
    }
}
