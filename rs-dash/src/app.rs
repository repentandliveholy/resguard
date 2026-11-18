use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Color32, RichText, ScrollArea, Vec2};
use resguard::config::{self, Config};

use crate::charts::{draw_sparkline, draw_threshold_bar, draw_time_series_chart, Series, ThresholdLine};
use crate::engine::{
    read_tail_lines, DashSnapshot, EngineCmd, EngineHandle, EventKind, GuardMode, ProcessRow,
};
use crate::theme::{
    apply_theme, card_frame, pct_color, pill_badge, pressure_color, subcard_frame, zone_color,
    zone_label, ACCENT_BLUE, ACCENT_CYAN, ACCENT_INDIGO, ACCENT_PURPLE, BG_BASE, BG_ELEVATED,
    BG_SURFACE, BORDER_SUBTLE, STATUS_AMBER, STATUS_CRIMSON, STATUS_GREEN, STATUS_ORANGE,
    STATUS_RED, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavTab {
    Dashboard,
    Processes,
    Logs,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcFilter {
    All,
    ActiveCpu,
    Governed,
    Protected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcSort {
    CpuDesc,
    RamDesc,
    PidAsc,
    NameAsc,
}

pub struct RsDashApp {
    engine: EngineHandle,
    tab: NavTab,
    // Processes tab state
    proc_search: String,
    proc_filter: ProcFilter,
    proc_sort: ProcSort,
    // Logs tab state
    daemon_log_path: String,
    daemon_log_lines: Vec<String>,
    daemon_log_err: Option<String>,
    last_log_poll: Instant,
    show_daemon_log: bool,
    // Settings draft state
    draft_cfg: Config,
    new_protect_name: String,
    settings_status: Option<(String, Color32)>,
}

impl RsDashApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_cfg: Config, start_active: bool) -> Self {
        apply_theme(&cc.egui_ctx);
        let engine = EngineHandle::start(cc.egui_ctx.clone(), initial_cfg.clone(), start_active);
        let log_path = initial_cfg
            .log_file
            .clone()
            .unwrap_or_else(|| "/tmp/resguard.log".to_string());

        Self {
            engine,
            tab: NavTab::Dashboard,
            proc_search: String::new(),
            proc_filter: ProcFilter::All,
            proc_sort: ProcSort::CpuDesc,
            daemon_log_path: log_path,
            daemon_log_lines: Vec::new(),
            daemon_log_err: None,
            last_log_poll: Instant::now() - Duration::from_secs(10),
            show_daemon_log: true,
            draft_cfg: initial_cfg,
            new_protect_name: String::new(),
            settings_status: None,
        }
    }

    fn refresh_daemon_log_if_due(&mut self) {
        if self.last_log_poll.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_log_poll = Instant::now();
        let path = PathBuf::from(self.daemon_log_path.trim());
        if !path.exists() {
            self.daemon_log_lines.clear();
            self.daemon_log_err = Some(format!("No log file at {}", path.display()));
            return;
        }
        match read_tail_lines(&path, 64 * 1024, 150) {
            Ok(lines) => {
                self.daemon_log_lines = lines;
                self.daemon_log_err = None;
            }
            Err(e) => {
                self.daemon_log_err = Some(e);
            }
        }
    }
}

impl eframe::App for RsDashApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let snap = self.engine.snapshot();

        if self.tab == NavTab::Logs && self.show_daemon_log {
            self.refresh_daemon_log_if_due();
        }

        // Top Navigation & Status Header
        egui::TopBottomPanel::top("rs_dash_header")
            .frame(
                egui::Frame::new()
                    .fill(BG_SURFACE)
                    .stroke(egui::Stroke::new(1.0, BORDER_SUBTLE))
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ctx, |ui| {
                self.render_header(ui, &snap);
            });

        // Bottom Status Bar
        egui::TopBottomPanel::bottom("rs_dash_footer")
            .frame(
                egui::Frame::new()
                    .fill(BG_SURFACE)
                    .stroke(egui::Stroke::new(1.0, BORDER_SUBTLE))
                    .inner_margin(egui::Margin::symmetric(16, 6)),
            )
            .show(ctx, |ui| {
                self.render_footer(ui, &snap);
            });

        // Main Workspace
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG_BASE)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| match self.tab {
                NavTab::Dashboard => self.render_dashboard(ui, &snap),
                NavTab::Processes => self.render_processes(ui, &snap),
                NavTab::Logs => self.render_logs(ui, &snap),
                NavTab::Settings => self.render_settings(ui, &snap),
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.engine.shutdown();
    }
}

impl RsDashApp {
    fn render_header(&mut self, ui: &mut egui::Ui, snap: &DashSnapshot) {
        ui.horizontal_wrapped(|ui| {
            // Brand
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                egui::Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(56, 189, 248, 30))
                    .stroke(egui::Stroke::new(1.0, ACCENT_CYAN))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("RS")
                                .size(14.0)
                                .strong()
                                .color(ACCENT_CYAN),
                        );
                    });

                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(
                        RichText::new("Resguard Dashboard")
                            .size(15.5)
                            .strong()
                            .color(TEXT_PRIMARY),
                    );
                    ui.label(
                        RichText::new(format!(
                            "v{} · {} · {} cores",
                            env!("CARGO_PKG_VERSION"),
                            snap.backend_name,
                            snap.num_cores
                        ))
                        .size(11.0)
                        .color(TEXT_MUTED),
                    );
                });
            });

            ui.add_space(10.0);

            // Navigation Tabs
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (NavTab::Dashboard, "Dashboard"),
                    (NavTab::Processes, "Processes"),
                    (NavTab::Logs, "Events & Logs"),
                    (NavTab::Settings, "Settings"),
                ] {
                    let selected = self.tab == tab;
                    let btn = egui::Button::new(
                        RichText::new(label)
                            .size(12.5)
                            .strong()
                            .color(if selected { TEXT_PRIMARY } else { TEXT_SECONDARY }),
                    )
                    .fill(if selected {
                        Color32::from_rgba_unmultiplied(56, 189, 248, 36)
                    } else {
                        BG_ELEVATED
                    })
                    .stroke(egui::Stroke::new(
                        1.0,
                        if selected { ACCENT_CYAN } else { BORDER_SUBTLE },
                    ))
                    .corner_radius(egui::CornerRadius::same(7));

                    if ui.add(btn).clicked() {
                        if tab == NavTab::Settings && self.tab != NavTab::Settings {
                            self.draft_cfg = snap.config.clone();
                            self.settings_status = None;
                        }
                        self.tab = tab;
                    }
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Zone badge
                let zc = zone_color(snap.zone);
                pill_badge(ui, zone_label(snap.zone), zc);

                // Mode selector
                egui::Frame::new()
                    .fill(BG_ELEVATED)
                    .stroke(egui::Stroke::new(1.0, BORDER_SUBTLE))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(4, 3))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 3.0;
                            for (mode, short_label, col) in [
                                (GuardMode::TelemetryOnly, "Observe", TEXT_SECONDARY),
                                (GuardMode::Monitor, "Dry-Run", ACCENT_CYAN),
                                (GuardMode::Active, "Active Guard", STATUS_GREEN),
                            ] {
                                let is_cur = snap.guard_mode == mode;
                                let btn = egui::Button::new(
                                    RichText::new(short_label)
                                        .size(11.5)
                                        .strong()
                                        .color(if is_cur { col } else { TEXT_MUTED }),
                                )
                                .fill(if is_cur {
                                    Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 32)
                                } else {
                                    Color32::TRANSPARENT
                                })
                                .stroke(egui::Stroke::new(
                                    if is_cur { 1.0 } else { 0.0 },
                                    col,
                                ))
                                .corner_radius(egui::CornerRadius::same(6));
                                if ui.add(btn).clicked() && !is_cur {
                                    self.engine.send(EngineCmd::SetMode(mode));
                                }
                            }
                        });
                    });

                // External daemon status badge
                if let Some(ref d) = snap.external_daemon {
                    ui.horizontal(|ui| {
                        pill_badge(ui, &format!("Daemon PID {}", d.pid), STATUS_GREEN);
                    })
                    .response
                    .on_hover_text(format!("Running daemon: {}", d.exe_path));
                } else {
                    pill_badge(ui, "Daemon Off", TEXT_MUTED);
                }
            });
        });

        if let Some(ref err) = snap.last_error {
            ui.add_space(6.0);
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(239, 68, 68, 28))
                .stroke(egui::Stroke::new(1.0, STATUS_RED))
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(egui::Margin::symmetric(10, 4))
                .show(ui, |ui| {
                    ui.label(RichText::new(format!("Warning: {}", err)).size(12.0).color(STATUS_RED));
                });
        }
    }

    fn render_footer(&self, ui: &mut egui::Ui, snap: &DashSnapshot) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "Tick {}ms  ·  Last sample {}  ·  Processes {}  ·  Protected {}",
                    snap.config.tick_ms,
                    snap.last_tick_ts,
                    snap.processes.len(),
                    snap.protected_count,
                ))
                .size(11.0)
                .monospace()
                .color(TEXT_MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mode_desc = match snap.guard_mode {
                    GuardMode::Monitor => "Dry-Run simulation active (no processes modified)",
                    GuardMode::Active => "Active Guard enforcing QoS, duty-cycle & MEM/SSD limits",
                    GuardMode::TelemetryOnly => "Telemetry sampling only",
                };
                ui.label(RichText::new(mode_desc).size(11.0).color(TEXT_SECONDARY));
            });
        });
    }

    fn render_dashboard(&mut self, ui: &mut egui::Ui, snap: &DashSnapshot) {
        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            // 1. 4-Card KPI Summary Grid
            let avail_w = ui.available_width();
            let cols = if avail_w >= 860.0 { 4 } else if avail_w >= 520.0 { 2 } else { 1 };
            let gap = 10.0;
            let card_w = (avail_w - gap * ((cols - 1) as f32)) / (cols as f32);

            let cpu_hist: Vec<f64> = snap.history.iter().map(|h| h.cpu_total).collect();
            let ram_hist: Vec<f64> = snap.history.iter().map(|h| h.ram_pct).collect();
            let swap_hist: Vec<f64> = snap.history.iter().map(|h| h.swap_pct).collect();
            let gpu_hist: Vec<f64> = snap
                .history
                .iter()
                .map(|h| h.gpu.unwrap_or(0.0))
                .collect();

            let m = &snap.metrics;
            let cfg = &snap.config;

            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(gap, gap);

                // Card 1: CPU
                ui.allocate_ui(Vec2::new(card_w, 138.0), |ui| {
                    card_frame().show(ui, |ui| {
                        ui.set_min_width(card_w - 30.0);
                        let cpu_col = pct_color(m.cpu_total_pct, cfg.cpu_soft, cfg.cpu_high);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("CPU LOAD").size(11.0).strong().color(TEXT_MUTED));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("load {:.2}", m.load_1m))
                                        .size(11.0)
                                        .monospace()
                                        .color(TEXT_SECONDARY),
                                );
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{:.1}%", m.cpu_total_pct))
                                    .size(26.0)
                                    .strong()
                                    .monospace()
                                    .color(cpu_col),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "u:{:.0}% s:{:.0}%",
                                        m.cpu_user_pct, m.cpu_sys_pct
                                    ))
                                    .size(11.0)
                                    .monospace()
                                    .color(TEXT_SECONDARY),
                                );
                            });
                        });
                        draw_threshold_bar(
                            ui,
                            m.cpu_total_pct,
                            cpu_col,
                            &[
                                (cfg.cpu_soft, STATUS_AMBER),
                                (cfg.cpu_high, ACCENT_PURPLE),
                                (cfg.cpu_cap, STATUS_RED),
                            ],
                        );
                        ui.add_space(2.0);
                        draw_sparkline(ui, &cpu_hist, 100.0, cpu_col);
                    });
                });

                // Card 2: Memory (RAM)
                ui.allocate_ui(Vec2::new(card_w, 138.0), |ui| {
                    card_frame().show(ui, |ui| {
                        ui.set_min_width(card_w - 30.0);
                        let ram_col = pct_color(m.ram_used_pct, cfg.ram_high_pct, cfg.ram_critical_pct);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("MEMORY").size(11.0).strong().color(TEXT_MUTED));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                pill_badge(
                                    ui,
                                    &m.memory_pressure.to_string(),
                                    pressure_color(m.memory_pressure),
                                );
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{:.1}%", m.ram_used_pct))
                                    .size(26.0)
                                    .strong()
                                    .monospace()
                                    .color(ram_col),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{:.1}/{:.1} GB",
                                        m.ram_used_gb(),
                                        m.ram_total_gb()
                                    ))
                                    .size(11.0)
                                    .monospace()
                                    .color(TEXT_SECONDARY),
                                );
                            });
                        });
                        draw_threshold_bar(
                            ui,
                            m.ram_used_pct,
                            ram_col,
                            &[
                                (cfg.ram_high_pct, STATUS_AMBER),
                                (cfg.ram_critical_pct, STATUS_RED),
                            ],
                        );
                        ui.add_space(2.0);
                        draw_sparkline(ui, &ram_hist, 100.0, ram_col);
                    });
                });

                // Card 3: Swap & SSD Guard
                ui.allocate_ui(Vec2::new(card_w, 138.0), |ui| {
                    card_frame().show(ui, |ui| {
                        ui.set_min_width(card_w - 30.0);
                        let ssd_gb = m.disk_free_gb();
                        let ssd_col = if ssd_gb < cfg.disk_min_free_gb {
                            STATUS_RED
                        } else if ssd_gb < cfg.disk_min_free_gb * 2.0 {
                            STATUS_AMBER
                        } else {
                            STATUS_GREEN
                        };
                        let swap_pct = if m.swap_total_bytes > 0 {
                            (m.swap_used_bytes as f64 / m.swap_total_bytes as f64) * 100.0
                        } else {
                            0.0
                        };
                        let swap_col = if m.swap_used_mb() > cfg.swap_warn_mb {
                            STATUS_AMBER
                        } else {
                            ACCENT_CYAN
                        };

                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("SWAP & ROOT SSD")
                                    .size(11.0)
                                    .strong()
                                    .color(TEXT_MUTED),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("min {:.0}G", cfg.disk_min_free_gb))
                                        .size(11.0)
                                        .monospace()
                                        .color(TEXT_MUTED),
                                );
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{:.1} GB", ssd_gb))
                                    .size(26.0)
                                    .strong()
                                    .monospace()
                                    .color(ssd_col),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "Swap {:.0}/{:.0}M",
                                        m.swap_used_mb(),
                                        m.swap_total_mb()
                                    ))
                                    .size(11.0)
                                    .monospace()
                                    .color(swap_col),
                                );
                            });
                        });
                        draw_threshold_bar(ui, swap_pct, swap_col, &[]);
                        ui.add_space(2.0);
                        draw_sparkline(ui, &swap_hist, 100.0, swap_col);
                    });
                });

                // Card 4: GPU & Governor Activity
                ui.allocate_ui(Vec2::new(card_w, 138.0), |ui| {
                    card_frame().show(ui, |ui| {
                        ui.set_min_width(card_w - 30.0);
                        let gpu_val = m.gpu_pct.unwrap_or(0.0);
                        let gpu_col = if m.gpu_pct.is_some() {
                            pct_color(gpu_val, cfg.cpu_soft, cfg.cpu_high)
                        } else {
                            TEXT_MUTED
                        };
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("GPU & GUARD")
                                    .size(11.0)
                                    .strong()
                                    .color(TEXT_MUTED),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("duty {:.0}%", snap.throttle_ratio * 100.0))
                                        .size(11.0)
                                        .monospace()
                                        .color(if snap.throttle_ratio > 0.02 {
                                            STATUS_ORANGE
                                        } else {
                                            TEXT_SECONDARY
                                        }),
                                );
                            });
                        });
                        ui.horizontal(|ui| {
                            let gpu_str = m
                                .gpu_pct
                                .map(|g| format!("{:.0}%", g))
                                .unwrap_or_else(|| "N/A".to_string());
                            ui.label(
                                RichText::new(gpu_str)
                                    .size(26.0)
                                    .strong()
                                    .monospace()
                                    .color(gpu_col),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "bg:{}  paused:{}",
                                        snap.demoted.len(),
                                        snap.emergency_paused.len()
                                    ))
                                    .size(11.5)
                                    .monospace()
                                    .color(if !snap.emergency_paused.is_empty() {
                                        STATUS_RED
                                    } else if !snap.demoted.is_empty() {
                                        STATUS_AMBER
                                    } else {
                                        TEXT_SECONDARY
                                    }),
                                );
                            });
                        });
                        draw_threshold_bar(ui, gpu_val, ACCENT_INDIGO, &[]);
                        ui.add_space(2.0);
                        draw_sparkline(ui, &gpu_hist, 100.0, ACCENT_INDIGO);
                    });
                });
            });

            ui.add_space(10.0);

            // 2. Live Time-Series Charts Row
            let timestamps: Vec<String> = snap.history.iter().map(|h| h.timestamp.clone()).collect();
            let user_hist: Vec<f64> = snap.history.iter().map(|h| h.cpu_user).collect();
            let sys_hist: Vec<f64> = snap.history.iter().map(|h| h.cpu_sys).collect();
            let duty_hist: Vec<f64> = snap.history.iter().map(|h| h.throttle_pct).collect();

            let avail_w = ui.available_width();
            let chart_cols = if avail_w >= 780.0 { 2 } else { 1 };
            let chart_w = if chart_cols == 2 {
                (avail_w - 10.0) * 0.5
            } else {
                avail_w
            };

            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);

                ui.allocate_ui(Vec2::new(chart_w, 220.0), |ui| {
                    card_frame().show(ui, |ui| {
                        ui.set_min_width(chart_w - 30.0);
                        draw_time_series_chart(
                            ui,
                            "CPU & Duty-Cycle History",
                            "%",
                            100.0,
                            165.0,
                            &timestamps,
                            &[
                                Series {
                                    label: "Total",
                                    color: ACCENT_CYAN,
                                    values: &cpu_hist,
                                    fill_alpha: 42,
                                },
                                Series {
                                    label: "User",
                                    color: STATUS_GREEN,
                                    values: &user_hist,
                                    fill_alpha: 0,
                                },
                                Series {
                                    label: "Sys",
                                    color: STATUS_AMBER,
                                    values: &sys_hist,
                                    fill_alpha: 0,
                                },
                                Series {
                                    label: "Duty",
                                    color: STATUS_RED,
                                    values: &duty_hist,
                                    fill_alpha: 20,
                                },
                            ],
                            &[
                                ThresholdLine {
                                    label: "release",
                                    value: cfg.cpu_release,
                                    color: STATUS_GREEN,
                                },
                                ThresholdLine {
                                    label: "soft",
                                    value: cfg.cpu_soft,
                                    color: STATUS_AMBER,
                                },
                                ThresholdLine {
                                    label: "high",
                                    value: cfg.cpu_high,
                                    color: ACCENT_PURPLE,
                                },
                                ThresholdLine {
                                    label: "cap",
                                    value: cfg.cpu_cap,
                                    color: STATUS_RED,
                                },
                            ],
                        );
                    });
                });

                ui.allocate_ui(Vec2::new(chart_w, 220.0), |ui| {
                    card_frame().show(ui, |ui| {
                        ui.set_min_width(chart_w - 30.0);
                        draw_time_series_chart(
                            ui,
                            "Memory, Swap & GPU History",
                            "%",
                            100.0,
                            165.0,
                            &timestamps,
                            &[
                                Series {
                                    label: "RAM",
                                    color: ACCENT_BLUE,
                                    values: &ram_hist,
                                    fill_alpha: 40,
                                },
                                Series {
                                    label: "Swap",
                                    color: STATUS_AMBER,
                                    values: &swap_hist,
                                    fill_alpha: 18,
                                },
                                Series {
                                    label: "GPU",
                                    color: ACCENT_PURPLE,
                                    values: &gpu_hist,
                                    fill_alpha: 0,
                                },
                            ],
                            &[
                                ThresholdLine {
                                    label: "ram_high",
                                    value: cfg.ram_high_pct,
                                    color: STATUS_AMBER,
                                },
                                ThresholdLine {
                                    label: "ram_crit",
                                    value: cfg.ram_critical_pct,
                                    color: STATUS_RED,
                                },
                            ],
                        );
                    });
                });
            });

            ui.add_space(10.0);

            // 3. Bottom Split: Top Active Processes + Active Interventions / Events
            let split_two = avail_w >= 820.0;
            let left_w = if split_two { (avail_w - 10.0) * 0.57 } else { avail_w };
            let right_w = if split_two { (avail_w - 10.0) * 0.43 } else { avail_w };

            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);

                // Left: Top Consumers
                ui.allocate_ui(Vec2::new(left_w, 285.0), |ui| {
                    card_frame().show(ui, |ui| {
                        ui.set_min_width(left_w - 30.0);
                        ui.set_min_height(255.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Top Active Processes")
                                    .size(13.5)
                                    .strong()
                                    .color(TEXT_PRIMARY),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("View all →").clicked() {
                                    self.tab = NavTab::Processes;
                                }
                            });
                        });
                        ui.add_space(4.0);
                        self.render_process_mini_list(ui, snap, 8);
                    });
                });

                if split_two {
                    // Right: Governor Interventions & Recent Events
                    ui.allocate_ui(Vec2::new(right_w, 285.0), |ui| {
                        self.render_interventions_and_events_card(ui, snap, right_w);
                    });
                }
            });

            if !split_two {
                ui.add_space(10.0);
                self.render_interventions_and_events_card(ui, snap, right_w);
            }
        });
    }

    fn render_interventions_and_events_card(
        &self,
        ui: &mut egui::Ui,
        snap: &DashSnapshot,
        width: f32,
    ) {
        card_frame().show(ui, |ui| {
            ui.set_min_width(width - 30.0);
            ui.set_min_height(255.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Governor Interventions & Feed")
                        .size(13.5)
                        .strong()
                        .color(TEXT_PRIMARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !snap.events.is_empty() && ui.small_button("Clear").clicked() {
                        self.engine.send(EngineCmd::ClearEvents);
                    }
                });
            });
            ui.add_space(4.0);

            if !snap.demoted.is_empty() || !snap.emergency_paused.is_empty() {
                subcard_frame().show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (pid, name) in &snap.emergency_paused {
                            pill_badge(ui, &format!("PAUSED {} ({})", name, pid), STATUS_CRIMSON);
                            if ui.small_button("Resume").clicked() {
                                self.engine.send(EngineCmd::ManualResume { pid: *pid });
                            }
                        }
                        for (pid, name) in &snap.demoted {
                            pill_badge(ui, &format!("BG {} ({})", name, pid), STATUS_AMBER);
                            if ui.small_button("Restore").clicked() {
                                self.engine.send(EngineCmd::ManualSetBg {
                                    pid: *pid,
                                    enable: false,
                                });
                            }
                        }
                    });
                });
                ui.add_space(6.0);
            }

            ScrollArea::vertical()
                .id_salt("dash_events_scroll")
                .max_height(195.0)
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    if snap.events.is_empty() {
                        ui.label(
                            RichText::new("No governor interventions yet. System is healthy.")
                                .size(12.0)
                                .color(TEXT_MUTED),
                        );
                    } else {
                        for ev in snap.events.iter().take(40) {
                            let col = event_kind_color(ev.kind);
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.label(
                                    RichText::new(&ev.timestamp)
                                        .size(11.0)
                                        .monospace()
                                        .color(TEXT_MUTED),
                                );
                                ui.label(RichText::new(&ev.message).size(11.5).color(col));
                            });
                        }
                    }
                });
        });
    }

    fn render_process_mini_list(&self, ui: &mut egui::Ui, snap: &DashSnapshot, limit: usize) {
        ScrollArea::vertical()
            .id_salt("mini_procs_scroll")
            .max_height(210.0)
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                egui::Grid::new("mini_procs_grid")
                    .num_columns(5)
                    .spacing([10.0, 6.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(RichText::new("PID").size(11.0).strong().color(TEXT_MUTED));
                        ui.label(RichText::new("Process").size(11.0).strong().color(TEXT_MUTED));
                        ui.label(RichText::new("CPU").size(11.0).strong().color(TEXT_MUTED));
                        ui.label(RichText::new("RAM").size(11.0).strong().color(TEXT_MUTED));
                        ui.label(RichText::new("Action").size(11.0).strong().color(TEXT_MUTED));
                        ui.end_row();

                        for row in snap.processes.iter().take(limit) {
                            let p = &row.sample;
                            ui.label(
                                RichText::new(format!("{}", p.pid))
                                    .size(11.5)
                                    .monospace()
                                    .color(TEXT_MUTED),
                            );
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(&p.name)
                                        .size(12.0)
                                        .strong()
                                        .color(TEXT_PRIMARY),
                                );
                                render_row_state_pill(ui, row);
                            });
                            let cpu_col = pct_color(
                                p.cpu_pct,
                                snap.config.min_proc_cpu_pct,
                                snap.config.min_proc_cpu_pct * 3.0,
                            );
                            ui.label(
                                RichText::new(format!("{:5.1}%", p.cpu_pct))
                                    .size(11.5)
                                    .monospace()
                                    .color(cpu_col),
                            );
                            ui.label(
                                RichText::new(format!("{:.0}M", p.rss_mb()))
                                    .size(11.5)
                                    .monospace()
                                    .color(TEXT_SECONDARY),
                            );
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                if row.is_protected {
                                    ui.label(
                                        RichText::new("Shielded")
                                            .size(11.0)
                                            .color(ACCENT_CYAN),
                                    );
                                } else {
                                    let bg_lbl = if row.is_demoted { "Norm" } else { "BG" };
                                    if ui.small_button(bg_lbl).clicked() {
                                        self.engine.send(EngineCmd::ManualSetBg {
                                            pid: p.pid,
                                            enable: !row.is_demoted,
                                        });
                                    }
                                    if p.is_stopped {
                                        if ui.small_button("Resume").clicked() {
                                            self.engine
                                                .send(EngineCmd::ManualResume { pid: p.pid });
                                        }
                                    } else if ui.small_button("Pause").clicked() {
                                        self.engine.send(EngineCmd::ManualPause { pid: p.pid });
                                    }
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
    }

    fn render_processes(&mut self, ui: &mut egui::Ui, snap: &DashSnapshot) {
        card_frame().show(ui, |ui| {
            // Toolbar
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Search:").size(12.0).color(TEXT_SECONDARY));
                ui.add(
                    egui::TextEdit::singleline(&mut self.proc_search)
                        .hint_text("name, pid, or path...")
                        .desired_width(210.0),
                );
                if !self.proc_search.is_empty() && ui.small_button("×").clicked() {
                    self.proc_search.clear();
                }

                ui.separator();

                for (filter, lbl) in [
                    (ProcFilter::All, "All"),
                    (ProcFilter::ActiveCpu, "Active (≥1% CPU)"),
                    (ProcFilter::Governed, "Governed / Paused"),
                    (ProcFilter::Protected, "Protected"),
                ] {
                    let sel = self.proc_filter == filter;
                    if ui.selectable_label(sel, lbl).clicked() {
                        self.proc_filter = filter;
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (sort, lbl) in [
                        (ProcSort::NameAsc, "Name"),
                        (ProcSort::PidAsc, "PID"),
                        (ProcSort::RamDesc, "RAM"),
                        (ProcSort::CpuDesc, "CPU %"),
                    ] {
                        let sel = self.proc_sort == sort;
                        if ui.selectable_label(sel, lbl).clicked() {
                            self.proc_sort = sort;
                        }
                    }
                    ui.label(RichText::new("Sort:").size(11.5).color(TEXT_MUTED));
                });
            });

            ui.add_space(8.0);

            let needle = self.proc_search.trim().to_ascii_lowercase();
            let mut filtered: Vec<&ProcessRow> = snap
                .processes
                .iter()
                .filter(|r| {
                    let match_filter = match self.proc_filter {
                        ProcFilter::All => true,
                        ProcFilter::ActiveCpu => r.sample.cpu_pct >= 1.0,
                        ProcFilter::Governed => {
                            r.is_demoted || r.is_emergency_paused || r.sample.is_stopped
                        }
                        ProcFilter::Protected => r.is_protected,
                    };
                    if !match_filter {
                        return false;
                    }
                    if needle.is_empty() {
                        return true;
                    }
                    r.sample.name.to_ascii_lowercase().contains(&needle)
                        || r.sample.exe_path.to_ascii_lowercase().contains(&needle)
                        || r.sample.pid.to_string().contains(&needle)
                })
                .collect();

            match self.proc_sort {
                ProcSort::CpuDesc => filtered.sort_by(|a, b| {
                    b.sample
                        .cpu_pct
                        .partial_cmp(&a.sample.cpu_pct)
                        .unwrap_or(std::cmp::Ordering::Equal)
                }),
                ProcSort::RamDesc => {
                    filtered.sort_by(|a, b| b.sample.rss_bytes.cmp(&a.sample.rss_bytes))
                }
                ProcSort::PidAsc => filtered.sort_by_key(|r| r.sample.pid),
                ProcSort::NameAsc => filtered.sort_by(|a, b| {
                    a.sample
                        .name
                        .to_ascii_lowercase()
                        .cmp(&b.sample.name.to_ascii_lowercase())
                }),
            }

            ui.label(
                RichText::new(format!(
                    "Showing {} of {} processes",
                    filtered.len(),
                    snap.processes.len()
                ))
                .size(11.0)
                .color(TEXT_MUTED),
            );
            ui.add_space(4.0);

            ScrollArea::both().auto_shrink([false; 2]).show(ui, |ui| {
                egui::Grid::new("full_procs_grid")
                    .num_columns(7)
                    .spacing([12.0, 6.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(RichText::new("PID").size(11.5).strong().color(TEXT_MUTED));
                        ui.label(RichText::new("PPID").size(11.5).strong().color(TEXT_MUTED));
                        ui.label(RichText::new("Name").size(11.5).strong().color(TEXT_MUTED));
                        ui.label(RichText::new("State").size(11.5).strong().color(TEXT_MUTED));
                        ui.label(RichText::new("CPU %").size(11.5).strong().color(TEXT_MUTED));
                        ui.label(RichText::new("RAM").size(11.5).strong().color(TEXT_MUTED));
                        ui.label(RichText::new("Controls / Path").size(11.5).strong().color(TEXT_MUTED));
                        ui.end_row();

                        for row in filtered.into_iter().take(250) {
                            let p = &row.sample;
                            ui.label(
                                RichText::new(format!("{}", p.pid))
                                    .size(11.5)
                                    .monospace()
                                    .color(TEXT_MUTED),
                            );
                            ui.label(
                                RichText::new(format!("{}", p.ppid))
                                    .size(11.5)
                                    .monospace()
                                    .color(TEXT_MUTED),
                            );
                            ui.label(
                                RichText::new(&p.name)
                                    .size(12.0)
                                    .strong()
                                    .color(TEXT_PRIMARY),
                            );
                            ui.horizontal(|ui| {
                                render_row_state_pill(ui, row);
                                if !row.is_protected
                                    && !row.is_demoted
                                    && !row.is_emergency_paused
                                    && !p.is_stopped
                                {
                                    ui.label(
                                        RichText::new("Normal")
                                            .size(11.0)
                                            .color(TEXT_MUTED),
                                    );
                                }
                            });
                            let cpu_col = pct_color(
                                p.cpu_pct,
                                snap.config.min_proc_cpu_pct,
                                snap.config.min_proc_cpu_pct * 3.0,
                            );
                            ui.label(
                                RichText::new(format!("{:5.1}%", p.cpu_pct))
                                    .size(11.5)
                                    .monospace()
                                    .color(cpu_col),
                            );
                            ui.label(
                                RichText::new(format!("{:6.1} MB", p.rss_mb()))
                                    .size(11.5)
                                    .monospace()
                                    .color(TEXT_SECONDARY),
                            );
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                let is_named_protected = snap
                                    .config
                                    .protect_names
                                    .iter()
                                    .any(|n| n.eq_ignore_ascii_case(&p.name));
                                let prot_lbl = if is_named_protected {
                                    "Unprotect"
                                } else {
                                    "Protect"
                                };
                                if ui.small_button(prot_lbl).clicked() {
                                    self.engine
                                        .send(EngineCmd::ToggleProtectName(p.name.clone()));
                                }

                                if !row.is_protected {
                                    let bg_lbl = if row.is_demoted { "Restore QoS" } else { "BG QoS" };
                                    if ui.small_button(bg_lbl).clicked() {
                                        self.engine.send(EngineCmd::ManualSetBg {
                                            pid: p.pid,
                                            enable: !row.is_demoted,
                                        });
                                    }
                                    if p.is_stopped {
                                        if ui.small_button("Resume").clicked() {
                                            self.engine
                                                .send(EngineCmd::ManualResume { pid: p.pid });
                                        }
                                    } else if ui.small_button("Pause").clicked() {
                                        self.engine.send(EngineCmd::ManualPause { pid: p.pid });
                                    }
                                }

                                if !p.exe_path.is_empty() {
                                    ui.label(
                                        RichText::new(&p.exe_path)
                                            .size(10.5)
                                            .monospace()
                                            .color(TEXT_MUTED),
                                    );
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
        });
    }

    fn render_logs(&mut self, ui: &mut egui::Ui, snap: &DashSnapshot) {
        let avail_w = ui.available_width();
        let split = avail_w >= 860.0 && self.show_daemon_log;
        let left_w = if split { (avail_w - 10.0) * 0.5 } else { avail_w };

        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);

            ui.allocate_ui(Vec2::new(left_w, ui.available_height()), |ui| {
                card_frame().show(ui, |ui| {
                    ui.set_min_width(left_w - 30.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Dashboard Governor Events")
                                .size(13.5)
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.checkbox(&mut self.show_daemon_log, "Show daemon log");
                            if ui.small_button("Clear").clicked() {
                                self.engine.send(EngineCmd::ClearEvents);
                            }
                        });
                    });
                    ui.add_space(6.0);
                    ScrollArea::vertical()
                        .id_salt("full_events_scroll")
                        .auto_shrink([false; 2])
                        .show(ui, |ui| {
                            if snap.events.is_empty() {
                                ui.label(
                                    RichText::new("No events recorded in this session yet.")
                                        .color(TEXT_MUTED),
                                );
                            } else {
                                for ev in &snap.events {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            RichText::new(&ev.timestamp)
                                                .size(11.0)
                                                .monospace()
                                                .color(TEXT_MUTED),
                                        );
                                        ui.label(
                                            RichText::new(&ev.message)
                                                .size(12.0)
                                                .color(event_kind_color(ev.kind)),
                                        );
                                    });
                                }
                            }
                        });
                });
            });

            if split {
                ui.allocate_ui(Vec2::new(left_w, ui.available_height()), |ui| {
                    card_frame().show(ui, |ui| {
                        ui.set_min_width(left_w - 30.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Daemon Log Tail")
                                    .size(13.5)
                                    .strong()
                                    .color(TEXT_PRIMARY),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut self.daemon_log_path)
                                    .desired_width(190.0),
                            );
                            if ui.small_button("Refresh").clicked() {
                                self.last_log_poll = Instant::now() - Duration::from_secs(10);
                                self.refresh_daemon_log_if_due();
                            }
                        });
                        ui.add_space(6.0);
                        ScrollArea::vertical()
                            .id_salt("daemon_log_scroll")
                            .stick_to_bottom(true)
                            .auto_shrink([false; 2])
                            .show(ui, |ui| {
                                if let Some(ref err) = self.daemon_log_err {
                                    ui.label(RichText::new(err).size(12.0).color(TEXT_MUTED));
                                } else {
                                    for line in &self.daemon_log_lines {
                                        ui.label(
                                            RichText::new(line)
                                                .size(11.0)
                                                .monospace()
                                                .color(TEXT_SECONDARY),
                                        );
                                    }
                                }
                            });
                    });
                });
            }
        });
    }

    fn render_settings(&mut self, ui: &mut egui::Ui, snap: &DashSnapshot) {
        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            let avail_w = ui.available_width();
            let cols = if avail_w >= 820.0 { 2 } else { 1 };
            let col_w = if cols == 2 {
                (avail_w - 10.0) * 0.5
            } else {
                avail_w
            };

            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);

                // CPU Governor Settings
                ui.allocate_ui(Vec2::new(col_w, 290.0), |ui| {
                    card_frame().show(ui, |ui| {
                        ui.set_min_width(col_w - 30.0);
                        ui.label(
                            RichText::new("CPU Governor Thresholds")
                                .size(14.0)
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        ui.add_space(6.0);
                        egui::Grid::new("cpu_settings_grid")
                            .num_columns(2)
                            .spacing([14.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("Release threshold (%)");
                                ui.add(
                                    egui::Slider::new(&mut self.draft_cfg.cpu_release, 20.0..=90.0)
                                        .suffix("%"),
                                );
                                ui.end_row();

                                ui.label("Soft QoS / E-core (%)");
                                ui.add(
                                    egui::Slider::new(&mut self.draft_cfg.cpu_soft, 30.0..=98.0)
                                        .suffix("%"),
                                );
                                ui.end_row();

                                ui.label("High duty-cycle (%)");
                                ui.add(
                                    egui::Slider::new(&mut self.draft_cfg.cpu_high, 50.0..=99.0)
                                        .suffix("%"),
                                );
                                ui.end_row();

                                ui.label("Hard cap ceiling (%)");
                                ui.add(
                                    egui::Slider::new(&mut self.draft_cfg.cpu_cap, 55.0..=100.0)
                                        .suffix("%"),
                                );
                                ui.end_row();

                                ui.label("Soft sustain (ms)");
                                ui.add(
                                    egui::DragValue::new(&mut self.draft_cfg.soft_sustain_ms)
                                        .range(0..=30_000)
                                        .speed(50)
                                        .suffix(" ms"),
                                );
                                ui.end_row();

                                ui.label("Release cooldown (ms)");
                                ui.add(
                                    egui::DragValue::new(&mut self.draft_cfg.release_cooldown_ms)
                                        .range(0..=60_000)
                                        .speed(100)
                                        .suffix(" ms"),
                                );
                                ui.end_row();
                            });
                    });
                });

                // Memory, Swap, SSD & Sampling Settings
                ui.allocate_ui(Vec2::new(col_w, 290.0), |ui| {
                    card_frame().show(ui, |ui| {
                        ui.set_min_width(col_w - 30.0);
                        ui.label(
                            RichText::new("Memory, Swap, SSD & Sampling")
                                .size(14.0)
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        ui.add_space(6.0);
                        egui::Grid::new("mem_settings_grid")
                            .num_columns(2)
                            .spacing([14.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("RAM high watermark (%)");
                                ui.add(
                                    egui::Slider::new(&mut self.draft_cfg.ram_high_pct, 50.0..=98.0)
                                        .suffix("%"),
                                );
                                ui.end_row();

                                ui.label("RAM critical pause (%)");
                                ui.add(
                                    egui::Slider::new(
                                        &mut self.draft_cfg.ram_critical_pct,
                                        60.0..=99.5,
                                    )
                                    .suffix("%"),
                                );
                                ui.end_row();

                                ui.label("SSD min free space (GB)");
                                ui.add(
                                    egui::DragValue::new(&mut self.draft_cfg.disk_min_free_gb)
                                        .range(1.0..=200.0)
                                        .speed(0.5)
                                        .suffix(" GB"),
                                );
                                ui.end_row();

                                ui.label("Swap warning (MB)");
                                ui.add(
                                    egui::DragValue::new(&mut self.draft_cfg.swap_warn_mb)
                                        .range(128.0..=65536.0)
                                        .speed(64.0)
                                        .suffix(" MB"),
                                );
                                ui.end_row();

                                ui.label("Tick interval (ms)");
                                ui.add(
                                    egui::Slider::new(&mut self.draft_cfg.tick_ms, 100..=5000)
                                        .suffix(" ms"),
                                );
                                ui.end_row();

                                ui.label("Min process CPU (%)");
                                ui.add(
                                    egui::DragValue::new(&mut self.draft_cfg.min_proc_cpu_pct)
                                        .range(0.5..=80.0)
                                        .speed(0.5)
                                        .suffix("%"),
                                );
                                ui.end_row();

                                ui.label("Min process RSS (MB)");
                                ui.add(
                                    egui::DragValue::new(&mut self.draft_cfg.min_proc_rss_mb)
                                        .range(16.0..=8192.0)
                                        .speed(16.0)
                                        .suffix(" MB"),
                                );
                                ui.end_row();

                                ui.label("Desktop notifications");
                                ui.checkbox(&mut self.draft_cfg.notify, "Notify on emergency pause");
                                ui.end_row();
                            });
                    });
                });
            });

            ui.add_space(10.0);

            // Protected Process Names Editor
            card_frame().show(ui, |ui| {
                ui.label(
                    RichText::new("Protected Process Names (Never Throttled or Paused)")
                        .size(14.0)
                        .strong()
                        .color(TEXT_PRIMARY),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.new_protect_name)
                            .hint_text("Add process or app name (e.g. Docker, Ollama, ffmpeg)")
                            .desired_width(300.0),
                    );
                    let add_clicked = ui.button("Add Protected Name").clicked()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if add_clicked {
                        let trimmed = self.new_protect_name.trim().to_string();
                        if !trimmed.is_empty()
                            && !self
                                .draft_cfg
                                .protect_names
                                .iter()
                                .any(|p| p.eq_ignore_ascii_case(&trimmed))
                        {
                            self.draft_cfg.protect_names.push(trimmed);
                            self.new_protect_name.clear();
                        }
                    }
                });
                ui.add_space(8.0);
                let mut remove_idx: Option<usize> = None;
                ui.horizontal_wrapped(|ui| {
                    for (idx, name) in self.draft_cfg.protect_names.iter().enumerate() {
                        egui::Frame::new()
                            .fill(BG_ELEVATED)
                            .stroke(egui::Stroke::new(1.0, BORDER_SUBTLE))
                            .corner_radius(egui::CornerRadius::same(99))
                            .inner_margin(egui::Margin::symmetric(8, 3))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    ui.label(RichText::new(name).size(11.5).color(TEXT_SECONDARY));
                                    if ui.small_button("×").clicked() {
                                        remove_idx = Some(idx);
                                    }
                                });
                            });
                    }
                });
                if let Some(idx) = remove_idx {
                    self.draft_cfg.protect_names.remove(idx);
                }
            });

            ui.add_space(10.0);

            // Action Bar
            card_frame().show(ui, |ui| {
                let validation = config::validate_config(&self.draft_cfg);
                if let Err(ref err) = validation {
                    ui.label(
                        RichText::new(format!("Validation error: {}", err))
                            .size(12.0)
                            .color(STATUS_RED),
                    );
                    ui.add_space(6.0);
                }

                ui.horizontal_wrapped(|ui| {
                    let valid = validation.is_ok();
                    if ui
                        .add_enabled(
                            valid,
                            egui::Button::new(
                                RichText::new("Apply Live").strong().color(TEXT_PRIMARY),
                            )
                            .fill(Color32::from_rgba_unmultiplied(56, 189, 248, 45)),
                        )
                        .clicked()
                    {
                        self.engine
                            .send(EngineCmd::UpdateConfig(self.draft_cfg.clone()));
                        self.settings_status = Some((
                            "Applied live to running governor".to_string(),
                            STATUS_GREEN,
                        ));
                    }

                    if ui
                        .add_enabled(valid, egui::Button::new("Save to config.toml"))
                        .clicked()
                    {
                        if let Some(path) = config::default_config_path() {
                            match config::save_config(&path, &self.draft_cfg) {
                                Ok(()) => {
                                    self.engine
                                        .send(EngineCmd::UpdateConfig(self.draft_cfg.clone()));
                                    self.settings_status = Some((
                                        format!("Saved to {}", path.display()),
                                        STATUS_GREEN,
                                    ));
                                }
                                Err(e) => {
                                    self.settings_status = Some((e, STATUS_RED));
                                }
                            }
                        } else {
                            self.settings_status = Some((
                                "Cannot determine default config path".to_string(),
                                STATUS_RED,
                            ));
                        }
                    }

                    if ui.button("Sync from Running").clicked() {
                        self.draft_cfg = snap.config.clone();
                        self.settings_status =
                            Some(("Synced from active engine".to_string(), ACCENT_CYAN));
                    }

                    if ui.button("Reset Defaults").clicked() {
                        self.draft_cfg = Config::default();
                        self.settings_status =
                            Some(("Reset draft to defaults (click Apply/Save)".to_string(), STATUS_AMBER));
                    }

                    if let Some((ref msg, col)) = self.settings_status {
                        ui.label(RichText::new(msg).size(12.0).color(col));
                    }
                });
            });
        });
    }
}

fn render_row_state_pill(ui: &mut egui::Ui, row: &ProcessRow) {
    if row.is_emergency_paused {
        pill_badge(ui, "MEM PAUSED", STATUS_CRIMSON);
    } else if row.sample.is_stopped {
        pill_badge(ui, "STOPPED", STATUS_RED);
    } else if row.is_demoted {
        pill_badge(ui, "BG QoS", STATUS_AMBER);
    } else if row.is_protected {
        pill_badge(ui, "PROTECTED", ACCENT_CYAN);
    }
}

fn event_kind_color(kind: EventKind) -> Color32 {
    match kind {
        EventKind::Emergency => STATUS_RED,
        EventKind::Intervention => STATUS_AMBER,
        EventKind::Restore => STATUS_GREEN,
        EventKind::DryRun => ACCENT_CYAN,
        EventKind::Manual => ACCENT_PURPLE,
        EventKind::System => TEXT_SECONDARY,
    }
}
