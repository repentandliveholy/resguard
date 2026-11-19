use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use resguard::config::{self, CliAction, ColorMode, Config};
use resguard::governor::{EventKind, Zone};
use resguard::model::MemoryPressure;
use resguard::runtime::{self, Authority, Mode, Role, Runtime, Snapshot};
use resguard::{service, updater};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
mod sys_rt {
    use super::STOP_REQUESTED;
    use std::ffi::c_int;
    use std::sync::atomic::Ordering;

    const SIGINT: c_int = 2;
    const SIGHUP: c_int = 1;
    const SIGTERM: c_int = 15;

    extern "C" {
        fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
    }

    extern "C" fn handle_exit_signal(_sig: c_int) {
        STOP_REQUESTED.store(true, Ordering::Relaxed);
    }

    pub fn install_signal_handlers() {
        unsafe {
            signal(SIGINT, handle_exit_signal);
            signal(SIGTERM, handle_exit_signal);
            signal(SIGHUP, handle_exit_signal);
        }
    }

    pub fn enable_ansi_terminal() {}

    pub fn detach_console() {}
}

#[cfg(windows)]
mod sys_rt {
    use super::STOP_REQUESTED;
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;

    type Handle = *mut c_void;
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(
            handler: extern "system" fn(u32) -> i32,
            add: i32,
        ) -> i32;
        fn FreeConsole() -> i32;
        fn GetStdHandle(n_std_handle: u32) -> Handle;
        fn GetConsoleMode(h_console_handle: Handle, lp_mode: *mut u32) -> i32;
        fn SetConsoleMode(h_console_handle: Handle, dw_mode: u32) -> i32;
    }

    extern "system" fn ctrl_handler(_ctrl_type: u32) -> i32 {
        STOP_REQUESTED.store(true, Ordering::Relaxed);
        1
    }

    pub fn install_signal_handlers() {
        unsafe {
            SetConsoleCtrlHandler(ctrl_handler, 1);
        }
    }

    /// Background service: drop the console window so nothing stays on screen.
    pub fn detach_console() {
        unsafe {
            FreeConsole();
        }
    }

    pub fn enable_ansi_terminal() {
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            if !h.is_null() {
                let mut mode: u32 = 0;
                if GetConsoleMode(h, &mut mode) != 0 {
                    let _ = SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
                }
            }
        }
    }
}

fn install_signal_handlers() {
    sys_rt::install_signal_handlers();
}

fn should_use_color(mode: ColorMode) -> bool {
    let use_it = match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
        }
    };
    if use_it {
        sys_rt::enable_ansi_terminal();
    }
    use_it
}

fn local_hms() -> String {
    runtime::local_hms()
}

struct Logger {
    use_color: bool,
    file: Option<File>,
}

impl Logger {
    fn new(cfg: &Config) -> Result<Self, String> {
        let file = if let Some(ref path) = cfg.log_file {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("Failed to open log file {}: {}", path, e))?;
            Some(f)
        } else {
            None
        };
        Ok(Self {
            use_color: should_use_color(cfg.color),
            file,
        })
    }

    fn emit(&mut self, plain: &str, colored: &str) {
        if self.use_color {
            println!("{}", colored);
        } else {
            println!("{}", plain);
        }
        if let Some(ref mut f) = self.file {
            let _ = writeln!(f, "{}", plain);
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.use_color {
            format!("\x1b[{}m{}\x1b[0m", code, text)
        } else {
            text.to_string()
        }
    }
}

fn sleep_interruptible(dur: Duration) {
    let start = Instant::now();
    while !STOP_REQUESTED.load(Ordering::Relaxed) {
        let elapsed = start.elapsed();
        if elapsed >= dur {
            break;
        }
        let rem = dur - elapsed;
        thread::sleep(rem.min(Duration::from_millis(50)));
    }
}

fn zone_color(zone: Zone) -> &'static str {
    match zone {
        Zone::Normal => "1;32",
        Zone::Soft => "1;33",
        Zone::High => "1;35",
        Zone::MemoryEmergency => "1;37;41",
    }
}

fn pct_color(val: f64, warn: f64, crit: f64) -> &'static str {
    if val >= crit {
        "1;31"
    } else if val >= warn {
        "1;33"
    } else {
        "32"
    }
}

fn pressure_color(p: MemoryPressure) -> &'static str {
    match p {
        MemoryPressure::Normal => "32",
        MemoryPressure::Warning => "1;33",
        MemoryPressure::Critical => "1;31",
    }
}

fn event_color(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Action => "33",
        EventKind::Restore => "32",
        EventKind::Alert => "1;31",
        EventKind::DryRun => "36",
        EventKind::Manual => "35",
        EventKind::Error => "31",
        EventKind::System => "0",
    }
}

fn format_top_procs(snap: &Snapshot, limit: usize) -> String {
    let mut sorted: Vec<_> = snap.procs.iter().filter(|r| r.sample.cpu_pct >= 0.5).collect();
    sorted.sort_by(|a, b| {
        b.sample
            .cpu_pct
            .partial_cmp(&a.sample.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sorted
        .into_iter()
        .take(limit)
        .map(|r| {
            let tag = if r.demoted {
                "↓"
            } else if r.paused {
                "‖"
            } else {
                ""
            };
            format!("{}{}:{:.1}%({:.0}M)", tag, r.sample.name, r.sample.cpu_pct, r.sample.rss_mb())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn run(cfg: Config, args: Vec<String>) -> Result<(), String> {
    install_signal_handlers();
    if cfg.service {
        sys_rt::detach_console();
    }

    let mut log = Logger::new(&cfg)?;
    let mode = if cfg.dry_run { Mode::DryRun } else { Mode::Active };
    let mut rt = Runtime::new(cfg.clone(), mode, Role::Daemon)?;
    if let Ok(Some(path)) = config::config_path_from_args(&args) {
        let reload_args = args.clone();
        rt.watch_config(path, move || config::build_run_config(&reload_args).map(|(c, _)| c));
    }

    let banner = format!(
        "resguard v{} | backend={} | cores={} | mode={} | soft={:.0}% high={:.0}% | RAM high={:.0}% crit={:.0}% | SSD min={:.1}GB | emergency_pause={}",
        env!("CARGO_PKG_VERSION"),
        rt.backend_name(),
        rt.num_cores(),
        mode.label(),
        cfg.cpu_soft,
        cfg.cpu_high,
        cfg.ram_high_pct,
        cfg.ram_critical_pct,
        cfg.disk_min_free_gb,
        cfg.emergency_pause,
    );
    log.emit(&banner, &log.paint("1;36", &banner));

    // First tick only primes CPU deltas.
    sleep_interruptible(Duration::from_millis(cfg.tick_ms));

    let auto_update_msg = spawn_auto_updater(&cfg);
    let mut last_status_print = Instant::now() - Duration::from_secs(10);
    let mut prev_zone = Zone::Normal;
    let mut prev_authority: Option<Authority> = None;

    while !STOP_REQUESTED.load(Ordering::Relaxed) {
        if let Some(slot) = &auto_update_msg {
            if let Ok(mut g) = slot.lock() {
                if let Some((msg, restart_pending)) = g.take() {
                    let full = if restart_pending {
                        format!("{} | {} (restarting service into new binary)", local_hms(), msg)
                    } else {
                        format!("{} | {} (restart resguard to run new binary)", local_hms(), msg)
                    };
                    log.emit(&full, &log.paint("1;32", &full));
                }
            }
        }

        let tick_started = Instant::now();
        let snap = rt.tick();

        for ev in &snap.events {
            let plain = format!("{} | {}", ev.ts, ev.msg);
            let colored = format!("{} | {}", log.paint("90", &ev.ts), log.paint(event_color(ev.kind), &ev.msg));
            log.emit(&plain, &colored);
        }

        if prev_authority.as_ref() != Some(&snap.authority) {
            if let Authority::Deferred { pid, role } = &snap.authority {
                let msg = format!("{} | Standing by: resguard {} (PID {}) is governing", local_hms(), role, pid);
                log.emit(&msg, &log.paint("36", &msg));
            }
            prev_authority = Some(snap.authority.clone());
        }

        let zone_changed = snap.zone != prev_zone;
        let print_periodic = !cfg.quiet && last_status_print.elapsed() >= Duration::from_secs(2);
        if let Some(m) = &snap.metrics {
            if cfg.once || zone_changed || print_periodic {
                last_status_print = Instant::now();
                print_status(&mut log, &cfg, &snap, m);
            }
        }
        prev_zone = snap.zone;

        if cfg.once {
            break;
        }
        let tick = Duration::from_millis(rt.config().tick_ms);
        sleep_interruptible(tick.saturating_sub(tick_started.elapsed()).max(Duration::from_millis(50)));
    }

    for ev in rt.shutdown() {
        let plain = format!("{} | {}", ev.ts, ev.msg);
        log.emit(&plain, &log.paint(event_color(ev.kind), &plain));
    }
    Ok(())
}

fn print_status(log: &mut Logger, cfg: &Config, snap: &Snapshot, m: &resguard::model::SystemMetrics) {
    let ts = local_hms();
    let top = format_top_procs(snap, 3);
    let top_str = if top.is_empty() { "-".to_string() } else { top };
    let zone_txt = format!("{:>9}", snap.zone.label());
    let cpu_txt = format!("{:5.1}%", m.cpu_total_pct);
    let ram_txt = format!("{:5.1}%", m.ram_used_pct);
    let gpu_txt = m.gpu_pct.map(|g| format!(" | GPU {:3.0}%", g)).unwrap_or_default();
    let attrib = format!(
        "yours {:.0}% (governable {:.0}%) other {:.0}%",
        snap.own_cpu, snap.governable_cpu, snap.other_cpu
    );
    let plain = format!(
        "{} [{}] CPU {} (u:{:4.1}% s:{:4.1}%) {} load {:.1}{} | RAM {} ({:.1}/{:.1}G {}) | Swap {:5.0}/{:.0}M | SSD {:5.1}G | slowed={} paused={} | top: {}",
        ts, zone_txt, cpu_txt, m.cpu_user_pct, m.cpu_sys_pct, attrib, m.load_1m, gpu_txt,
        ram_txt, m.ram_used_gb(), m.ram_total_gb(), m.memory_pressure,
        m.swap_used_mb(), m.swap_total_mb(), m.disk_free_gb(),
        snap.demoted.len(), snap.paused.len(), top_str
    );
    let ssd_col = if m.disk_free_gb() < cfg.disk_min_free_gb {
        "1;31"
    } else if m.disk_free_gb() < cfg.disk_min_free_gb * 2.0 {
        "1;33"
    } else {
        "32"
    };
    let colored = format!(
        "{} [{}] {} {} {} {} {} {}{} | {} {} ({:.1}/{:.1}G {}) | Swap {:5.0}/{:.0}M | SSD {} | slowed={} paused={} | {} {}",
        log.paint("90", &ts),
        log.paint(zone_color(snap.zone), &zone_txt),
        log.paint("1", "CPU"),
        log.paint(pct_color(m.cpu_total_pct, cfg.cpu_soft, cfg.cpu_high), &cpu_txt),
        log.paint("90", &format!("(u:{:4.1}% s:{:4.1}%)", m.cpu_user_pct, m.cpu_sys_pct)),
        log.paint("90", &attrib),
        log.paint("90", "load"),
        log.paint(pct_color(m.load_1m, m.num_cores as f64, (m.num_cores as f64) * 3.0), &format!("{:.1}", m.load_1m)),
        gpu_txt,
        log.paint("1", "RAM"),
        log.paint(pct_color(m.ram_used_pct, cfg.ram_high_pct, cfg.ram_critical_pct), &ram_txt),
        m.ram_used_gb(),
        m.ram_total_gb(),
        log.paint(pressure_color(m.memory_pressure), &m.memory_pressure.to_string()),
        m.swap_used_mb(),
        m.swap_total_mb(),
        log.paint(ssd_col, &format!("{:5.1}G", m.disk_free_gb())),
        snap.demoted.len(),
        snap.paused.len(),
        log.paint("90", "top:"),
        log.paint("37", &top_str),
    );
    log.emit(&plain, &colored);
}

type UpdateSlot = std::sync::Arc<std::sync::Mutex<Option<(String, bool)>>>;

fn spawn_auto_updater(cfg: &Config) -> Option<UpdateSlot> {
    if !cfg.auto_update || cfg.once || cfg.dry_run {
        return None;
    }
    let slot: UpdateSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot_bg = std::sync::Arc::clone(&slot);
    let interval = Duration::from_secs(cfg.auto_update_interval_hours.max(1) * 3600);
    let _ = thread::Builder::new()
        .name("resguard-autoupdate".to_string())
        .spawn(move || {
            // Initial grace delay so startup never races
            for _ in 0..60 {
                if STOP_REQUESTED.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_secs(1));
            }
            loop {
                if STOP_REQUESTED.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(updater::UpdateOutcome::Updated { from, to }) =
                    updater::perform_self_update(false)
                {
                    let running_under_service = std::env::var_os("INVOCATION_ID").is_some()
                        || std::env::var_os("XPC_SERVICE_NAME").is_some();
                    if let Ok(mut g) = slot_bg.lock() {
                        *g = Some((format!("Auto-updated resguard {} -> {}", from, to), running_under_service));
                    }
                    if running_under_service {
                        // Exit cleanly (restoring everything); the service manager restarts us.
                        STOP_REQUESTED.store(true, Ordering::Relaxed);
                    }
                    return;
                }
                let mut slept = Duration::ZERO;
                while slept < interval {
                    if STOP_REQUESTED.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_secs(5));
                    slept += Duration::from_secs(5);
                }
            }
        });
    Some(slot)
}

fn main() {
    match config::parse_args_and_load() {
        Ok(CliAction::ExitOk) => {}
        Ok(CliAction::Run(cfg)) => {
            let args: Vec<String> = std::env::args().skip(1).collect();
            if let Err(e) = run(cfg, args) {
                eprintln!("resguard error: {}", e);
                process::exit(1);
            }
        }
        Ok(CliAction::RunTui(cfg)) => {
            #[cfg(feature = "tui")]
            {
                if let Err(e) = resguard::tui::run_tui(cfg) {
                    eprintln!("resguard TUI error: {}", e);
                    process::exit(1);
                }
            }
            #[cfg(not(feature = "tui"))]
            {
                let _ = cfg;
                eprintln!("resguard was built without the 'tui' feature.");
                process::exit(2);
            }
        }
        Ok(CliAction::CheckUpdate) => match updater::check_latest_version() {
            Ok(info) => {
                if info.update_available {
                    println!(
                        "Update available: {} -> {} (run `resguard --update`)",
                        info.current_version, info.latest_version
                    );
                } else {
                    println!("resguard {} is up to date.", info.current_version);
                }
            }
            Err(e) => {
                eprintln!("Update check failed: {}", e);
                process::exit(1);
            }
        },
        Ok(CliAction::SelfUpdate { force }) => match updater::perform_self_update(force) {
            Ok(updater::UpdateOutcome::UpToDate(v)) => {
                println!("resguard {} is already up to date.", v);
            }
            Ok(updater::UpdateOutcome::Updated { from, to }) => {
                println!("Updated resguard {} -> {}", from, to);
            }
            Err(e) => {
                eprintln!("Self-update failed: {}", e);
                process::exit(1);
            }
        },
        Ok(CliAction::InstallService) => match service::install_service() {
            Ok(msg) => println!("{}", msg),
            Err(e) => {
                eprintln!("Service install failed: {}", e);
                process::exit(1);
            }
        },
        Ok(CliAction::UninstallService) => match service::uninstall_service() {
            Ok(msg) => println!("{}", msg),
            Err(e) => {
                eprintln!("Service uninstall failed: {}", e);
                process::exit(1);
            }
        },
        Ok(CliAction::ServiceStatus) => match service::service_status() {
            Ok(msg) => println!("{}", msg),
            Err(e) => {
                eprintln!("Service status failed: {}", e);
                process::exit(1);
            }
        },
        Ok(CliAction::Restore) => match runtime::restore_cli() {
            Ok(msg) => println!("{}", msg),
            Err(e) => {
                eprintln!("Restore failed: {}", e);
                process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(2);
        }
    }
}
