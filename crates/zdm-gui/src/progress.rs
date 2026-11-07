//! The small per-download window: percentage, speed, time left and one bar per connection.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tokio::sync::mpsc::UnboundedSender;
use zdm_proto::{Request, State, TaskInfo};

use crate::fmt;
use crate::segments::Layout;

const PALETTE: [(f64, f64, f64); 8] =
    [(0.18, 0.66, 0.29), (0.08, 0.47, 0.84), (0.90, 0.54, 0.0), (0.56, 0.27, 0.68), (0.0, 0.62, 0.62), (0.84, 0.23, 0.16), (0.82, 0.60, 0.0), (0.85, 0.3, 0.6)];

/// How many one-second samples the speed graph shows.
const SAMPLES: usize = 60;

pub struct ProgressWin {
    pub win: adw::Window,
    title: gtk::Label,
    url: gtk::Label,
    status: gtk::Label,
    bar: gtk::ProgressBar,
    values: [gtk::Label; 5], // size, downloaded, rate, left, connections
    area: gtk::DrawingArea,
    layout: Rc<RefCell<Option<Layout>>>,
    full: Rc<Cell<bool>>,
    graph: gtk::DrawingArea,
    history: Rc<RefCell<VecDeque<f64>>>,
    toggle: gtk::Button,
    close_when_done: gtk::CheckButton,
    state: Cell<State>,
}

impl ProgressWin {
    pub fn new(parent: &adw::ApplicationWindow, id: i64, cmd_tx: UnboundedSender<Request>, on_close: impl Fn() + 'static) -> Rc<ProgressWin> {
        let title = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).css_classes(["title-3"]).build();
        let url = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).css_classes(["dim-label", "caption"]).selectable(true).build();
        let status = gtk::Label::builder().xalign(0.0).css_classes(["heading"]).build();
        let bar = gtk::ProgressBar::builder().show_text(true).build();
        bar.set_height_request(22);

        let grid = gtk::Grid::builder().row_spacing(4).column_spacing(18).build();
        let names = ["File size", "Downloaded", "Transfer rate", "Time left", "Connections"];
        let values: [gtk::Label; 5] = std::array::from_fn(|i| {
            grid.attach(&gtk::Label::builder().label(names[i]).xalign(0.0).css_classes(["dim-label"]).build(), 0, i as i32, 1, 1);
            let v = gtk::Label::builder().xalign(0.0).hexpand(true).selectable(true).build();
            grid.attach(&v, 1, i as i32, 1, 1);
            v
        });

        let layout: Rc<RefCell<Option<Layout>>> = Rc::default();
        let full = Rc::new(Cell::new(false));
        let area = gtk::DrawingArea::builder().content_height(28).hexpand(true).build();
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
                        cr.set_source_rgb(1.0, 1.0, 1.0);
                        cr.rectangle(s.end as f64 / size * w - 0.5, 0.0, 1.0, h);
                        let _ = cr.fill();
                    }
                }
            });
        }
        // speed over the last minute, one sample per second
        let history: Rc<RefCell<VecDeque<f64>>> = Rc::default();
        let graph = gtk::DrawingArea::builder().content_height(64).hexpand(true).build();
        {
            let history = history.clone();
            graph.set_draw_func(move |_, cr, w, h| {
                let (w, h) = (w as f64, h as f64);
                cr.set_source_rgb(0.93, 0.96, 0.99);
                cr.rectangle(0.0, 0.0, w, h);
                let _ = cr.fill();
                let hist = history.borrow();
                let peak = hist.iter().cloned().fold(1024.0, f64::max);
                // grid: three faint lines
                cr.set_source_rgba(0.55, 0.65, 0.8, 0.35);
                cr.set_line_width(1.0);
                for i in 1..4 {
                    let y = h * i as f64 / 4.0;
                    cr.move_to(0.0, y);
                    cr.line_to(w, y);
                    let _ = cr.stroke();
                }
                if hist.len() >= 2 {
                    let step = w / (SAMPLES as f64 - 1.0);
                    let x0 = w - step * (hist.len() as f64 - 1.0);
                    cr.move_to(x0, h);
                    for (i, v) in hist.iter().enumerate() {
                        cr.line_to(x0 + step * i as f64, h - (v / peak) * (h - 4.0));
                    }
                    cr.line_to(w, h);
                    cr.close_path();
                    cr.set_source_rgba(0.18, 0.66, 0.29, 0.55);
                    let _ = cr.fill_preserve();
                    cr.set_source_rgb(0.12, 0.5, 0.2);
                    cr.set_line_width(1.5);
                    let _ = cr.stroke();
                }
            });
        }
        let graph_caption = gtk::Label::builder().label("Speed, last minute").xalign(0.0).css_classes(["dim-label", "caption"]).build();
        let area_caption = gtk::Label::builder().label("Progress by connection").xalign(0.0).css_classes(["dim-label", "caption"]).build();

        let toggle = gtk::Button::builder().label("Pause").build();
        let hide = gtk::Button::builder().label("Hide").build();
        let close_when_done = gtk::CheckButton::builder().label("Close this window when finished").hexpand(true).build();
        let buttons = gtk::Box::builder().spacing(8).build();
        buttons.append(&close_when_done);
        buttons.append(&toggle);
        buttons.append(&hide);

        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).margin_top(14).margin_bottom(14).margin_start(18).margin_end(18).build();
        for w in [title.upcast_ref::<gtk::Widget>(), url.upcast_ref(), status.upcast_ref(), bar.upcast_ref(), grid.upcast_ref(), area_caption.upcast_ref(), area.upcast_ref(), graph_caption.upcast_ref(), graph.upcast_ref(), buttons.upcast_ref()] {
            content.append(w);
        }
        let header = adw::HeaderBar::new();
        header.add_css_class("ldm-header");
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        tv.set_content(Some(&content));
        let win = adw::Window::builder().transient_for(parent).destroy_with_parent(true).title("Download progress").default_width(520).content(&tv).build();

        let pw = Rc::new(ProgressWin { win, title, url, status, bar, values, area, layout, full, graph, history, toggle, close_when_done, state: Cell::new(State::Queued) });
        {
            let pw2 = pw.clone();
            pw.toggle.connect_clicked(move |_| {
                let r = if matches!(pw2.state.get(), State::Running | State::Queued) { Request::Pause { id } } else { Request::Resume { id } };
                let _ = cmd_tx.send(r);
            });
        }
        {
            let w = pw.win.clone();
            hide.connect_clicked(move |_| w.close());
        }
        pw.win.connect_close_request(move |_| {
            on_close();
            glib::Propagation::Proceed
        });
        pw
    }

    /// Returns true when the window should close itself (finished and the box is ticked).
    pub fn update(&self, t: &TaskInfo, speed: f64) -> bool {
        self.state.set(t.state);
        let name = t.dest.as_deref().and_then(|d| std::path::Path::new(d).file_name()).map(|n| n.to_string_lossy().into_owned()).or_else(|| t.filename.clone()).unwrap_or_else(|| t.url.clone());
        self.title.set_text(&name);
        self.win.set_title(Some(&format!("{} — {}", fmt::status_label(t), name)));
        self.url.set_text(&t.url);
        self.status.set_text(match t.state {
            State::Running => "Receiving data…",
            State::Queued => "Waiting in queue",
            State::Paused => "Paused",
            State::Completed => "Download complete",
            State::Failed => "Download failed",
        });
        self.bar.set_fraction(fmt::percent(t) as f64 / 100.0);
        self.bar.set_text(Some(&fmt::status_label(t)));
        let layout = t.dest.as_deref().filter(|_| t.state != State::Completed).and_then(Layout::read_for);
        self.values[0].set_text(&fmt::size(t));
        self.values[1].set_text(&fmt::bytes(t.downloaded));
        self.values[2].set_text(&fmt::rate(t, speed));
        self.values[3].set_text(&fmt::time_left(t, speed));
        self.values[4].set_text(&match (&layout, t.state) {
            (Some(l), State::Running) => l.active().to_string(),
            _ => String::new(),
        });
        self.full.set(t.state == State::Completed);
        *self.layout.borrow_mut() = layout;
        self.area.queue_draw();
        {
            let mut h = self.history.borrow_mut();
            h.push_back(if t.state == State::Running { speed } else { 0.0 });
            while h.len() > SAMPLES {
                h.pop_front();
            }
        }
        self.graph.queue_draw();
        self.toggle.set_label(if matches!(t.state, State::Running | State::Queued) { "Pause" } else { "Resume" });
        self.toggle.set_sensitive(t.state != State::Completed);
        t.state == State::Completed && self.close_when_done.is_active()
    }
}
