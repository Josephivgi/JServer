//! The window. Laid out like the SSH page in JAgents: a sidebar with the
//! server list and keys, and a main area with a sticky header and cards.

use crate::describe::Kind;
use crate::insights::{self, Insight, Level};
use crate::sshconf::{self, Scan};
use crate::theme::{self, Tokens};
use crate::update::{self, Updater};
use crate::{autostart, collector, db, fmt, paths};
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, RichText, Sense, Stroke, Ui, UiBuilder, vec2};
use egui_plot::{Line, Plot, PlotPoints, Points, VLine};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub fn run() -> Result<(), String> {
    run_with(None)
}

/// Opens the window and saves PNGs of each page into `dir`, then closes.
pub fn run_capture(dir: std::path::PathBuf) -> Result<(), String> {
    run_with(Some(Capture::new(dir)))
}

struct Step {
    name: String,
    page: Page,
    scroll: f32,
    dark: bool,
    size: Option<egui::Vec2>,
}

struct Capture {
    dir: std::path::PathBuf,
    steps: Vec<Step>,
    idx: usize,
    started: Instant,
    settle_until: Option<Instant>,
    waiting: bool,
    tx: mpsc::Sender<std::sync::Arc<egui::ColorImage>>,
    rx: mpsc::Receiver<std::sync::Arc<egui::ColorImage>>,
}

impl Capture {
    fn new(dir: std::path::PathBuf) -> Self {
        let mut steps = Vec::new();
        let mut add = |name: &str, page: Page, scroll: f32, dark: bool, size: Option<egui::Vec2>| steps.push(Step { name: name.into(), page, scroll, dark, size });
        add("01-overview", Page::Overview, 0.0, false, None);
        add("02-overview-keys", Page::Overview, 700.0, false, None);
        for (i, a) in sshconf::aliases().iter().enumerate() {
            for (j, off) in [0.0, 600.0, 1200.0, 1800.0].iter().enumerate() {
                add(&format!("1{i}-{a}-{j}"), Page::Server(a.clone()), *off, false, None);
            }
        }
        add("30-keys", Page::Keys, 0.0, false, None);
        add("31-keys-table", Page::Keys, 600.0, false, None);
        add("40-overview-dark", Page::Overview, 0.0, true, None);
        if let Some(a) = sshconf::aliases().into_iter().nth(1) {
            add("41-server-dark", Page::Server(a), 0.0, true, None);
        }
        add("50-overview-narrow", Page::Overview, 0.0, false, Some(egui::vec2(960.0, 640.0)));
        add("51-keys-narrow", Page::Keys, 500.0, false, Some(egui::vec2(960.0, 640.0)));
        let (tx, rx) = mpsc::channel();
        Self { dir, steps, idx: 0, started: Instant::now(), settle_until: None, waiting: false, tx, rx }
    }
}

fn run_with(capture: Option<Capture>) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("JServer")
            .with_inner_size([1240.0, 820.0])
            .with_min_inner_size([960.0, 620.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).unwrap_or_default()),
        ..Default::default()
    };
    eframe::run_native(
        "JServer",
        options,
        Box::new(|cc| {
            let mut app = App::new(cc)?;
            app.capture = capture;
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| e.to_string())
}

#[derive(Clone, PartialEq, Eq)]
enum Page {
    Overview,
    Server(String),
    Keys,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    H1,
    H6,
    H24,
    D7,
    D30,
}

impl Range {
    const ALL: [Range; 5] = [Range::H1, Range::H6, Range::H24, Range::D7, Range::D30];
    fn secs(self) -> i64 {
        match self {
            Range::H1 => 3600,
            Range::H6 => 6 * 3600,
            Range::H24 => 86_400,
            Range::D7 => 7 * 86_400,
            Range::D30 => 30 * 86_400,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Range::H1 => "1 hour",
            Range::H6 => "6 hours",
            Range::H24 => "24 hours",
            Range::D7 => "7 days",
            Range::D30 => "30 days",
        }
    }
    /// Bucket size giving roughly 360 points across the range.
    fn bucket(self) -> i64 {
        (self.secs() / 360).max(db::INTERVAL as i64)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Memory,
    Cpu,
}

/// The tone of a status dot, like `.ssh-dot` in JAgents.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    None,
    Ok,
    Busy,
    Warn,
    Danger,
}

fn tone_of(status: &str) -> Tone {
    match status {
        "up" => Tone::Ok,
        "connecting" => Tone::Busy,
        "timeout" | "hostkey" | "lost" => Tone::Warn,
        "" | "paused" => Tone::None,
        _ => Tone::Danger,
    }
}

fn tone_color(t: &Tokens, tone: Tone) -> Color32 {
    match tone {
        Tone::None => t.muted,
        Tone::Ok => t.ok,
        Tone::Busy => t.accent,
        Tone::Warn => t.warn,
        Tone::Danger => t.danger,
    }
}

fn level_color(t: &Tokens, l: Level) -> Color32 {
    match l {
        Level::Info => t.muted,
        Level::Warn => t.warn,
        Level::Danger => t.danger,
    }
}

/// Everything the window shows about one server, refreshed every 2 seconds.
#[derive(Default)]
struct View {
    srv: db::Server,
    latest: Option<db::Point>,
    groups: Vec<db::LiveGroup>,
    mounts: Vec<db::MountRow>,
    findings: Vec<Insight>,
}

impl View {
    fn fresh(&self) -> Option<&db::Point> {
        self.latest.as_ref().filter(|p| chrono::Utc::now().timestamp() - p.ts < 60 && self.srv.status == "up")
    }
    fn label(&self) -> &str {
        match self.srv.status.as_str() {
            "" => "Not checked",
            "connecting" => "Connecting…",
            _ => &self.srv.label,
        }
    }
}

struct App {
    conn: Connection,
    page: Page,
    range: Range,

    scan: Option<Scan>,
    scan_rx: Option<mpsc::Receiver<Scan>>,
    views: Vec<View>,
    last_live: Option<Instant>,
    last_findings: Option<Instant>,
    last_range: Option<Instant>,
    range_dirty: bool,
    history: Vec<db::Point>,
    history_for: String,
    tips: TipCache,
    sort: Sort,
    hover_moment: Option<i64>,
    /// Card heights from the last frame, so cards in a row line up.
    row_heights: HashMap<usize, f32>,
    scroll_to_keys: bool,

    collector_running: bool,
    status: Option<db::CollectorStatus>,
    autostart: bool,
    error: Option<String>,
    toast: Option<(String, Instant)>,
    refreshing: Option<Instant>,

    updater: Updater,
    update_installing: bool,
    show_update_notes: bool,

    show_all_groups: bool,
    capture: Option<Capture>,
    /// Scroll position forced by --capture.
    scroll_override: Option<f32>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        theme::install(&cc.egui_ctx);
        let conn = db::open(&paths::db_path())?;
        db::set_setting(&conn, db::KEY_MONITORING, "1")?;
        db::ensure_servers(&conn, &sshconf::aliases())?;
        if let Err(e) = collector::spawn_detached() {
            collector::log(&format!("could not start collector: {e}"));
        }
        autostart::refresh_path();
        update::cleanup_after_update();
        let mut app = Self {
            conn,
            page: Page::Overview,
            range: Range::H1,
            scan: None,
            scan_rx: None,
            views: Vec::new(),
            last_live: None,
            last_findings: None,
            last_range: None,
            range_dirty: true,
            history: Vec::new(),
            history_for: String::new(),
            tips: TipCache::default(),
            sort: Sort::Memory,
            hover_moment: None,
            row_heights: HashMap::new(),
            scroll_to_keys: false,
            collector_running: false,
            status: None,
            autostart: autostart::is_enabled(),
            error: None,
            toast: None,
            refreshing: None,
            updater: Updater::new(),
            update_installing: false,
            show_update_notes: false,
            show_all_groups: false,
            capture: None,
            scroll_override: None,
        };
        app.start_scan(&cc.egui_ctx);
        Ok(app)
    }

    /// Reads ~/.ssh again in the background (ssh -G and ssh-keygen take a moment).
    fn start_scan(&mut self, ctx: &egui::Context) {
        if self.scan_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(sshconf::scan());
            ctx.request_repaint();
        });
        self.scan_rx = Some(rx);
    }

    /// Refresh: read ~/.ssh again and log in to every failed server now.
    fn refresh_all(&mut self, ctx: &egui::Context) {
        self.start_scan(ctx);
        self.refreshing = Some(Instant::now());
        if let Err(e) = db::bump_retry(&self.conn) {
            self.error = Some(format!("Could not ask the monitor to retry: {e}"));
        }
        if !collector::is_running() {
            self.start_collector();
        }
        self.last_live = None;
    }

    fn start_collector(&mut self) {
        let _ = db::set_setting(&self.conn, db::KEY_MONITORING, "1");
        if let Err(e) = collector::spawn_detached() {
            self.error = Some(format!("Could not start background monitoring: {e}"));
        }
        self.last_live = None;
    }

    fn host(&self, alias: &str) -> Option<&sshconf::Host> {
        self.scan.as_ref().and_then(|s| s.hosts.iter().find(|h| h.alias == alias))
    }

    fn view(&self, alias: &str) -> Option<&View> {
        self.views.iter().find(|v| v.srv.alias == alias)
    }

    fn refresh(&mut self) {
        if let Some(rx) = &self.scan_rx {
            if let Ok(scan) = rx.try_recv() {
                let aliases: Vec<String> = scan.hosts.iter().map(|h| h.alias.clone()).collect();
                let _ = db::ensure_servers(&self.conn, &aliases);
                self.scan = Some(scan);
                self.scan_rx = None;
                self.last_live = None;
            }
        }
        let now = chrono::Utc::now().timestamp();
        if self.last_live.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
            self.last_live = Some(Instant::now());
            let findings_due = self.last_findings.is_none_or(|t| t.elapsed() >= Duration::from_secs(10));
            if findings_due {
                self.last_findings = Some(Instant::now());
            }
            let r: db::Result<()> = (|| {
                let mut servers = db::servers(&self.conn)?;
                // Keep ~/.ssh/config order once it has been read.
                let order: Vec<String> = match &self.scan {
                    Some(s) => s.hosts.iter().map(|h| h.alias.clone()).collect(),
                    None => sshconf::aliases(),
                };
                servers.retain(|s| order.contains(&s.alias));
                servers.sort_by_key(|s| order.iter().position(|a| *a == s.alias).unwrap_or(usize::MAX));
                let mut views = Vec::with_capacity(servers.len());
                for srv in servers {
                    let a = srv.alias.clone();
                    let old = self.views.iter_mut().find(|v| v.srv.alias == a).map(|v| std::mem::take(&mut v.findings));
                    let findings = match (findings_due, old) {
                        (false, Some(f)) => f,
                        _ => insights::compute(&self.conn, &srv, now - 3600, now)?,
                    };
                    views.push(View {
                        latest: db::latest(&self.conn, &a)?,
                        groups: db::live(&self.conn, &a)?,
                        mounts: db::mounts(&self.conn, &a)?,
                        findings,
                        srv,
                    });
                }
                self.views = views;
                self.status = db::collector_status(&self.conn)?;
                Ok(())
            })();
            self.collector_running = collector::is_running();
            match r {
                Ok(()) => {
                    if self.error.as_deref().is_some_and(|e| e.starts_with("Could not read")) {
                        self.error = None;
                    }
                }
                Err(e) => self.error = Some(format!("Could not read data: {e}")),
            }
            if let Some(started) = self.refreshing {
                let busy = self.views.iter().any(|v| v.srv.status == "connecting");
                if (started.elapsed() > Duration::from_secs(3) && !busy && self.scan_rx.is_none()) || started.elapsed() > Duration::from_secs(45) {
                    self.refreshing = None;
                    let up = self.views.iter().filter(|v| v.srv.status == "up").count();
                    self.toast = Some((format!("{up} of {} servers online", self.views.len()), Instant::now()));
                }
            }
        }
        if let Page::Server(alias) = &self.page {
            let alias = alias.clone();
            if self.history_for != alias {
                self.history_for = alias.clone();
                self.range_dirty = true;
                self.tips.map.clear();
            }
            if self.range_dirty || self.last_range.is_none_or(|t| t.elapsed() >= Duration::from_secs(15)) {
                self.range_dirty = false;
                self.last_range = Some(Instant::now());
                match db::series(&self.conn, &alias, now - self.range.secs(), now, self.range.bucket()) {
                    Ok(h) => self.history = h,
                    Err(e) => self.error = Some(format!("Could not read data: {e}")),
                }
                self.tips.map.clear();
            }
        }
    }
}

impl App {
    /// Drives --capture: pick a page, let it settle, save a screenshot, repeat.
    fn capture_tick(&mut self, ctx: &egui::Context) {
        let Some(cap) = self.capture.as_mut() else { return };
        if let Ok(img) = cap.rx.try_recv() {
            if cap.waiting {
                let step = &cap.steps[cap.idx];
                let path = cap.dir.join(format!("{}.png", step.name));
                let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                match crate::capture::write_png(&path, img.size[0], img.size[1], &rgba) {
                    Ok(()) => println!("saved {}", path.display()),
                    Err(e) => eprintln!("could not save {}: {e}", path.display()),
                }
                cap.waiting = false;
                cap.settle_until = None;
                cap.idx += 1;
            }
        }
        ctx.request_repaint_after(Duration::from_millis(100));
        if cap.waiting {
            return;
        }
        // Wait for ~/.ssh and the first live data before the first shot.
        let ready = self.scan.is_some() && !self.views.is_empty() && cap.started.elapsed() > Duration::from_secs(5);
        if !ready {
            return;
        }
        let Some(step) = cap.steps.get(cap.idx) else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        };
        match cap.settle_until {
            None => {
                self.page = step.page.clone();
                self.scroll_override = Some(step.scroll);
                ctx.set_theme(if step.dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light });
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(step.size.unwrap_or(egui::vec2(1240.0, 820.0))));
                self.range_dirty = true;
                cap.settle_until = Some(Instant::now() + Duration::from_millis(1500));
            }
            Some(t) if Instant::now() >= t => {
                cap.waiting = true;
                let (tx, c) = (cap.tx.clone(), ctx.clone());
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                    let _ = tx.send(img);
                    c.request_repaint();
                })));
            }
            _ => {}
        }
    }

    fn scroll_area(&self, id: &str) -> egui::ScrollArea {
        let mut s = egui::ScrollArea::vertical().id_salt(id).auto_shrink(false);
        if let Some(off) = self.scroll_override {
            s = s.vertical_scroll_offset(off);
        }
        s
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.capture_tick(ui.ctx());
        self.refresh();
        let busy = self.views.iter().any(|v| v.srv.status == "connecting") || self.scan_rx.is_some();
        ui.ctx().request_repaint_after(if busy { Duration::from_millis(60) } else { Duration::from_secs(2) });
        let t = theme::tokens(ui.ctx());
        self.updater.tick(ui.ctx());
        if matches!(self.updater.state(), update::State::Restarting) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::Panel::left("nav")
            .exact_size(244.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(t.surface).stroke(Stroke::new(1.0, t.line)))
            .show(ui, |ui| self.sidebar(ui, &t));

        egui::CentralPanel::default().frame(egui::Frame::new().fill(t.canvas)).show(ui, |ui| match self.page.clone() {
            Page::Overview => self.page_overview(ui, &t),
            Page::Server(a) => self.page_server(ui, &t, &a),
            Page::Keys => self.page_keys(ui, &t),
        });

        if let Some((msg, at)) = &self.toast {
            if at.elapsed() > Duration::from_secs(4) {
                self.toast = None;
            } else {
                let msg = msg.clone();
                egui::Area::new(egui::Id::unique("toast")).anchor(egui::Align2::CENTER_BOTTOM, vec2(122.0, -24.0)).show(ui.ctx(), |ui| {
                    egui::Frame::new()
                        .fill(t.ink)
                        .corner_radius(CornerRadius::same(theme::RADIUS))
                        .inner_margin(egui::Margin::symmetric(14, 9))
                        .show(ui, |ui| ui.label(RichText::new(msg).color(t.canvas)));
                });
            }
        }
        if self.show_update_notes {
            self.update_notes_dialog(ui, &t);
        }
    }
}

// ---------- small widgets, after the JAgents CSS ----------

fn dot(ui: &mut Ui, t: &Tokens, tone: Tone) {
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
    let mut c = tone_color(t, tone);
    match tone {
        Tone::None => c = c.gamma_multiply(0.5),
        Tone::Busy => {
            let phase = (ui.input(|i| i.time) * std::f64::consts::PI).sin().abs() as f32;
            c = c.gamma_multiply(0.35 + 0.65 * phase);
        }
        _ => {}
    }
    ui.painter().circle_filled(rect.center(), 4.0, c);
}

/// A circular arrow, like JAgents' "restart" icon.
fn paint_restart(ui: &Ui, center: Pos2, r: f32, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    let start = -0.35f32;
    let end = start + std::f32::consts::TAU * 0.8;
    let pts: Vec<Pos2> = (0..=24)
        .map(|i| {
            let a = start + (end - start) * i as f32 / 24.0;
            center + vec2(a.cos(), a.sin()) * r
        })
        .collect();
    let tip = pts[0];
    ui.painter().add(egui::Shape::line(pts, stroke));
    let a = start;
    let tangent = vec2(a.sin(), -a.cos());
    let radial = vec2(a.cos(), a.sin());
    ui.painter().line_segment([tip, tip + (tangent * -1.0 + radial * 0.0) * 0.0 + (-tangent + radial) * 3.2], stroke);
    ui.painter().line_segment([tip, tip + (-tangent - radial) * 3.2], stroke);
}

/// `.btn.primary`: accent fill, light text, restart icon.
fn primary_button(ui: &mut Ui, t: &Tokens, label: &str, height: f32, full: bool, enabled: bool) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(label.to_string(), theme::font(13.0), t.accent_ink);
    let w = if full { ui.available_width() } else { galley.size().x + 44.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, height), if enabled { Sense::click() } else { Sense::hover() });
    let fill = if !enabled { t.accent.gamma_multiply(0.6) } else if resp.is_pointer_button_down_on() { t.accent.gamma_multiply(0.85) } else if resp.hovered() { t.accent.gamma_multiply(0.92) } else { t.accent };
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    let total = 15.0 + 6.0 + galley.size().x;
    let x0 = rect.center().x - total / 2.0;
    paint_restart(ui, Pos2::new(x0 + 7.0, rect.center().y), 5.2, t.accent_ink);
    ui.painter().galley(Pos2::new(x0 + 21.0, rect.center().y - galley.size().y / 2.0), galley, t.accent_ink);
    if enabled {
        resp.clone().on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    resp
}

/// `.btn` (secondary): surface fill, hairline border.
fn button(ui: &mut Ui, t: &Tokens, label: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(label.to_string(), theme::font(13.0), t.ink);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 22.0, 30.0), Sense::click());
    let fill = if resp.hovered() { t.well } else { t.surface };
    ui.painter().rect(rect, CornerRadius::same(theme::RADIUS), fill, Stroke::new(1.0, t.line), egui::StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, t.ink);
    resp
}

/// `.icon-btn` with the restart icon.
fn icon_button(ui: &mut Ui, t: &Tokens, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), if enabled { Sense::click() } else { Sense::hover() });
    let hovered = enabled && resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, CornerRadius::same(7), t.well);
    }
    let color = if !enabled { t.muted.gamma_multiply(0.5) } else if hovered { t.ink } else { t.muted };
    paint_restart(ui, rect.center(), 5.0, color);
    resp
}

/// `.chip`: a small pill with a hairline border.
fn chip(ui: &mut Ui, t: &Tokens, text: &str, color: Color32) {
    let border = if color == t.muted { t.line } else { color.gamma_multiply(0.45) };
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::font(11.0), color);
    let (rect, _) = ui.allocate_exact_size(vec2(galley.size().x + 14.0, galley.size().y + 4.0), Sense::hover());
    ui.painter().rect_stroke(rect, CornerRadius::same(255), Stroke::new(1.0, border), egui::StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
}

/// `.nav-label`: 11px, semibold, uppercase, letter spaced.
fn nav_label(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(RichText::new(text.to_uppercase()).font(theme::bold_font(11.0)).extra_letter_spacing(0.6).color(t.muted));
    });
    ui.add_space(2.0);
}

/// `.ssh-meter`: label and number on top, a 6px bar under them.
fn meter(ui: &mut Ui, t: &Tokens, label: &str, pct: f64, text: &str) {
    let p = pct.clamp(0.0, 100.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.0).color(t.muted));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(text).size(12.0).color(t.ink));
        });
    });
    ui.add_space(-3.0);
    bar(ui, t, p, theme::level(t, p), 6.0);
}

fn bar(ui: &mut Ui, t: &Tokens, pct: f64, color: Color32, h: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let r = CornerRadius::same(255);
    ui.painter().rect_filled(rect, r, t.well);
    let w = rect.width() * (pct.clamp(0.0, 100.0) / 100.0) as f32;
    if w > 0.5 {
        ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(w.max(h), h)), r, color);
    }
}

/// The sticky header at the top of a page (`.ssh-head`).
fn page_header(ui: &mut Ui, t: &Tokens, add: impl FnOnce(&mut Ui)) {
    let frame = egui::Frame::new().fill(t.canvas).inner_margin(egui::Margin { left: 18, right: 18, top: 10, bottom: 10 });
    let resp = frame.show(ui, |ui| {
        let w = ui.available_width();
        ui.allocate_ui_with_layout(vec2(w, 38.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_size(vec2(w, 38.0));
            add(ui)
        });
    });
    let r = resp.response.rect;
    ui.painter().line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, t.line));
}

fn h2(ui: &mut Ui, t: &Tokens, title: &str, count: Option<String>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(theme::bold_font(13.0)).color(t.ink));
        if let Some(c) = count {
            ui.label(RichText::new(c).size(13.0).color(t.muted));
        }
    });
    ui.add_space(4.0);
}

fn empty_note(ui: &mut Ui, t: &Tokens, text: &str) {
    let resp = egui::Frame::new().inner_margin(egui::Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(text).size(13.0).color(t.muted));
    });
    // Dashed border like `.ssh-empty`.
    let r = resp.response.rect.shrink(0.5);
    let stroke = Stroke::new(1.0, t.line);
    let corners = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    for w in corners.windows(2) {
        ui.painter().add(egui::Shape::dashed_line(&[w[0], w[1]], stroke, 5.0, 4.0));
    }
}

fn gb(b: f64) -> String {
    let g = b / 1073741824.0;
    if g >= 100.0 { format!("{g:.0} GB") } else { format!("{g:.1} GB") }
}

fn uptime(secs: i64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    if d >= 1 { format!("{d} d {h} h") } else { format!("{h} h {} min", (secs % 3600) / 60) }
}

/// How a key or a server finding reads in a list: colored dot plus text.
fn finding_row(ui: &mut Ui, t: &Tokens, level: Level, title: &str, detail: &str) {
    ui.horizontal_top(|ui| {
        ui.add_space(2.0);
        ui.vertical(|ui| {
            ui.add_space(5.0);
            let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 3.5, level_color(t, level));
        });
        ui.vertical(|ui| {
            ui.label(RichText::new(title).font(theme::bold_font(13.0)).color(t.ink));
            if !detail.is_empty() {
                ui.label(RichText::new(detail).size(12.5).color(t.muted));
            }
        });
    });
}

// ---------- sidebar ----------

impl App {
    fn sidebar(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.spacing_mut().item_spacing.y = 2.0;
        egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 12, top: 14, bottom: 0 }).show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::hover());
                ui.painter().rect_filled(rect, CornerRadius::same(6), t.accent);
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "JS", theme::bold_font(12.0), t.accent_ink);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new("JServer").font(theme::bold_font(14.0)).color(t.ink));
                    ui.label(RichText::new("Server performance over SSH").size(11.5).color(t.muted));
                });
            });
        });
        egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 12, top: 12, bottom: 4 }).show(ui, |ui| {
            let busy = self.refreshing.is_some();
            let resp = primary_button(ui, t, if busy { "Checking…" } else { "Refresh" }, 34.0, true, !busy);
            if resp.on_hover_text("Read ~/.ssh again and log in to every server now").clicked() {
                self.refresh_all(ui.ctx());
            }
        });

        let pad = egui::Margin { left: 6, right: 6, top: 0, bottom: 0 };
        egui::Frame::new().inner_margin(pad).show(ui, |ui| {
            ui.add_space(6.0);
            let on = self.page == Page::Overview;
            if side_item(ui, t, on, |ui| {
                ui.label(RichText::new("Overview").size(13.0).color(t.ink));
            })
            .clicked()
            {
                self.page = Page::Overview;
            }
        });

        nav_label(ui, t, "Servers");
        let mut go: Option<Page> = None;
        let bottom_reserve = 150.0;
        let list_h = (ui.available_height() - bottom_reserve - 110.0).max(80.0);
        egui::ScrollArea::vertical().id_salt("side_servers").max_height(list_h).auto_shrink([false, true]).show(ui, |ui| {
            egui::Frame::new().inner_margin(pad).show(ui, |ui| {
                if self.views.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(RichText::new(if self.scan.is_none() { "Reading ~/.ssh…" } else { "No hosts in ~/.ssh/config." }).size(12.5).color(t.muted));
                    });
                }
                for v in &self.views {
                    let on = self.page == Page::Server(v.srv.alias.clone());
                    let fresh = v.fresh();
                    let when = match (v.srv.status.as_str(), fresh) {
                        ("up", Some(p)) => fmt::pct(p.cpu),
                        ("connecting", _) => "…".into(),
                        _ => v.label().to_string(),
                    };
                    let resp = side_item(ui, t, on, |ui| {
                        dot(ui, t, tone_of(&v.srv.status));
                        ui.add_space(1.0);
                        ui.label(RichText::new(&v.srv.alias).size(13.0).color(t.ink));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(when).size(11.0).color(t.muted));
                        });
                    });
                    let tip = format!("{} · {}", v.srv.alias, v.label());
                    if resp.on_hover_text(tip).clicked() {
                        go = Some(Page::Server(v.srv.alias.clone()));
                    }
                }
            });
        });
        if let Some(p) = go {
            self.page = p;
        }

        nav_label(ui, t, "Keys");
        egui::Frame::new().inner_margin(pad).show(ui, |ui| {
            let on = self.page == Page::Keys;
            let summary = match &self.scan {
                Some(s) => {
                    let in_use = s.keys.iter().filter(|k| !k.used_by.is_empty()).count();
                    let problems = s.keys.iter().flat_map(|k| &k.findings).chain(&s.findings).filter(|f| f.level != sshconf::Level::Info).count();
                    (
                        format!("{} keys · {} in use", s.keys.len(), in_use),
                        format!("ssh-agent: {}", if s.agent.running { format!("{} loaded", s.agent.keys) } else { "not running".into() }),
                        problems,
                    )
                }
                None => ("Reading ~/.ssh…".into(), String::new(), 0),
            };
            if side_item(ui, t, on, |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&summary.0).size(12.5).color(t.ink));
                        if summary.2 > 0 {
                            chip(ui, t, &format!("{} to fix", summary.2), t.warn);
                        }
                    });
                    if !summary.1.is_empty() {
                        ui.label(RichText::new(&summary.1).size(11.5).color(t.muted));
                    }
                });
            })
            .clicked()
            {
                self.page = Page::Keys;
            }
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 12, top: 0, bottom: 12 }).show(ui, |ui| {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    self.update_status(ui, t);
                    ui.add_space(8.0);
                    let mut auto = self.autostart;
                    if ui
                        .checkbox(&mut auto, RichText::new("Start with Windows").size(12.5))
                        .on_hover_text("Keep watching your servers after you restart your PC.")
                        .changed()
                    {
                        match autostart::set_enabled(auto) {
                            Ok(()) => self.autostart = auto,
                            Err(e) => self.error = Some(format!("Could not change the startup option: {e}")),
                        }
                    }
                    ui.add_space(4.0);
                    let watching = self.views.iter().filter(|v| v.srv.enabled).count();
                    let stalled = self.status.as_ref().is_some_and(|s| chrono::Utc::now().timestamp() - s.heartbeat > 30);
                    if self.collector_running && stalled {
                        ui.label(RichText::new("The monitor has not answered for 30 s").size(11.5).color(t.warn));
                    } else if self.collector_running {
                        if let Some(s) = &self.status {
                            ui.label(RichText::new(format!("Running since {}", fmt::ago(s.started))).size(11.5).color(t.muted));
                        }
                    } else if ui.small_button("Start monitoring").clicked() {
                        self.start_collector();
                    }
                    ui.horizontal(|ui| {
                        dot(ui, t, if self.collector_running { Tone::Ok } else { Tone::Warn });
                        let text = if self.collector_running { format!("Watching {watching} servers in background") } else { "Background monitor stopped".into() };
                        ui.label(RichText::new(text).size(12.0).color(t.ink));
                    });
                    let r = ui.min_rect();
                    ui.painter().line_segment([Pos2::new(r.left() - 16.0, r.top() - 10.0), Pos2::new(r.right() + 12.0, r.top() - 10.0)], Stroke::new(1.0, t.line));
                });
            });
        });
    }
}

/// `.thread-item`: a full-width row, `--well` on hover, `--accent-soft` when active.
fn side_item(ui: &mut Ui, t: &Tokens, active: bool, add: impl FnOnce(&mut Ui)) -> egui::Response {
    let resp = ui
        .scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
            let hovered = ui.response().hovered();
            let fill = if active { t.accent_soft } else if hovered { t.well } else { Color32::TRANSPARENT };
            egui::Frame::new()
                .fill(fill)
                .corner_radius(CornerRadius::same(theme::RADIUS))
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(18.0);
                    ui.horizontal(add);
                });
        })
        .response;
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ---------- overview ----------

impl App {
    fn header_summary(&self) -> String {
        let n = self.views.len();
        if n == 0 {
            return String::new();
        }
        let up = self.views.iter().filter(|v| v.srv.status == "up").count();
        let unchecked = self.views.iter().filter(|v| v.srv.status.is_empty()).count();
        let mut s = format!("{up} of {n} servers online");
        if unchecked > 0 {
            s.push_str(&format!(" · {unchecked} not checked"));
        }
        s
    }

    fn page_overview(&mut self, ui: &mut Ui, t: &Tokens) {
        let summary = self.header_summary();
        let mut refresh = false;
        let busy = self.refreshing.is_some();
        page_header(ui, t, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("Servers").font(theme::bold_font(15.0)).color(t.ink));
                ui.label(RichText::new(summary).size(12.5).color(t.muted));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if primary_button(ui, t, if busy { "Checking…" } else { "Refresh" }, 30.0, false, !busy).on_hover_text("Read ~/.ssh again and log in to every server now").clicked() {
                    refresh = true;
                }
            });
        });
        if refresh {
            self.refresh_all(ui.ctx());
        }
        self.scroll_area("main_overview").show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 18, top: 16, bottom: 24 }).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(1100.0));
                ui.spacing_mut().item_spacing.y = 6.0;
                if let Some(e) = &self.error {
                    ui.label(RichText::new(e).color(t.danger));
                }
                h2(ui, t, "Servers", Some(self.views.len().to_string()));
                if self.views.is_empty() {
                    let dir = paths::ssh_dir();
                    empty_note(ui, t, &format!("No hosts in {}\\config. Add a Host entry and press Refresh.", dir.display()));
                } else {
                    self.server_grid(ui, t);
                }

                // Problems across all servers, worst first.
                let mut attention: Vec<(String, Insight)> = self
                    .views
                    .iter()
                    .flat_map(|v| v.findings.iter().filter(|f| f.level != Level::Info).map(move |f| (v.srv.alias.clone(), f.clone())))
                    .collect();
                attention.sort_by(|a, b| b.1.level.cmp(&a.1.level));
                ui.add_space(16.0);
                h2(ui, t, "Needs attention", Some(attention.len().to_string()));
                if attention.is_empty() {
                    empty_note(ui, t, "Nothing right now. Memory, disks, CPU and logins look fine on every server JServer can reach.");
                } else {
                    theme::card(t).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for (i, (alias, f)) in attention.iter().enumerate() {
                            if i > 0 {
                                ui.add_space(4.0);
                                ui.separator();
                                ui.add_space(2.0);
                            }
                            let resp = ui
                                .scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
                                    finding_row(ui, t, f.level, &format!("{alias}: {}", f.title), &f.detail);
                                })
                                .response
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if resp.clicked() {
                                self.page = Page::Server(alias.clone());
                            }
                        }
                    });
                }

                ui.add_space(16.0);
                let keys_resp = ui.scope(|ui| self.keys_section(ui, t, false)).response;
                if self.scroll_to_keys {
                    self.scroll_to_keys = false;
                    keys_resp.scroll_to_me(Some(egui::Align::TOP));
                }
            });
        });
    }

    fn server_grid(&mut self, ui: &mut Ui, t: &Tokens) {
        let gap = 10.0;
        let w = ui.available_width();
        let cols = (((w + gap) / (270.0 + gap)).floor() as usize).max(1);
        let cw = (w - gap * (cols - 1) as f32) / cols as f32;
        let mut open: Option<String> = None;
        let mut retry: Option<String> = None;
        let rows: Vec<Vec<usize>> = (0..self.views.len()).collect::<Vec<_>>().chunks(cols).map(|c| c.to_vec()).collect();
        let mut heights = HashMap::new();
        for (ri, row) in rows.iter().enumerate() {
            let min_h = self.row_heights.get(&ri).copied().unwrap_or(0.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for &i in row {
                    let r = ui.allocate_ui_with_layout(vec2(cw, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(cw);
                        let v = &self.views[i];
                        let host = self.host(&v.srv.alias).cloned();
                        server_card(ui, t, v, host.as_ref(), min_h)
                    });
                    let (h, clicked, retry_clicked) = r.inner;
                    let e: &mut f32 = heights.entry(ri).or_insert(0.0);
                    *e = e.max(h);
                    if clicked {
                        open = Some(self.views[i].srv.alias.clone());
                    }
                    if retry_clicked {
                        retry = Some(self.views[i].srv.alias.clone());
                    }
                }
            });
            ui.add_space(gap - 6.0);
        }
        if heights != self.row_heights {
            self.row_heights = heights;
            ui.ctx().request_repaint();
        }
        if let Some(a) = open {
            self.page = Page::Server(a);
        }
        if let Some(a) = retry {
            self.retry_server(&a);
        }
    }

    fn retry_server(&mut self, alias: &str) {
        let _ = db::set_enabled(&self.conn, alias, true);
        let _ = db::bump_retry(&self.conn);
        if !collector::is_running() {
            self.start_collector();
        }
        self.toast = Some((format!("Checking {alias}"), Instant::now()));
        self.last_live = None;
    }
}

/// One `.ssh-card`. Returns (content height, card clicked, check clicked).
fn server_card(ui: &mut Ui, t: &Tokens, v: &View, host: Option<&sshconf::Host>, min_h: f32) -> (f32, bool, bool) {
    let mut retry = false;
    let resp = ui.scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
        let hovered = ui.response().hovered();
        let frame = theme::card(t).stroke(Stroke::new(1.0, if hovered { t.muted.gamma_multiply(0.6) } else { t.line }));
        let inner = frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            let top = ui.min_rect().top();
            ui.spacing_mut().item_spacing.y = 6.0;
            let tone = tone_of(&v.srv.status);
            ui.horizontal(|ui| {
                dot(ui, t, tone);
                ui.label(RichText::new(&v.srv.alias).font(theme::bold_font(13.5)).color(t.ink));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, t, v.srv.status != "connecting").on_hover_text(format!("Check {}", v.srv.alias)).clicked() {
                        retry = true;
                    }
                    let c = if tone == Tone::None { t.muted } else { tone_color(t, tone) };
                    ui.label(RichText::new(v.label()).size(12.0).color(c));
                });
            });
            if let Some(h) = host {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(h.address()).font(theme::mono(12.0)).color(t.muted));
                    if !h.proxy_jump.is_empty() {
                        chip(ui, t, &format!("via {}", h.proxy_jump), t.muted);
                    }
                });
            }
            if let Some(p) = v.fresh() {
                let cores = v.srv.cores.max(1);
                meter(ui, t, "CPU", p.cpu, &format!("{} · load {:.2} / {} cores", fmt::pct(p.cpu), p.load1, cores));
                meter(ui, t, "Memory", p.mem_pct(), &format!("{} of {}", gb(p.mem_used()), gb(p.mem_total)));
                if let Some(root) = v.mounts.iter().find(|m| m.mount == "/").or(v.mounts.first()) {
                    meter(ui, t, &format!("Disk {}", root.mount), root.pct(), &format!("{} free of {}", gb(root.avail), gb(root.size)));
                }
                if p.swap_total > 0.0 && p.swap_used > 64.0 * 1048576.0 {
                    meter(ui, t, "Swap", p.swap_pct(), &format!("{} of {}", gb(p.swap_used), gb(p.swap_total)));
                }
                // What is using it, in one line.
                if let Some(g) = v.groups.iter().filter(|g| g.kind != Kind::Kernel).max_by(|a, b| a.mem.total_cmp(&b.mem)) {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.label(RichText::new("Most memory").size(12.0).color(t.muted));
                        ui.label(RichText::new(&g.label).size(12.0).color(t.ink));
                        ui.label(RichText::new(fmt::bytes(g.mem)).size(12.0).color(t.muted));
                    });
                }
                if let Some(g) = v.groups.iter().max_by(|a, b| a.cpu.total_cmp(&b.cpu)).filter(|g| g.cpu >= 0.5) {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.label(RichText::new("Most CPU").size(12.0).color(t.muted));
                        ui.label(RichText::new(&g.label).size(12.0).color(t.ink));
                        ui.label(RichText::new(fmt::pct(g.cpu)).size(12.0).color(t.muted));
                    });
                }
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    ui.label(RichText::new(format!("Up {}", uptime(p.uptime))).size(12.0).color(t.muted));
                    if !v.srv.os.is_empty() {
                        ui.label(RichText::new(&v.srv.os).size(12.0).color(t.muted));
                    }
                    if !v.srv.hostname.is_empty() {
                        ui.label(RichText::new(&v.srv.hostname).font(theme::mono(12.0)).color(t.muted));
                    }
                });
            } else if v.srv.status == "paused" {
                ui.label(RichText::new("Monitoring is paused for this server.").size(12.0).color(t.muted));
            } else if !v.srv.detail.is_empty() && v.srv.status != "connecting" {
                ui.label(RichText::new(&v.srv.detail).size(12.0).color(t.muted));
                let hint = insights::connection_hint(&v.srv.status, &v.srv.alias);
                if !hint.is_empty() {
                    ui.label(RichText::new(hint).size(12.0).color(t.muted));
                }
            } else if v.srv.status == "up" {
                ui.label(RichText::new("Logged in. The first sample is on its way.").size(12.0).color(t.muted));
            }
            let warns = v.findings.iter().filter(|f| f.level != Level::Info).count();
            // Natural height of the card (body + footer + frame margins); rows line up to the tallest.
            const FOOTER: f32 = 22.0;
            let natural = ui.min_rect().bottom() - top + 2.0 + FOOTER + 24.0;
            ui.add_space(2.0 + (min_h - natural).max(0.0));
            ui.horizontal(|ui| {
                let left = match v.srv.status.as_str() {
                    "up" if v.srv.last_sample > 0 => format!("Updated {}", fmt::ago(v.srv.last_sample)),
                    "connecting" | "" => String::new(),
                    _ if v.srv.checked_at > 0 => format!("Checked {}", fmt::ago(v.srv.checked_at)),
                    _ => String::new(),
                };
                ui.label(RichText::new(left).size(11.5).color(t.muted));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if v.srv.status == "up" && v.srv.login_ms > 0 {
                        ui.label(RichText::new(format!("Login {} ms", v.srv.login_ms)).size(11.5).color(t.muted));
                    }
                    if warns > 0 {
                        let worst = v.findings.iter().map(|f| f.level).max().unwrap_or(Level::Info);
                        chip(ui, t, &format!("{warns} to check"), level_color(t, worst));
                    }
                });
            });
            natural
        });
        inner.inner
    });
    let clicked = resp.response.clicked() && !retry;
    resp.response.on_hover_cursor(egui::CursorIcon::PointingHand);
    (resp.inner, clicked, retry)
}

// ---------- keys ----------

fn key_age(created: i64) -> String {
    let d = (chrono::Utc::now().timestamp() - created) / 86_400;
    if d < 1 {
        "today".into()
    } else if d < 60 {
        format!("{d} days old")
    } else {
        format!("{} months old", (d as f64 / 30.0).round())
    }
}

fn ssh_level(l: sshconf::Level) -> Level {
    match l {
        sshconf::Level::Info => Level::Info,
        sshconf::Level::Warn => Level::Warn,
        sshconf::Level::Danger => Level::Danger,
    }
}

impl App {
    fn keys_section(&mut self, ui: &mut Ui, t: &Tokens, detailed: bool) {
        let Some(scan) = self.scan.clone() else {
            h2(ui, t, "Keys", None);
            empty_note(ui, t, "Reading ~/.ssh…");
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Keys").font(theme::bold_font(13.0)).color(t.ink));
            ui.label(RichText::new(scan.keys.len().to_string()).size(13.0).color(t.muted));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let text = if scan.agent.running { format!("ssh-agent: {} loaded", scan.agent.keys) } else { "ssh-agent: not running".into() };
                ui.label(RichText::new(text).size(12.5).color(t.muted));
            });
        });
        ui.add_space(4.0);
        if scan.keys.is_empty() {
            empty_note(ui, t, &format!("No public keys (*.pub) in {}.", scan.ssh_dir.display()));
            return;
        }
        let mut keys = scan.keys.clone();
        keys.sort_by(|a, b| b.used_by.len().cmp(&a.used_by.len()).then(a.name.cmp(&b.name)));
        let mut go: Option<String> = None;
        egui::Frame::new()
            .fill(t.surface)
            .stroke(Stroke::new(1.0, t.line))
            .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let w = ui.available_width();
                let narrow = w < 640.0;
                let cols: Vec<(&str, f32)> = if narrow {
                    vec![("Key", 0.38), ("Type", 0.17), ("Used by", 0.3), ("Age", 0.15)]
                } else {
                    vec![("Key", 0.26), ("Type", 0.12), ("Fingerprint", 0.22), ("Used by", 0.28), ("Age", 0.12)]
                };
                let row_ui = |ui: &mut Ui, cells: Vec<Box<dyn FnOnce(&mut Ui) + '_>>| {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for ((_, frac), cell) in cols.iter().zip(cells) {
                            ui.allocate_ui_with_layout(vec2(w * frac, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                                egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                                    ui.set_width(w * frac - 24.0);
                                    cell(ui);
                                });
                            });
                        }
                    });
                };
                let line = |ui: &mut Ui| {
                    let r = ui.available_rect_before_wrap();
                    ui.painter().line_segment([Pos2::new(r.left(), r.top()), Pos2::new(r.right(), r.top())], Stroke::new(1.0, t.line));
                };
                ui.spacing_mut().item_spacing.y = 0.0;
                row_ui(
                    ui,
                    cols.iter()
                        .map(|(h, _)| {
                            let h = h.to_uppercase();
                            Box::new(move |ui: &mut Ui| {
                                ui.label(RichText::new(h).font(theme::bold_font(11.0)).extra_letter_spacing(0.5).color(t.muted));
                            }) as Box<dyn FnOnce(&mut Ui)>
                        })
                        .collect(),
                );
                for k in &keys {
                    line(ui);
                    let mut cells: Vec<Box<dyn FnOnce(&mut Ui) + '_>> = Vec::new();
                    cells.push(Box::new(|ui: &mut Ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(RichText::new(&k.name).font(theme::mono(12.5)).color(t.ink));
                        if !k.comment.is_empty() {
                            ui.label(RichText::new(&k.comment).size(12.0).color(t.muted));
                        }
                    }));
                    cells.push(Box::new(|ui: &mut Ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 5.0;
                            ui.label(RichText::new(&k.kind).size(12.5).color(t.ink));
                            if k.bits > 0 {
                                ui.label(RichText::new(k.bits.to_string()).size(12.5).color(t.muted));
                            }
                        });
                    }));
                    if !narrow {
                        cells.push(Box::new(|ui: &mut Ui| {
                            let fp = if k.fingerprint.is_empty() { &k.error } else { &k.fingerprint };
                            ui.add(egui::Label::new(RichText::new(fp).font(theme::mono(12.0)).color(t.muted)).truncate()).on_hover_text(fp);
                        }));
                    }
                    let go_ref = &mut go;
                    cells.push(Box::new(move |ui: &mut Ui| {
                        if k.used_by.is_empty() {
                            ui.label(RichText::new("Not in config").size(12.5).color(t.muted));
                        } else {
                            // Wrap chips by hand so a long list never widens the column.
                            let max_w = ui.available_width();
                            let widths: Vec<f32> = k.used_by.iter().map(|a| ui.painter().layout_no_wrap(a.clone(), theme::font(11.0), t.muted).size().x + 14.0).collect();
                            let mut lines: Vec<Vec<usize>> = vec![Vec::new()];
                            let mut x = 0.0;
                            for (n, w) in widths.iter().enumerate() {
                                if x > 0.0 && x + w > max_w {
                                    lines.push(Vec::new());
                                    x = 0.0;
                                }
                                lines.last_mut().unwrap().push(n);
                                x += w + 4.0;
                            }
                            ui.spacing_mut().item_spacing.y = 4.0;
                            for line in lines {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    for n in line {
                                        let a = &k.used_by[n];
                                        let r = ui
                                            .scope_builder(UiBuilder::new().sense(Sense::click()), |ui| chip(ui, t, a, t.muted))
                                            .response
                                            .on_hover_text(format!("Open {a}"))
                                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                                        if r.clicked() {
                                            *go_ref = Some(a.clone());
                                        }
                                    }
                                });
                            }
                        }
                    }));
                    cells.push(Box::new(|ui: &mut Ui| {
                        if k.has_private {
                            ui.label(RichText::new(key_age(k.created)).size(12.5).color(t.muted));
                        } else {
                            ui.label(RichText::new("No private key").size(12.0).color(t.warn));
                        }
                    }));
                    row_ui(ui, cells);
                    let shown: Vec<&sshconf::Finding> = k.findings.iter().filter(|f| detailed || f.level != sshconf::Level::Info).collect();
                    if !shown.is_empty() {
                        egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 12, top: 0, bottom: 8 }).show(ui, |ui| {
                            for f in shown {
                                ui.horizontal(|ui| {
                                    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                                    ui.painter().circle_filled(rect.center(), 3.0, level_color(t, ssh_level(f.level)));
                                    ui.label(RichText::new(&f.text).size(12.0).color(if f.level == sshconf::Level::Info { t.muted } else { level_color(t, ssh_level(f.level)) }));
                                });
                            }
                        });
                    }
                }
            });
        if let Some(a) = go {
            self.page = Page::Server(a);
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Only public key files are read. Logins never prompt and never accept a new or changed host key.").size(12.5).color(t.muted));
    }

    fn page_keys(&mut self, ui: &mut Ui, t: &Tokens) {
        let summary = match &self.scan {
            Some(s) => format!("{} keys in {} · {} used by a host", s.keys.len(), s.ssh_dir.display(), s.keys.iter().filter(|k| !k.used_by.is_empty()).count()),
            None => "Reading ~/.ssh…".into(),
        };
        let mut refresh = false;
        let busy = self.scan_rx.is_some();
        page_header(ui, t, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("SSH keys").font(theme::bold_font(15.0)).color(t.ink));
                ui.label(RichText::new(summary).size(12.5).color(t.muted));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if primary_button(ui, t, if busy { "Reading…" } else { "Check keys" }, 30.0, false, !busy).clicked() {
                    refresh = true;
                }
            });
        });
        if refresh {
            self.start_scan(ui.ctx());
        }
        self.scroll_area("main_keys").show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 18, top: 16, bottom: 24 }).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(1100.0));
                if let Some(scan) = self.scan.clone() {
                    let mut problems: Vec<(Level, String, String)> = scan.findings.iter().map(|f| (ssh_level(f.level), f.text.clone(), String::new())).collect();
                    if !scan.agent.running {
                        let detail = if scan.agent.note.is_empty() {
                            "Keys without a passphrase still work. Keys with one will ask every time unless the agent holds them.".to_string()
                        } else {
                            format!("{} Keys without a passphrase still work; keys with one would need it each login, which the background monitor cannot type.", scan.agent.note)
                        };
                        problems.push((Level::Info, "ssh-agent is not running".into(), detail));
                    }
                    for v in &self.views {
                        if matches!(v.srv.status.as_str(), "denied" | "hostkey") {
                            problems.push((Level::Danger, format!("{}: {}", v.srv.alias, v.srv.label), insights::connection_hint(&v.srv.status, &v.srv.alias).into()));
                        }
                    }
                    problems.sort_by(|a, b| b.0.cmp(&a.0));
                    h2(ui, t, "Checks", Some(problems.len().to_string()));
                    if problems.is_empty() {
                        empty_note(ui, t, "Every host points at a key that exists, and every server accepted its key.");
                    } else {
                        theme::card(t).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for (i, (l, title, detail)) in problems.iter().enumerate() {
                                if i > 0 {
                                    ui.separator();
                                }
                                finding_row(ui, t, *l, title, detail);
                            }
                        });
                    }
                    ui.add_space(16.0);
                    h2(ui, t, "Hosts", Some(scan.hosts.len().to_string()));
                    theme::card(t).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::Grid::new("hosts_grid").num_columns(4).spacing(vec2(24.0, 8.0)).show(ui, |ui| {
                            for h in ["Host", "Address", "Key", "Login"] {
                                ui.label(RichText::new(h.to_uppercase()).font(theme::bold_font(11.0)).extra_letter_spacing(0.5).color(t.muted));
                            }
                            ui.end_row();
                            for h in &scan.hosts {
                                ui.label(RichText::new(&h.alias).font(theme::bold_font(13.0)).color(t.ink));
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(h.address()).font(theme::mono(12.0)).color(t.muted));
                                    if !h.proxy_jump.is_empty() {
                                        chip(ui, t, &format!("via {}", h.proxy_jump), t.muted);
                                    }
                                });
                                let keys: Vec<String> = h
                                    .identity_files
                                    .iter()
                                    .map(|f| f.rsplit(['/', '\\']).next().unwrap_or(f).to_string())
                                    .collect();
                                ui.label(RichText::new(keys.join(", ")).font(theme::mono(12.0)).color(t.muted));
                                match self.view(&h.alias) {
                                    Some(v) => {
                                        let tone = tone_of(&v.srv.status);
                                        ui.horizontal(|ui| {
                                            dot(ui, t, tone);
                                            ui.label(RichText::new(v.label()).size(12.5).color(if tone == Tone::None { t.muted } else { tone_color(t, tone) }));
                                        });
                                    }
                                    None => {
                                        ui.label("");
                                    }
                                }
                                ui.end_row();
                            }
                        });
                    });
                    ui.add_space(16.0);
                }
                self.keys_section(ui, t, true);
            });
        });
    }
}

// ---------- one server ----------

/// Big-number tile used on the server page.
fn tile(ui: &mut Ui, t: &Tokens, label: &str, value: &str, sub: &str, pct: Option<f64>) {
    theme::card(t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(label).size(12.0).color(t.muted));
        ui.label(RichText::new(value).font(theme::bold_font(21.0)).color(t.ink));
        ui.add(egui::Label::new(RichText::new(if sub.is_empty() { " " } else { sub }).size(11.5).color(t.muted)).truncate()).on_hover_text(sub);
        ui.add_space(4.0);
        match pct {
            Some(p) => bar(ui, t, p, theme::level(t, p), 4.0),
            None => {
                ui.add_space(4.0);
            }
        }
    });
}

impl App {
    fn page_server(&mut self, ui: &mut Ui, t: &Tokens, alias: &str) {
        let Some(vi) = self.views.iter().position(|v| v.srv.alias == alias) else {
            page_header(ui, t, |ui| {
                ui.label(RichText::new(alias).font(theme::bold_font(15.0)).color(t.ink));
            });
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.label(RichText::new("This host is no longer in ~/.ssh/config.").color(t.muted));
            });
            return;
        };
        let host = self.host(alias).cloned();
        let (status, label, enabled) = {
            let v = &self.views[vi];
            (v.srv.status.clone(), v.label().to_string(), v.srv.enabled)
        };
        let mut toggle = false;
        let mut retry = false;
        page_header(ui, t, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.horizontal(|ui| {
                    dot(ui, t, tone_of(&status));
                    ui.label(RichText::new(alias).font(theme::bold_font(15.0)).color(t.ink));
                    let tone = tone_of(&status);
                    ui.label(RichText::new(&label).size(12.5).color(if tone == Tone::None { t.muted } else { tone_color(t, tone) }));
                });
                if let Some(h) = &host {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(h.address()).font(theme::mono(12.0)).color(t.muted));
                        if !h.proxy_jump.is_empty() {
                            chip(ui, t, &format!("via {}", h.proxy_jump), t.muted);
                        }
                    });
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(ui, t, if enabled { "Pause monitoring" } else { "Resume monitoring" })
                    .on_hover_text(if enabled { "Log out of this server and stop recording it." } else { "Log in again and keep recording." })
                    .clicked()
                {
                    toggle = true;
                }
                if enabled && icon_button(ui, t, status != "connecting").on_hover_text("Log in again now").clicked() {
                    retry = true;
                }
            });
        });
        if toggle {
            let _ = db::set_enabled(&self.conn, alias, !enabled);
            if enabled {
                let _ = db::set_status(&self.conn, alias, "paused", "Paused", "", chrono::Utc::now().timestamp());
            } else {
                let _ = db::bump_retry(&self.conn);
            }
            self.last_live = None;
        }
        if retry {
            self.retry_server(alias);
        }

        let mut clicked_ts = None;
        let mut toggle_all = false;
        self.scroll_area("main_server").show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 18, top: 16, bottom: 24 }).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(1100.0));
                ui.spacing_mut().item_spacing.y = 6.0;
                let v = &self.views[vi];
                if let Some(e) = &self.error {
                    ui.label(RichText::new(e).color(t.danger));
                }
                let fresh = v.fresh().cloned();
                if fresh.is_none() {
                    let (title, detail) = match v.srv.status.as_str() {
                        "paused" => ("Monitoring is paused".to_string(), "Press Resume monitoring to log in and record again.".to_string()),
                        "connecting" | "" => ("Logging in…".to_string(), "This takes a few seconds, longer through a jump host.".to_string()),
                        "up" => ("Waiting for the first sample".to_string(), "Rates need two samples, about 10 seconds.".to_string()),
                        _ => (
                            v.label().to_string(),
                            [v.srv.detail.as_str(), insights::connection_hint(&v.srv.status, alias)].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("\n"),
                        ),
                    };
                    theme::card(t).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        finding_row(ui, t, if matches!(tone_of(&v.srv.status), Tone::Danger | Tone::Warn) { Level::Danger } else { Level::Info }, &title, &detail);
                        if let Some(p) = &v.latest {
                            ui.add_space(4.0);
                            ui.label(RichText::new(format!("Last sample {}. The history below is from before.", fmt::ago(p.ts))).size(12.0).color(t.muted));
                        }
                    });
                    ui.add_space(12.0);
                }

                if let Some(p) = &fresh {
                    let cores = v.srv.cores.max(1) as f64;
                    let tiles: [(&str, String, String, Option<f64>); 6] = [
                        ("CPU", fmt::pct(p.cpu), format!("waiting on disk {} · taken by host {}", fmt::pct(p.iowait), fmt::pct(p.steal)), Some(p.cpu)),
                        ("Memory", fmt::pct(p.mem_pct()), format!("{} of {}", gb(p.mem_used()), gb(p.mem_total)), Some(p.mem_pct())),
                        (
                            "Swap",
                            if p.swap_total > 0.0 { fmt::pct(p.swap_pct()) } else { "None".into() },
                            if p.swap_total > 0.0 { format!("{} of {}", gb(p.swap_used), gb(p.swap_total)) } else { "No swap set up".into() },
                            (p.swap_total > 0.0).then(|| p.swap_pct()),
                        ),
                        ("Load", format!("{:.2}", p.load1), format!("{:.2} · {:.2} · {} cores", p.load5, p.load15, cores), Some((p.load1 / cores * 100.0).min(100.0))),
                        ("Network", fmt::rate(p.net_rx + p.net_tx), format!("in {} · out {}", fmt::rate(p.net_rx), fmt::rate(p.net_tx)), None),
                        ("Disk I/O", fmt::rate(p.disk_read + p.disk_write), format!("read {} · write {} · busy {}", fmt::rate(p.disk_read), fmt::rate(p.disk_write), fmt::pct(p.disk_busy)), None),
                    ];
                    let cols = 3;
                    for chunk in tiles.chunks(cols) {
                        ui.columns(cols, |c| {
                            for (col, (l, val, sub, pct)) in c.iter_mut().zip(chunk) {
                                tile(col, t, l, val, sub, *pct);
                            }
                        });
                    }
                    ui.add_space(12.0);
                    memory_card(ui, t, v, p);
                    ui.add_space(12.0);
                }

                // What is using it.
                if !v.groups.is_empty() && fresh.is_some() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("What is using this server").font(theme::bold_font(13.0)).color(t.ink));
                        ui.label(RichText::new(format!("{} groups · {} processes", v.groups.len(), v.groups.iter().map(|g| g.procs).sum::<i64>())).size(13.0).color(t.muted));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.selectable_value(&mut self.sort, Sort::Cpu, "CPU");
                            ui.selectable_value(&mut self.sort, Sort::Memory, "Memory");
                            ui.label(RichText::new("Sort by").size(12.5).color(t.muted));
                        });
                    });
                    ui.add_space(4.0);
                    let total_mem = fresh.as_ref().map(|p| p.mem_total).unwrap_or(1.0).max(1.0);
                    if groups_table(ui, t, &v.groups, self.sort, total_mem, self.show_all_groups) {
                        toggle_all = true;
                    }
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Processes are grouped by the VM, container or systemd service that started them. Memory is resident memory (RSS); processes that share memory can add up to more than they really use.")
                            .size(12.0)
                            .color(t.muted),
                    );
                    ui.add_space(12.0);
                }

                if !v.mounts.is_empty() {
                    h2(ui, t, "Disks", Some(v.mounts.len().to_string()));
                    theme::card(t).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let cols = if ui.available_width() > 700.0 { 2 } else { 1 };
                        for chunk in v.mounts.chunks(cols) {
                            ui.columns(cols, |c| {
                                for (col, m) in c.iter_mut().zip(chunk) {
                                    meter(col, t, &m.mount, m.pct(), &format!("{} free of {} · {}%", gb(m.avail), gb(m.size), m.pct()));
                                    col.label(RichText::new(&m.fs).font(theme::mono(11.0)).color(t.muted));
                                }
                            });
                            ui.add_space(4.0);
                        }
                    });
                    ui.add_space(12.0);
                }

                if !v.findings.is_empty() {
                    h2(ui, t, "Findings", Some("last hour".into()));
                    theme::card(t).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for (i, f) in v.findings.iter().enumerate() {
                            if i > 0 {
                                ui.add_space(2.0);
                                ui.separator();
                            }
                            finding_row(ui, t, f.level, &f.title, &f.detail);
                        }
                    });
                    ui.add_space(12.0);
                }
            });

            // History charts.
            egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 18, top: 0, bottom: 24 }).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(1100.0));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("History").font(theme::bold_font(13.0)).color(t.ink));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        for r in Range::ALL.iter().rev() {
                            if ui.selectable_label(self.range == *r, r.label()).clicked() && self.range != *r {
                                self.range = *r;
                                self.range_dirty = true;
                            }
                        }
                    });
                });
                ui.label(RichText::new("Hover a chart to see what was running at that moment.").size(12.0).color(t.muted));
                ui.add_space(4.0);
                let now = chrono::Utc::now().timestamp();
                if self.history.is_empty() {
                    empty_note(ui, t, &format!("No samples in the last {} yet. Charts fill in once JServer can log in.", self.range.label()));
                    ui.add_space(12.0);
                    about_card(ui, t, &self.views[vi], host.as_ref());
                    return;
                }
                let h = &self.history;
                let pick = |f: fn(&db::Point) -> f64| -> Vec<(i64, f64)> { h.iter().map(|p| (p.ts, f(p))).collect() };
                let cpu = pick(|p| p.cpu);
                let mem = pick(|p| p.mem_pct());
                let load = pick(|p| p.load1);
                let rx = pick(|p| p.net_rx);
                let tx = pick(|p| p.net_tx);
                let rd = pick(|p| p.disk_read);
                let wr = pick(|p| p.disk_write);
                let bucket = self.range.bucket();
                let x_range = Some((now - self.range.secs(), now));
                let gap = (bucket * 3).max(30);
                let marker = self.hover_moment;
                let mut hovered = None;
                let alias_s = alias.to_string();
                                ui.columns(2, |c| {
                    let spec = |id, title, value_fmt: fn(f64) -> String, y_max, tip| ChartSpec { id, title, value_fmt, y_max, height: 140.0, gap, x_range, marker, tip, bucket };
                    let (a, ha) = time_chart(&mut c[0], t, spec("cpu", "CPU", pct_fmt, Some(100.0), Tip::Cpu), &[("CPU", &cpu, t.accent)], &mut self.tips, &self.conn, &alias_s);
                    let (b, hb) = time_chart(&mut c[1], t, spec("mem", "Memory", pct_fmt, Some(100.0), Tip::Mem), &[("Memory", &mem, t.accent)], &mut self.tips, &self.conn, &alias_s);
                    c[0].add_space(8.0);
                    c[1].add_space(8.0);
                    let (cl, hl) = time_chart(&mut c[0], t, spec("load", "Load", load_fmt, None, Tip::Cpu), &[("Load", &load, t.accent)], &mut self.tips, &self.conn, &alias_s);
                    let (d, hd) = time_chart(&mut c[1], t, spec("net", "Network", rate_fmt, None, Tip::None), &[("In", &rx, t.accent), ("Out", &tx, t.ok)], &mut self.tips, &self.conn, &alias_s);
                    c[0].add_space(8.0);
                    let (e, he) = time_chart(&mut c[0], t, spec("disk", "Disk I/O", rate_fmt, None, Tip::None), &[("Read", &rd, t.accent), ("Write", &wr, t.warn)], &mut self.tips, &self.conn, &alias_s);
                    clicked_ts = a.or(b).or(cl).or(d).or(e);
                    hovered = ha.or(hb).or(hl).or(hd).or(he);
                });
                self.hover_moment = hovered;
                ui.add_space(12.0);
                about_card(ui, t, &self.views[vi], host.as_ref());
            });
        });
        let _ = clicked_ts;
        if toggle_all {
            self.show_all_groups = !self.show_all_groups;
        }
    }
}

fn memory_card(ui: &mut Ui, t: &Tokens, v: &View, p: &db::Point) {
    let total = p.mem_total.max(1.0);
    let arc = p.arc.min(p.mem_used());
    let programs = (p.mem_used() - arc).max(0.0);
    let cache = (p.mem_avail - p.mem_free).max(0.0);
    let free = p.mem_free.max(0.0);
    let segs: Vec<(&str, f64, Color32)> = [
        ("Programs", programs, t.accent),
        ("ZFS cache", arc, t.warn),
        ("File cache, freed when needed", cache, t.accent.gamma_multiply(0.35)),
        ("Free", free, t.well),
    ]
    .into_iter()
    .filter(|s| s.1 > 0.0)
    .collect();
    theme::card(t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new("Memory").font(theme::bold_font(13.0)).color(t.ink));
            ui.label(RichText::new(format!("{} total", gb(total))).size(13.0).color(t.muted));
        });
        ui.add_space(4.0);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 12.0), Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(255), t.well);
        let mut x = rect.left();
        for (i, (_, b, c)) in segs.iter().enumerate() {
            let w = (rect.width() * (*b / total) as f32).max(0.0);
            let r = Rect::from_min_size(Pos2::new(x, rect.top()), vec2(w, rect.height()));
            let cr = CornerRadius { nw: if i == 0 { 6 } else { 0 }, sw: if i == 0 { 6 } else { 0 }, ne: if i == segs.len() - 1 { 6 } else { 0 }, se: if i == segs.len() - 1 { 6 } else { 0 } };
            ui.painter().rect_filled(r, cr, *c);
            x += w;
        }
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            for (l, b, c) in &segs {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    let (r, _) = ui.allocate_exact_size(vec2(9.0, 9.0), Sense::hover());
                    ui.painter().rect(r, CornerRadius::same(2), *c, Stroke::new(1.0, t.line), egui::StrokeKind::Inside);
                    ui.label(RichText::new(*l).size(12.0).color(t.muted));
                    ui.label(RichText::new(gb(*b)).size(12.0).color(t.ink));
                });
            }
        });
        // One sentence on what takes it.
        let mut top: Vec<&db::LiveGroup> = v.groups.iter().filter(|g| g.kind != Kind::Kernel).collect();
        top.sort_by(|a, b| b.mem.total_cmp(&a.mem));
        if let Some(g) = top.first() {
            let mut s = format!(
                "Programs use {}. The biggest is {} with {} ({:.0}% of all memory)",
                gb(programs),
                g.label,
                fmt::bytes(g.mem),
                g.mem / total * 100.0
            );
            if let Some(g2) = top.get(1) {
                s.push_str(&format!(", then {} with {}", g2.label, fmt::bytes(g2.mem)));
            }
            s.push('.');
            if arc > total * 0.05 {
                s.push_str(&format!(" ZFS keeps {} of disk data cached in memory.", gb(arc)));
            }
            ui.add_space(2.0);
            ui.label(RichText::new(s).size(12.5).color(t.ink));
        }
    });
}

/// Returns true when "Show all" / "Show fewer" was pressed.
fn groups_table(ui: &mut Ui, t: &Tokens, groups: &[db::LiveGroup], sort: Sort, total_mem: f64, show_all: bool) -> bool {
    let limit = if show_all { 200 } else { 10 };
    let mut toggle = false;
    let mut rows: Vec<&db::LiveGroup> = groups.iter().collect();
    match sort {
        Sort::Memory => rows.sort_by(|a, b| b.mem.total_cmp(&a.mem)),
        Sort::Cpu => rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem.total_cmp(&a.mem))),
    }
    let max_mem = rows.iter().map(|g| g.mem).fold(1.0, f64::max);
    let max_cpu = rows.iter().map(|g| g.cpu).fold(0.1, f64::max);
    egui::Frame::new()
        .fill(t.surface)
        .stroke(Stroke::new(1.0, t.line))
        .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let w = ui.available_width();
            let (c_name, c_num) = (w - 3.0 * 120.0, 120.0);
            let header = |ui: &mut Ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for (h, cw) in [("Name", c_name), ("CPU", c_num), ("Memory", c_num), ("Processes", c_num)] {
                        ui.allocate_ui_with_layout(vec2(cw, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                            egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                                ui.set_width(cw - 24.0);
                                ui.label(RichText::new(h.to_uppercase()).font(theme::bold_font(11.0)).extra_letter_spacing(0.5).color(t.muted));
                            });
                        });
                    }
                });
            };
            header(ui);
            for g in rows.iter().take(limit) {
                let r = ui.available_rect_before_wrap();
                ui.painter().line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, t.line));
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.allocate_ui_with_layout(vec2(c_name, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                            ui.set_width(c_name - 24.0);
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                ui.label(RichText::new(&g.label).font(theme::bold_font(13.0)).color(t.ink));
                                chip(ui, t, g.kind.chip(), if matches!(g.kind, Kind::Vm | Kind::Docker | Kind::Lxc) { t.accent } else { t.muted });
                                if !g.users.is_empty() && g.kind != Kind::Kernel {
                                    ui.label(RichText::new(&g.users).size(11.5).color(t.muted));
                                }
                            });
                            ui.label(RichText::new(&g.explain).size(12.5).color(t.muted));
                            if !g.cmdline.is_empty() {
                                ui.add(egui::Label::new(RichText::new(&g.cmdline).font(theme::mono(11.0)).color(t.muted)).truncate()).on_hover_text(&g.cmdline);
                            }
                        });
                    });
                    let num = |ui: &mut Ui, text: String, frac: Option<f64>| {
                        ui.allocate_ui_with_layout(vec2(c_num, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                            egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                                ui.set_width(c_num - 24.0);
                                ui.label(RichText::new(text).size(13.0).color(t.ink));
                                if let Some(f) = frac {
                                    ui.add_space(3.0);
                                    bar(ui, t, f * 100.0, t.accent, 4.0);
                                }
                            });
                        });
                    };
                    num(ui, fmt::pct(g.cpu), Some(g.cpu / max_cpu));
                    num(ui, format!("{}  {:.0}%", fmt::bytes(g.mem), g.mem / total_mem * 100.0), Some(g.mem / max_mem));
                    num(ui, g.procs.to_string(), None);
                });
            }
            if rows.len() > 10 {
                let r = ui.available_rect_before_wrap();
                ui.painter().line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, t.line));
                egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
                    let text = if show_all { "Show the top 10".to_string() } else { format!("Show all {} ({} smaller not shown)", rows.len(), rows.len() - 10) };
                    if ui.link(RichText::new(text).size(12.5)).clicked() {
                        toggle = true;
                    }
                });
            }
        });
    toggle
}

fn about_card(ui: &mut Ui, t: &Tokens, v: &View, host: Option<&sshconf::Host>) {
    h2(ui, t, "About this server", None);
    theme::card(t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::Grid::new("about").num_columns(2).spacing(vec2(24.0, 6.0)).show(ui, |ui| {
            let mut row = |k: &str, val: String| {
                if !val.is_empty() {
                    ui.label(RichText::new(k).size(12.5).color(t.muted));
                    ui.label(RichText::new(val).size(12.5).color(t.ink));
                    ui.end_row();
                }
            };
            row("Host name", v.srv.hostname.clone());
            row("System", v.srv.os.clone());
            row("Kernel", v.srv.kernel.clone());
            row("Processor", if v.srv.cpu_model.is_empty() { String::new() } else { format!("{} ({} cores)", v.srv.cpu_model, v.srv.cores) });
            if let Some(p) = &v.latest {
                row("Up for", uptime(p.uptime));
            }
            row("Logged in as", v.srv.login_user.clone());
            if let Some(h) = host {
                row("Address", h.address());
                if !h.proxy_jump.is_empty() {
                    row("Through", h.proxy_jump.clone());
                }
            }
            if v.srv.login_ms > 0 {
                row("Login took", format!("{} ms", v.srv.login_ms));
            }
            if v.srv.connected_at > 0 && v.srv.status == "up" {
                row("Connected", fmt::ago(v.srv.connected_at));
            }
        });
    });
}

// ---------- charts ----------

/// What a chart's hover tooltip breaks down by group.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tip {
    None,
    Cpu,
    Mem,
}

struct ChartSpec<'a> {
    id: &'a str,
    title: &'a str,
    value_fmt: fn(f64) -> String,
    y_max: Option<f64>,
    height: f32,
    gap: i64,
    x_range: Option<(i64, i64)>,
    marker: Option<i64>,
    tip: Tip,
    bucket: i64,
}

#[derive(Default)]
struct TipCache {
    map: HashMap<(i64, i64), (Vec<db::GroupSummary>, i64)>,
}

impl TipCache {
    fn get(&mut self, conn: &Connection, alias: &str, from: i64, to: i64) -> &(Vec<db::GroupSummary>, i64) {
        if self.map.len() > 400 {
            self.map.clear();
        }
        self.map.entry((from, to)).or_insert_with(|| {
            let g = db::group_summaries(conn, alias, from, to).unwrap_or_default();
            let m = db::minutes_recorded(conn, alias, from, to).unwrap_or(0);
            (g, m)
        })
    }
}

/// Splits points into segments wherever there is a gap (the server was not reachable).
fn segments(points: &[(i64, f64)], max_gap: i64) -> Vec<Vec<[f64; 2]>> {
    let mut out: Vec<Vec<[f64; 2]>> = Vec::new();
    let mut prev: Option<i64> = None;
    for &(ts, v) in points {
        if prev.is_none_or(|p| ts - p > max_gap) {
            out.push(Vec::new());
        }
        out.last_mut().unwrap().push([ts as f64, v]);
        prev = Some(ts);
    }
    out
}

fn nearest(points: &[(i64, f64)], x: f64, max_dist: i64) -> Option<(i64, f64)> {
    let i = points.partition_point(|(ts, _)| (*ts as f64) < x);
    [i.checked_sub(1), Some(i)]
        .into_iter()
        .flatten()
        .filter_map(|i| points.get(i).copied())
        .min_by(|a, b| (a.0 as f64 - x).abs().total_cmp(&(b.0 as f64 - x).abs()))
        .filter(|(ts, _)| (*ts as f64 - x).abs() <= max_dist as f64)
}

/// A time chart in a card. Returns (clicked time, hovered time).
fn time_chart(
    ui: &mut Ui,
    t: &Tokens,
    spec: ChartSpec,
    series: &[(&str, &Vec<(i64, f64)>, Color32)],
    tips: &mut TipCache,
    conn: &Connection,
    alias: &str,
) -> (Option<i64>, Option<i64>) {
    theme::card(t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(spec.title).font(theme::bold_font(13.0)).color(t.ink));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for (name, pts, color) in series.iter().rev() {
                    if let Some((_, v)) = pts.last() {
                        ui.label(RichText::new((spec.value_fmt)(*v)).size(12.0).color(t.ink));
                        if series.len() > 1 {
                            ui.label(RichText::new(*name).size(12.0).color(*color));
                        }
                    }
                }
            });
        });
        let main = series[0].1;
        if main.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("No samples in this range yet.").size(12.5).color(t.muted));
            ui.add_space(8.0);
            return (None, None);
        }
        let value_fmt = spec.value_fmt;
        let mut plot = Plot::new((spec.id, alias))
            .height(spec.height)
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .show_x(false)
            .show_y(false)
            .show_grid([false, true])
            .show_axes([true, true])
            .include_y(0.0)
            .set_margin_fraction(vec2(0.0, 0.08))
            .x_axis_formatter(|m, r| fmt::axis(m.value as i64, r.end() - r.start()))
            .y_axis_formatter(move |m, _| value_fmt(m.value))
            .label_formatter(|_| None);
        if let Some(max) = spec.y_max {
            plot = plot.include_y(max);
        }
        if let Some((a, b)) = spec.x_range {
            plot = plot.include_x(a as f64).include_x(b as f64);
        }
        let resp = plot.show(ui, |p| {
            for (name, pts, color) in series {
                for seg in segments(pts, spec.gap) {
                    let mut line = Line::new(*name, PlotPoints::from(seg)).color(*color).width(1.8);
                    if series.len() == 1 {
                        line = line.fill(0.0).fill_alpha(0.10);
                    }
                    p.line(line);
                }
            }
            if let Some(m) = spec.marker {
                p.vline(VLine::new("marker", m as f64).color(t.muted.gamma_multiply(0.5)).width(1.0));
            }
            let hover = if p.response().hovered() { p.pointer_coordinate().and_then(|c| nearest(main, c.x, spec.gap)) } else { None };
            if let Some((ts, v)) = hover {
                p.vline(VLine::new("hover", ts as f64).color(t.muted).width(1.0));
                p.points(Points::new("hover point", vec![[ts as f64, v]]).radius(4.0).color(series[0].2).filled(true));
            }
            let clicked = if p.response().clicked() { p.pointer_coordinate().map(|c| c.x as i64) } else { None };
            (clicked, hover)
        });
        let (clicked, hover) = resp.inner;
        if let Some((ts, _)) = hover {
            let values: Vec<(String, String, Color32)> = series
                .iter()
                .filter_map(|(n, pts, c)| nearest(pts, ts as f64, spec.gap).map(|(_, v)| (n.to_string(), (spec.value_fmt)(v), *c)))
                .collect();
            resp.response.on_hover_ui_at_pointer(|ui| tip_ui(ui, t, &spec, ts, &values, tips, conn, alias));
        }
        (clicked, hover.map(|h| h.0))
    })
    .inner
}

#[allow(clippy::too_many_arguments)]
fn tip_ui(ui: &mut Ui, t: &Tokens, spec: &ChartSpec, ts: i64, values: &[(String, String, Color32)], tips: &mut TipCache, conn: &Connection, alias: &str) {
    ui.set_min_width(250.0);
    let bucket = spec.bucket.max(1);
    let from = ts - ts.rem_euclid(bucket);
    let when = if bucket < 60 { fmt::day_time(ts) } else { format!("{} to {}", fmt::day_time(from), fmt::local(from + bucket).format("%H:%M")) };
    ui.label(RichText::new(when).size(12.0).color(t.muted));
    for (n, v, c) in values {
        ui.horizontal(|ui| {
            ui.label(RichText::new(n).size(12.5).color(*c));
            ui.label(RichText::new(v).font(theme::bold_font(12.5)).color(t.ink));
        });
    }
    if spec.tip == Tip::None {
        return;
    }
    let a_from = from - from.rem_euclid(60);
    let a_to = (from + bucket).max(a_from + 60) - 1;
    let (groups, minutes) = tips.get(conn, alias, a_from, a_to);
    if *minutes == 0 || groups.is_empty() {
        ui.label(RichText::new("Details are saved once a minute and are not ready for this moment yet.").size(12.0).color(t.muted));
        return;
    }
    let minutes = *minutes as f64;
    let mut rows: Vec<(f64, String, &db::GroupSummary)> = groups
        .iter()
        .map(|g| match spec.tip {
            Tip::Mem => (g.mem_avg, fmt::bytes(g.mem_avg), g),
            _ => {
                let v = g.cpu_load / minutes;
                (v, fmt::pct(v), g)
            }
        })
        .filter(|(v, _, _)| *v > 0.0)
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    ui.add_space(4.0);
    ui.label(RichText::new(if spec.tip == Tip::Mem { "Using the most memory" } else { "Using the most CPU" }).size(12.0).color(t.muted));
    egui::Grid::new(("tip", spec.id)).num_columns(2).spacing(vec2(16.0, 2.0)).show(ui, |ui| {
        for (_, label, g) in rows.iter().take(6) {
            ui.label(RichText::new(&g.label).size(12.5).color(t.ink));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(label).font(theme::bold_font(12.5)).color(t.ink));
            });
            ui.end_row();
        }
    });
}

fn pct_fmt(v: f64) -> String {
    format!("{v:.0}%")
}
fn rate_fmt(v: f64) -> String {
    fmt::rate(v)
}
fn load_fmt(v: f64) -> String {
    format!("{v:.2}")
}

// ---------- updates ----------

impl App {
    fn update_notes_dialog(&mut self, ui: &mut Ui, t: &Tokens) {
        let update::State::Available(rel) = self.updater.state() else {
            self.show_update_notes = false;
            return;
        };
        let resp = egui::Modal::new(egui::Id::unique("update_notes")).show(ui.ctx(), |ui| {
            ui.set_width(460.0);
            ui.label(RichText::new(format!("What's new in {}", rel.version)).font(theme::bold_font(15.0)));
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                let notes = if rel.notes.is_empty() { "No notes for this release." } else { rel.notes.as_str() };
                ui.label(RichText::new(notes).color(t.muted));
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Install and restart").strong()).clicked() {
                    self.update_installing = true;
                    self.updater.install(rel.clone(), ui.ctx());
                    self.show_update_notes = false;
                }
                ui.hyperlink_to("Open on GitHub", &rel.page);
                if ui.button("Close").clicked() {
                    self.show_update_notes = false;
                }
            });
        });
        if resp.should_close() {
            self.show_update_notes = false;
        }
    }

    /// Update controls at the bottom of the sidebar (laid out bottom-up).
    fn update_status(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.label(RichText::new(format!("Version {}", update::CURRENT)).size(11.5).color(t.muted));
        match self.updater.state() {
            update::State::Available(rel) => {
                ui.horizontal(|ui| {
                    if ui.small_button("What's new").clicked() {
                        self.show_update_notes = true;
                    }
                    if ui.small_button(RichText::new("Install").strong()).on_hover_text("Restarts JServer. Monitoring pauses for a few seconds.").clicked() {
                        self.update_installing = true;
                        self.updater.install(rel.clone(), ui.ctx());
                    }
                });
                ui.label(RichText::new(format!("Version {} is available", rel.version)).size(11.5).color(t.accent));
            }
            update::State::Downloading(done, total) => {
                let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                ui.add(egui::ProgressBar::new(frac).desired_height(6.0));
                ui.label(RichText::new("Downloading update").size(11.5).color(t.muted));
            }
            update::State::Restarting => {
                ui.label(RichText::new("Restarting").size(11.5).color(t.muted));
            }
            state => {
                if ui.small_button("Check for updates").clicked() {
                    self.update_installing = false;
                    self.updater.check(ui.ctx());
                }
                let note = match state {
                    update::State::Checking => Some("Checking for updates"),
                    update::State::UpToDate => Some("You have the latest version"),
                    update::State::Failed(_) if self.update_installing => Some("The update was not installed"),
                    update::State::Failed(_) => Some("Could not check for updates"),
                    _ => None,
                };
                if let Some(note) = note {
                    let color = if self.update_installing { t.danger } else { t.muted };
                    let resp = ui.label(RichText::new(note).size(11.5).color(color));
                    if let update::State::Failed(e) = state {
                        resp.on_hover_text(e);
                    }
                }
            }
        }
    }
}
