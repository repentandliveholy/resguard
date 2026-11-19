use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Align, Color32, CornerRadius, Layout, RichText, ScrollArea, Sense, Stroke, Vec2};
use resguard::config::{self, Config};
use resguard::control::Command;
use resguard::governor::EventKind;
use resguard::paths;
use resguard::runtime::{Authority, Event, Mode, ProcRow, Snapshot};

use crate::charts::{draw_time_series_chart, Series, ThresholdLine};
use crate::engine::{read_tail_lines, EngineCmd, EngineHandle, HistoryPoint};
use crate::theme::{
    apply_theme, card_frame, pct_color, pill_badge, pressure_color, zone_color, zone_label,
    ACCENT_BLUE, ACCENT_CYAN, BG_ELEVATED, BG_SURFACE, BORDER_SUBTLE, STATUS_AMBER,
    STATUS_GREEN, STATUS_ORANGE, STATUS_RED, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};

const MANUAL_PAUSE_SECS: u64 = 30;
const MAX_PROC_ROWS: usize = 300;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Processes,
    Activity,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcFilter {
    Busy,
    All,
    Changed,
    Governable,
    Protected,
}

impl ProcFilter {
    const ALL: [ProcFilter; 5] = [
        ProcFilter::Busy,
        ProcFilter::All,
        ProcFilter::Changed,
        ProcFilter::Governable,
        ProcFilter::Protected,
    ];
    fn label(self) -> &'static str {
        match self {
            ProcFilter::Busy => "Busy",
            ProcFilter::All => "All",
            ProcFilter::Changed => "Slowed / paused",
            ProcFilter::Governable => "Governable",
            ProcFilter::Protected => "Protected",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcSort {
    Cpu,
    Memory,
    Name,
}

/// Local copy of what the engine published, refreshed when its generation changes.
struct View {
    generation: u64,
    backend: String,
    cores: usize,
    snap: Option<Snapshot>,
    cfg: Config,
    history: Vec<HistoryPoint>,
    events: Vec<Event>,
    toast: Option<(String, bool, Instant)>,
    fatal: Option<String>,
}

pub struct RsDashApp {
    engine: EngineHandle,
    view: View,
    tab: Tab,
    // processes
    search: String,
    filter: ProcFilter,
    sort: ProcSort,
    // activity
    only_noteworthy: bool,
    show_log: bool,
    log_lines: Vec<String>,
    log_err: Option<String>,
    last_log_poll: Instant,
    // settings
    draft: Option<Config>,
    new_protect: String,
    settings_msg: Option<(String, bool)>,
}

impl RsDashApp {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, mode: Mode) -> Self {
        apply_theme(&cc.egui_ctx);
        let engine = EngineHandle::start(cc.egui_ctx.clone(), cfg.clone(), mode);
        Self {
            engine,
            view: View {
                generation: u64::MAX,
                backend: String::new(),
                cores: 1,
                snap: None,
                cfg,
                history: Vec::new(),
                events: Vec::new(),
                toast: None,
                fatal: None,
            },
            tab: Tab::Overview,
            search: String::new(),
            filter: ProcFilter::Busy,
            sort: ProcSort::Cpu,
            only_noteworthy: false,
            show_log: false,
            log_lines: Vec::new(),
            log_err: None,
            last_log_poll: Instant::now() - Duration::from_secs(60),
            draft: None,
            new_protect: String::new(),
            settings_msg: None,
        }
    }

    fn refresh_view(&mut self) {
        let s = self.engine.shared.lock().unwrap();
        self.view.toast = s.toast.clone();
        self.view.fatal = s.fatal.clone();
        if s.generation == self.view.generation {
            return;
        }
        self.view.generation = s.generation;
        self.view.backend = s.backend_name.clone();
        self.view.cores = s.num_cores;
        self.view.snap = s.snapshot.clone();
        self.view.cfg = s.config.clone();
        self.view.history = s.history.iter().cloned().collect();
        self.view.events = s.events.iter().cloned().collect();
    }

    fn run(&self, cmd: Command) {
        self.engine.send(EngineCmd::Run(cmd));
    }

    fn log_path(&self) -> PathBuf {
        self.view
            .cfg
            .log_file
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(paths::default_service_log)
    }
}

impl eframe::App for RsDashApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.refresh_view();

        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(BG_SURFACE)
                    .stroke(Stroke::new(1.0, BORDER_SUBTLE))
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ctx, |ui| self.top_bar(ui));

        egui::TopBottomPanel::bottom("bottom")
            .frame(
                egui::Frame::new()
                    .fill(BG_SURFACE)
                    .stroke(Stroke::new(1.0, BORDER_SUBTLE))
                    .inner_margin(egui::Margin::symmetric(16, 6)),
            )
            .show(ctx, |ui| self.bottom_bar(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(crate::theme::BG_BASE).inner_margin(egui::Margin::same(16)))
            .show(ctx, |ui| {
                if let Some(err) = &self.view.fatal {
                    ui.colored_label(STATUS_RED, err);
                    return;
                }
                let Some(snap) = self.view.snap.clone() else {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("Collecting first sample…").color(TEXT_MUTED));
                    });
                    return;
                };
                self.status_banner(ui, &snap);
                ui.add_space(12.0);
                match self.tab {
                    Tab::Overview => self.overview(ui, &snap),
                    Tab::Processes => self.processes(ui, &snap),
                    Tab::Activity => self.activity(ui),
                    Tab::Settings => self.settings(ui),
                }
            });

        // Keep toasts fading out even without new samples.
        if self.view.toast.as_ref().map_or(false, |t| t.2.elapsed() < Duration::from_secs(6)) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.engine.shutdown();
    }
}

// ------------------------------------------------------------------ chrome

impl RsDashApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("RS Dash").size(17.0).strong().color(TEXT_PRIMARY));
            ui.label(
                RichText::new(format!("resguard {}", env!("CARGO_PKG_VERSION")))
                    .size(11.5)
                    .color(TEXT_MUTED),
            );
            ui.add_space(18.0);
            for (tab, label) in [
                (Tab::Overview, "Overview"),
                (Tab::Processes, "Processes"),
                (Tab::Activity, "Activity"),
                (Tab::Settings, "Settings"),
            ] {
                if ui
                    .selectable_label(self.tab == tab, RichText::new(label).size(13.5))
                    .clicked()
                {
                    self.tab = tab;
                }
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let current = self.view.snap.as_ref().map(|s| s.mode).unwrap_or(Mode::DryRun);
                for (mode, label, tip) in [
                    (Mode::Observe, "Observe", "Telemetry only. Never changes anything."),
                    (Mode::DryRun, "Dry-run", "Shows what resguard would do, without doing it."),
                    (
                        Mode::Active,
                        "Active",
                        "Lowers the priority of heavy batch work under sustained load. \
                         If the background service is running, it stays in charge.",
                    ),
                ] {
                    if ui
                        .selectable_label(current == mode, label)
                        .on_hover_text(tip)
                        .clicked()
                        && current != mode
                    {
                        self.engine.send(EngineCmd::SetMode(mode));
                    }
                }
                ui.label(RichText::new("Mode").size(11.5).color(TEXT_MUTED));
            });
        });
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let small = |t: String| RichText::new(t).size(11.0).color(TEXT_MUTED);
            ui.label(small(format!(
                "{} · {} cores · sample every {} ms",
                self.view.backend.split(' ').next().unwrap_or(""),
                self.view.cores,
                self.view.cfg.tick_ms
            )));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some((msg, ok, at)) = &self.view.toast {
                    if at.elapsed() < Duration::from_secs(6) {
                        ui.label(
                            RichText::new(msg)
                                .size(12.0)
                                .color(if *ok { STATUS_GREEN } else { STATUS_RED }),
                        );
                    }
                }
            });
        });
    }

    /// One plain sentence: who is in charge and what has been changed.
    fn status_banner(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let changed = snap.demoted.len() + snap.paused.len();
        let (color, title, detail) = match &snap.authority {
            Authority::Governing => (
                STATUS_GREEN,
                "RS Dash is governing".to_string(),
                "Heavy batch work is slowed under sustained load. Apps are never paused.".to_string(),
            ),
            Authority::Deferred { pid, role } => (
                ACCENT_CYAN,
                format!("The resguard {} is governing", role),
                format!("PID {}. Actions you take here are sent to it.", pid),
            ),
            Authority::Simulating => (
                ACCENT_BLUE,
                "Dry-run: nothing is being changed".to_string(),
                "Switch to Active, or install the background service (resguard --install-service).".to_string(),
            ),
            Authority::Observing => (
                TEXT_MUTED,
                "Observing only".to_string(),
                "No governing, no simulation.".to_string(),
            ),
        };

        egui::Frame::new()
            .fill(BG_SURFACE)
            .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 5.0, color);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(RichText::new(title).size(14.5).strong().color(TEXT_PRIMARY));
                        ui.label(RichText::new(detail).size(12.0).color(TEXT_SECONDARY));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if changed > 0 {
                            let can_act = matches!(
                                snap.authority,
                                Authority::Governing | Authority::Deferred { .. }
                            );
                            if ui
                                .add_enabled(can_act, egui::Button::new("Undo all changes"))
                                .on_hover_text("Restore normal priority and resume everything resguard changed")
                                .clicked()
                            {
                                self.run(Command::RestoreAll);
                            }
                        }
                        let zone = snap.zone;
                        pill_badge(ui, zone_label(zone), zone_color(zone));
                        if changed > 0 {
                            pill_badge(
                                ui,
                                &format!("{} slowed · {} paused", snap.demoted.len(), snap.paused.len()),
                                STATUS_AMBER,
                            );
                        }
                    });
                });
            });
    }
}

// ------------------------------------------------------------------ overview

fn bar(ui: &mut egui::Ui, frac: f64, color: Color32) {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 6.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(r, CornerRadius::same(3), BG_ELEVATED);
    let mut fill = r;
    fill.set_width((r.width() * frac.clamp(0.0, 1.0) as f32).max(0.0));
    p.rect_filled(fill, CornerRadius::same(3), color);
}

fn tile(ui: &mut egui::Ui, title: &str, value: String, sub: String, frac: f64, color: Color32) {
    card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(RichText::new(title).size(11.5).color(TEXT_MUTED));
        ui.label(RichText::new(value).size(26.0).strong().color(color));
        ui.label(RichText::new(sub).size(11.5).color(TEXT_SECONDARY));
        ui.add_space(4.0);
        bar(ui, frac, color);
    });
}

fn gb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

impl RsDashApp {
    fn overview(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let cfg = self.view.cfg.clone();
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if let Some(m) = &snap.metrics {
                let cols = if ui.available_width() > 860.0 { 4 } else { 2 };
                let tiles: Vec<(String, String, String, f64, Color32)> = vec![
                    (
                        "CPU".into(),
                        format!("{:.0}%", m.cpu_total_pct),
                        format!("user {:.0}% · system {:.0}% · load {:.1}", m.cpu_user_pct, m.cpu_sys_pct, m.load_1m),
                        m.cpu_total_pct / 100.0,
                        pct_color(m.cpu_total_pct, cfg.cpu_soft, cfg.cpu_high),
                    ),
                    (
                        "MEMORY".into(),
                        format!("{:.0}%", m.ram_used_pct),
                        format!("{:.1} / {:.1} GB · pressure {}", m.ram_used_gb(), m.ram_total_gb(), m.memory_pressure),
                        m.ram_used_pct / 100.0,
                        if m.memory_pressure != resguard::model::MemoryPressure::Normal {
                            pressure_color(m.memory_pressure)
                        } else {
                            pct_color(m.ram_used_pct, cfg.ram_high_pct, cfg.ram_critical_pct)
                        },
                    ),
                    (
                        "SWAP".into(),
                        format!("{:.1} GB", gb(m.swap_used_bytes)),
                        format!("of {:.1} GB · warn at {:.1} GB", gb(m.swap_total_bytes), cfg.swap_warn_mb / 1024.0),
                        if m.swap_total_bytes > 0 { m.swap_used_bytes as f64 / m.swap_total_bytes as f64 } else { 0.0 },
                        if m.swap_used_mb() > cfg.swap_warn_mb { STATUS_AMBER } else { ACCENT_CYAN },
                    ),
                    (
                        "DISK FREE".into(),
                        format!("{:.1} GB", m.disk_free_gb()),
                        format!("system volume · alert below {:.0} GB", cfg.disk_min_free_gb),
                        (cfg.disk_min_free_gb * 4.0 - m.disk_free_gb()).max(0.0) / (cfg.disk_min_free_gb * 4.0),
                        if m.disk_free_gb() < cfg.disk_min_free_gb {
                            STATUS_RED
                        } else if m.disk_free_gb() < cfg.disk_min_free_gb * 2.0 {
                            STATUS_AMBER
                        } else {
                            STATUS_GREEN
                        },
                    ),
                ];
                for row in tiles.chunks(cols) {
                    ui.columns(cols, |c| {
                        for (i, (t, v, s, f, col)) in row.iter().enumerate() {
                            tile(&mut c[i], t, v.clone(), s.clone(), *f, *col);
                        }
                    });
                    ui.add_space(4.0);
                }
                ui.add_space(8.0);
                self.load_sources(ui, snap, m.cpu_total_pct);
                ui.add_space(12.0);
            }

            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                let ts: Vec<String> = self.view.history.iter().map(|h| h.ts.clone()).collect();
                let cpu: Vec<f64> = self.view.history.iter().map(|h| h.cpu).collect();
                let ram: Vec<f64> = self.view.history.iter().map(|h| h.ram).collect();
                let gov: Vec<f64> = self.view.history.iter().map(|h| h.governable).collect();
                draw_time_series_chart(
                    ui,
                    "Last two minutes",
                    "%",
                    100.0,
                    170.0,
                    &ts,
                    &[
                        Series { label: "CPU", color: STATUS_ORANGE, values: &cpu, fill_alpha: 40 },
                        Series { label: "Governable CPU", color: STATUS_AMBER, values: &gov, fill_alpha: 0 },
                        Series { label: "RAM", color: ACCENT_CYAN, values: &ram, fill_alpha: 20 },
                    ],
                    &[
                        ThresholdLine { label: "busy", value: cfg.cpu_soft, color: STATUS_AMBER },
                        ThresholdLine { label: "saturated", value: cfg.cpu_high, color: STATUS_RED },
                    ],
                );
            });
            ui.add_space(12.0);

            ui.columns(2, |c| {
                self.changes_card(&mut c[0], snap);
                self.recent_card(&mut c[1]);
            });
        });
    }

    /// Who is using the CPU, split into what resguard can and cannot help with.
    fn load_sources(&self, ui: &mut egui::Ui, snap: &Snapshot, total: f64) {
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Where the CPU load comes from").size(13.5).strong());
            ui.add_space(6.0);
            let governable = snap.governable_cpu.min(total);
            let yours_other = (snap.own_cpu - snap.governable_cpu).max(0.0).min(total - governable);
            let system = (total - governable - yours_other).max(0.0);
            let parts = [
                (governable, STATUS_AMBER, "Batch work resguard may slow down"),
                (yours_other, ACCENT_BLUE, "Your apps (never touched)"),
                (system, TEXT_MUTED, "System, kernel & other users (out of reach)"),
            ];
            let w = ui.available_width();
            let (r, _) = ui.allocate_exact_size(Vec2::new(w, 14.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(r, CornerRadius::same(4), BG_ELEVATED);
            let mut x = r.left();
            for (v, col, _) in parts {
                let pw = r.width() * (v / 100.0).clamp(0.0, 1.0) as f32;
                if pw > 0.5 {
                    let seg = egui::Rect::from_min_size(egui::pos2(x, r.top()), Vec2::new(pw, r.height()));
                    p.rect_filled(seg, CornerRadius::ZERO, col);
                    x += pw;
                }
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                for (v, col, label) in parts {
                    let (d, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                    ui.painter().rect_filled(d, CornerRadius::same(2), col);
                    ui.label(RichText::new(format!("{} {:.0}%", label, v)).size(12.0).color(TEXT_SECONDARY));
                    ui.add_space(10.0);
                }
            });
            if total >= self.view.cfg.cpu_soft && snap.governable_cpu < self.view.cfg.min_governable_cpu_pct {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Most of this load is outside resguard's reach, so it is not acting. \
                         Slowing your other apps would not help and could make them hang.",
                    )
                    .size(12.0)
                    .color(STATUS_AMBER),
                );
            }
        });
    }

    fn changes_card(&self, ui: &mut egui::Ui, snap: &Snapshot) {
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(180.0);
            ui.label(RichText::new("Changed right now").size(13.5).strong());
            ui.add_space(4.0);
            if snap.demoted.is_empty() && snap.paused.is_empty() {
                ui.label(RichText::new("Nothing. Every process runs at its normal priority.").color(TEXT_MUTED));
                return;
            }
            let can_act = matches!(snap.authority, Authority::Governing | Authority::Deferred { .. });
            ScrollArea::vertical().id_salt("changes").max_height(220.0).show(ui, |ui| {
                for (pid, secs, name) in &snap.paused {
                    ui.horizontal(|ui| {
                        pill_badge(ui, "paused", STATUS_RED);
                        ui.label(format!("{} ({})", name, pid));
                        ui.label(RichText::new(format!("resumes in {} s", secs)).color(TEXT_MUTED));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add_enabled(can_act, egui::Button::new("Resume").small()).clicked() {
                                self.run(Command::Resume { pid: *pid });
                            }
                        });
                    });
                }
                for (pid, name) in &snap.demoted {
                    ui.horizontal(|ui| {
                        pill_badge(ui, "slowed", STATUS_AMBER);
                        ui.label(format!("{} ({})", name, pid));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add_enabled(can_act, egui::Button::new("Restore").small()).clicked() {
                                self.run(Command::Background { pid: *pid, on: false });
                            }
                        });
                    });
                }
            });
        });
    }

    fn recent_card(&mut self, ui: &mut egui::Ui) {
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(180.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Recent activity").size(13.5).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("View all").clicked() {
                        self.tab = Tab::Activity;
                    }
                });
            });
            ui.add_space(4.0);
            let items: Vec<&Event> = self
                .view
                .events
                .iter()
                .filter(|e| crate::engine::event_is_noteworthy(e.kind))
                .take(8)
                .collect();
            if items.is_empty() {
                ui.label(RichText::new("No actions or alerts yet.").color(TEXT_MUTED));
            }
            for ev in items {
                event_row(ui, ev);
            }
        });
    }
}

fn event_color(kind: EventKind) -> Color32 {
    match kind {
        EventKind::Action => STATUS_AMBER,
        EventKind::Restore => STATUS_GREEN,
        EventKind::Alert | EventKind::Error => STATUS_RED,
        EventKind::DryRun => ACCENT_BLUE,
        EventKind::Manual => ACCENT_CYAN,
        EventKind::System => TEXT_MUTED,
    }
}

fn event_label(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Action => "action",
        EventKind::Restore => "restored",
        EventKind::Alert => "alert",
        EventKind::Error => "error",
        EventKind::DryRun => "dry-run",
        EventKind::Manual => "you",
        EventKind::System => "info",
    }
}

fn event_row(ui: &mut egui::Ui, ev: &Event) {
    ui.horizontal_top(|ui| {
        ui.label(RichText::new(&ev.ts).monospace().size(11.5).color(TEXT_MUTED));
        ui.add_sized(
            [64.0, 16.0],
            egui::Label::new(RichText::new(event_label(ev.kind)).size(11.0).strong().color(event_color(ev.kind))),
        );
        ui.add(egui::Label::new(RichText::new(&ev.msg).size(12.5).color(TEXT_PRIMARY)).wrap());
    });
}

// ------------------------------------------------------------------ processes

impl RsDashApp {
    fn processes(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search name, path or PID")
                    .desired_width(240.0),
            );
            ui.add_space(8.0);
            for f in ProcFilter::ALL {
                if ui.selectable_label(self.filter == f, f.label()).clicked() {
                    self.filter = f;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for (s, label) in [(ProcSort::Name, "Name"), (ProcSort::Memory, "Memory"), (ProcSort::Cpu, "CPU")] {
                    if ui.selectable_label(self.sort == s, label).clicked() {
                        self.sort = s;
                    }
                }
                ui.label(RichText::new("Sort").color(TEXT_MUTED));
            });
        });
        ui.label(
            RichText::new(
                "Slow down = lower priority (the process keeps running). Pause = stop for 30 s, then resume automatically. \
                 GUI apps, system processes, terminals and protected names cannot be changed.",
            )
            .size(11.5)
            .color(TEXT_MUTED),
        );
        ui.add_space(6.0);

        let needle = self.search.trim().to_ascii_lowercase();
        let mut rows: Vec<&ProcRow> = snap
            .procs
            .iter()
            .filter(|r| match self.filter {
                ProcFilter::Busy => r.sample.cpu_pct >= 0.5 || r.demoted || r.paused,
                ProcFilter::All => true,
                ProcFilter::Changed => r.demoted || r.paused || r.sample.is_stopped,
                ProcFilter::Governable => r.eligibility.is_governable(),
                ProcFilter::Protected => !r.eligibility.is_governable(),
            })
            .filter(|r| {
                needle.is_empty()
                    || r.sample.name.to_ascii_lowercase().contains(&needle)
                    || r.sample.exe_path.to_ascii_lowercase().contains(&needle)
                    || r.sample.pid.to_string().contains(&needle)
            })
            .collect();
        match self.sort {
            ProcSort::Cpu => rows.sort_by(|a, b| {
                b.sample.cpu_pct.partial_cmp(&a.sample.cpu_pct).unwrap_or(std::cmp::Ordering::Equal)
            }),
            ProcSort::Memory => rows.sort_by(|a, b| b.sample.rss_bytes.cmp(&a.sample.rss_bytes)),
            ProcSort::Name => rows.sort_by_key(|r| r.sample.name.to_ascii_lowercase()),
        }
        let total = rows.len();
        rows.truncate(MAX_PROC_ROWS);

        let can_act = matches!(snap.authority, Authority::Governing | Authority::Deferred { .. });
        let mut cmd: Option<Command> = None;

        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("procs")
                    .striped(true)
                    .num_columns(6)
                    .spacing([14.0, 6.0])
                    .min_col_width(40.0)
                    .show(ui, |ui| {
                        for h in ["Process", "PID", "CPU", "Memory", "Status", ""] {
                            ui.label(RichText::new(h).size(11.5).strong().color(TEXT_MUTED));
                        }
                        ui.end_row();
                        for r in &rows {
                            let s = &r.sample;
                            let name: String = s.name.chars().take(38).collect();
                            ui.label(RichText::new(name).color(TEXT_PRIMARY))
                                .on_hover_text(if s.exe_path.is_empty() { "(path unavailable)" } else { &s.exe_path });
                            ui.label(RichText::new(s.pid.to_string()).monospace().color(TEXT_SECONDARY));
                            ui.label(RichText::new(format!("{:.1}%", s.cpu_pct)).monospace().color(
                                if s.cpu_pct >= 10.0 { STATUS_AMBER } else { TEXT_SECONDARY },
                            ));
                            ui.label(RichText::new(format!("{:.0} MB", s.rss_mb())).monospace().color(TEXT_SECONDARY));
                            if r.paused {
                                pill_badge(ui, "paused", STATUS_RED);
                            } else if s.is_stopped {
                                pill_badge(ui, "stopped", STATUS_RED);
                            } else if r.demoted {
                                pill_badge(ui, "slowed", STATUS_AMBER);
                            } else if !r.eligibility.is_governable() {
                                ui.label(RichText::new(format!("protected · {}", r.eligibility.reason())).size(11.5).color(TEXT_MUTED));
                            } else {
                                ui.label(RichText::new("normal").size(11.5).color(TEXT_MUTED));
                            }
                            ui.horizontal(|ui| {
                                ui.add_enabled_ui(can_act, |ui| {
                                    if r.paused || s.is_stopped {
                                        if ui.small_button("Resume").clicked() {
                                            cmd = Some(Command::Resume { pid: s.pid });
                                        }
                                    } else if r.demoted {
                                        if ui.small_button("Restore").clicked() {
                                            cmd = Some(Command::Background { pid: s.pid, on: false });
                                        }
                                    } else if r.eligibility.is_governable() {
                                        if ui.small_button("Slow down").clicked() {
                                            cmd = Some(Command::Background { pid: s.pid, on: true });
                                        }
                                        if ui.small_button("Pause 30 s").clicked() {
                                            cmd = Some(Command::Pause { pid: s.pid, secs: MANUAL_PAUSE_SECS });
                                        }
                                    }
                                })
                                .response
                                .on_disabled_hover_text("Switch to Active mode to change processes");
                            });
                            ui.end_row();
                        }
                    });
                if total > rows.len() {
                    ui.label(
                        RichText::new(format!("Showing {} of {} — refine the search to see more", rows.len(), total))
                            .size(11.5)
                            .color(TEXT_MUTED),
                    );
                }
            });
        });
        if let Some(c) = cmd {
            self.run(c);
        }
    }
}

// ------------------------------------------------------------------ activity

impl RsDashApp {
    fn activity(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.only_noteworthy, "Only actions and alerts");
            ui.checkbox(&mut self.show_log, "Show service log file");
        });
        ui.add_space(6.0);

        if self.show_log {
            if self.last_log_poll.elapsed() >= Duration::from_secs(2) {
                self.last_log_poll = Instant::now();
                match read_tail_lines(&self.log_path(), 96 * 1024, 300) {
                    Ok(l) => {
                        self.log_lines = l;
                        self.log_err = None;
                    }
                    Err(e) => self.log_err = Some(e),
                }
            }
            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(self.log_path().display().to_string()).size(11.5).color(TEXT_MUTED));
                if let Some(e) = &self.log_err {
                    ui.colored_label(STATUS_AMBER, e);
                }
                ScrollArea::vertical()
                    .id_salt("log")
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for l in &self.log_lines {
                            ui.label(RichText::new(l).monospace().size(11.0).color(TEXT_SECONDARY));
                        }
                    });
            });
            return;
        }

        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let mut any = false;
                for ev in self
                    .view
                    .events
                    .iter()
                    .filter(|e| !self.only_noteworthy || crate::engine::event_is_noteworthy(e.kind))
                {
                    any = true;
                    event_row(ui, ev);
                }
                if !any {
                    ui.label(RichText::new("Nothing yet.").color(TEXT_MUTED));
                }
            });
        });
    }
}

// ------------------------------------------------------------------ settings

fn setting_row(ui: &mut egui::Ui, label: &str, help: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.label(RichText::new(label).color(TEXT_PRIMARY)).on_hover_text(help);
    add(ui);
    ui.label(RichText::new(help).size(11.5).color(TEXT_MUTED));
    ui.end_row();
}

impl RsDashApp {
    fn settings(&mut self, ui: &mut egui::Ui) {
        if self.draft.is_none() {
            self.draft = Some(self.view.cfg.clone());
        }
        let mut save = false;
        let mut revert = false;
        let defaults = config::default_protected_names();

        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let d = self.draft.as_mut().unwrap();

            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("CPU").size(14.0).strong());
                ui.add_space(4.0);
                egui::Grid::new("cpu").num_columns(3).spacing([16.0, 8.0]).show(ui, |ui| {
                    setting_row(ui, "Busy threshold", "Sustained CPU above this lowers the priority of heavy batch work.", |ui| {
                        ui.add(egui::Slider::new(&mut d.cpu_soft, 30.0..=99.0).suffix("%"));
                    });
                    setting_row(ui, "Saturated", "Shown as saturated in the dashboard.", |ui| {
                        ui.add(egui::Slider::new(&mut d.cpu_high, 30.0..=100.0).suffix("%"));
                    });
                    setting_row(ui, "Release below", "Priorities are restored once CPU stays below this.", |ui| {
                        ui.add(egui::Slider::new(&mut d.cpu_release, 10.0..=95.0).suffix("%"));
                    });
                    setting_row(ui, "Act only if governable ≥", "If less CPU than this comes from batch work, resguard does nothing.", |ui| {
                        ui.add(egui::Slider::new(&mut d.min_governable_cpu_pct, 0.0..=80.0).suffix("%"));
                    });
                    setting_row(ui, "Per-process minimum", "Ignore processes using less CPU than this.", |ui| {
                        ui.add(egui::Slider::new(&mut d.min_proc_cpu_pct, 1.0..=50.0).suffix("%"));
                    });
                });
            });
            ui.add_space(10.0);

            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Memory, swap & disk").size(14.0).strong());
                ui.add_space(4.0);
                egui::Grid::new("mem").num_columns(3).spacing([16.0, 8.0]).show(ui, |ui| {
                    setting_row(ui, "RAM high", "Counts toward a low-disk emergency.", |ui| {
                        ui.add(egui::Slider::new(&mut d.ram_high_pct, 50.0..=99.0).suffix("%"));
                    });
                    setting_row(ui, "RAM critical", "Raises a memory alert.", |ui| {
                        ui.add(egui::Slider::new(&mut d.ram_critical_pct, 50.0..=99.9).suffix("%"));
                    });
                    setting_row(ui, "Disk free minimum", "Low disk plus heavy swap raises an alert.", |ui| {
                        ui.add(egui::Slider::new(&mut d.disk_min_free_gb, 1.0..=100.0).suffix(" GB"));
                    });
                    setting_row(ui, "Swap warning", "Swap above this counts toward an emergency.", |ui| {
                        ui.add(egui::Slider::new(&mut d.swap_warn_mb, 256.0..=32768.0).suffix(" MB").logarithmic(true));
                    });
                    setting_row(
                        ui,
                        "Pause in an emergency",
                        "Off by default. Pauses the largest governable process (never an app) for a limited time.",
                        |ui| {
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut d.emergency_pause, "");
                                ui.add_enabled(
                                    d.emergency_pause,
                                    egui::Slider::new(&mut d.emergency_pause_secs, 5..=120).suffix(" s max"),
                                );
                            });
                        },
                    );
                });
            });
            ui.add_space(10.0);

            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("General").size(14.0).strong());
                ui.add_space(4.0);
                egui::Grid::new("gen").num_columns(3).spacing([16.0, 8.0]).show(ui, |ui| {
                    setting_row(ui, "Sample interval", "How often resguard measures.", |ui| {
                        ui.add(egui::Slider::new(&mut d.tick_ms, 250..=5000).suffix(" ms"));
                    });
                    setting_row(ui, "Notifications", "Desktop notification on memory alerts.", |ui| {
                        ui.checkbox(&mut d.notify, "");
                    });
                    setting_row(ui, "Automatic updates", "The background service updates itself daily.", |ui| {
                        ui.checkbox(&mut d.auto_update, "");
                    });
                });
            });
            ui.add_space(10.0);

            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Protected names").size(14.0).strong());
                ui.label(
                    RichText::new(
                        "Processes with these names are never touched. GUI apps, system processes and other users' \
                         processes are always protected, whether listed or not.",
                    )
                    .size(11.5)
                    .color(TEXT_MUTED),
                );
                ui.add_space(4.0);
                let extras: Vec<String> = d
                    .protect_names
                    .iter()
                    .filter(|n| !defaults.iter().any(|x| x.eq_ignore_ascii_case(n)))
                    .cloned()
                    .collect();
                ui.horizontal_wrapped(|ui| {
                    if extras.is_empty() {
                        ui.label(RichText::new("No custom names.").color(TEXT_MUTED));
                    }
                    for name in extras {
                        if ui.small_button(format!("{}  ✕", name)).on_hover_text("Remove").clicked() {
                            d.protect_names.retain(|n| n != &name);
                        }
                    }
                });
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.new_protect)
                            .hint_text("Process name, e.g. Docker")
                            .desired_width(220.0),
                    );
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Add").clicked() || enter {
                        let n = self.new_protect.trim().to_string();
                        if !n.is_empty() && !d.protect_names.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                            d.protect_names.push(n);
                        }
                        self.new_protect.clear();
                    }
                });
                egui::CollapsingHeader::new(RichText::new(format!("Built-in list ({})", defaults.len())).size(12.0))
                    .show(ui, |ui| {
                        ui.label(RichText::new(defaults.join(", ")).size(11.5).color(TEXT_MUTED));
                    });
            });
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                if ui.button(RichText::new("Save & apply").strong()).clicked() {
                    save = true;
                }
                if ui.button("Discard changes").clicked() {
                    revert = true;
                }
                if let Some((msg, ok)) = &self.settings_msg {
                    ui.label(RichText::new(msg).color(if *ok { STATUS_GREEN } else { STATUS_RED }));
                }
            });
        });

        if revert {
            self.draft = Some(self.view.cfg.clone());
            self.settings_msg = None;
        }
        if save {
            let d = self.draft.clone().unwrap();
            let res = config::validate_config(&d).and_then(|_| {
                let path = config::default_config_path().ok_or("Cannot resolve config path")?;
                config::save_config(&path, &d).map(|_| path)
            });
            self.settings_msg = Some(match res {
                Ok(path) => {
                    self.engine.send(EngineCmd::SetConfig(d));
                    (
                        format!("Saved to {} — the background service picks it up automatically", path.display()),
                        true,
                    )
                }
                Err(e) => (e, false),
            });
        }
    }
}
