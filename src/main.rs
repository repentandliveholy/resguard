use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use resguard::config::{self, CliAction, ColorMode, Config};
use resguard::governor::{Governor, Zone};
use resguard::model::{self, MemoryPressure};
use resguard::platform::{create_backend, PlatformBackend};
use resguard::{service, updater};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
mod sys_rt {
    use super::STOP_REQUESTED;
    use std::ffi::{c_int, c_long};
    use std::sync::atomic::Ordering;

    const SIGINT: c_int = 2;
    const SIGHUP: c_int = 1;
    const SIGTERM: c_int = 15;

    #[repr(C)]
    struct Ctm {
        tm_sec: c_int,
        tm_min: c_int,
        tm_hour: c_int,
        _rest: [u8; 44],
    }

    impl Default for Ctm {
        fn default() -> Self {
            Self {
                tm_sec: 0,
                tm_min: 0,
                tm_hour: 0,
                _rest: [0u8; 44],
            }
        }
    }

    extern "C" {
        fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
        fn time(tloc: *mut c_long) -> c_long;
        fn localtime_r(timep: *const c_long, result: *mut Ctm) -> *mut Ctm;
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

    pub fn local_hms() -> String {
        unsafe {
            let mut now: c_long = 0;
            time(&mut now);
            let mut tm = Ctm::default();
            if localtime_r(&now, &mut tm).is_null() {
                return "00:00:00".to_string();
            }
            format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
        }
    }

    pub fn enable_ansi_terminal() {}
}

#[cfg(windows)]
mod sys_rt {
    use super::STOP_REQUESTED;
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;

    type Handle = *mut c_void;
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        w_year: u16,
        w_month: u16,
        w_day_of_week: u16,
        w_day: u16,
        w_hour: u16,
        w_minute: u16,
        w_second: u16,
        w_milliseconds: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(
            handler: extern "system" fn(u32) -> i32,
            add: i32,
        ) -> i32;
        fn GetLocalTime(lp_system_time: *mut SystemTime);
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

    pub fn local_hms() -> String {
        let mut st = SystemTime::default();
        unsafe { GetLocalTime(&mut st) };
        format!("{:02}:{:02}:{:02}", st.w_hour, st.w_minute, st.w_second)
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
    sys_rt::local_hms()
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

fn format_top_procs(procs: &[model::ProcessSample], limit: usize) -> String {
    let mut sorted: Vec<&model::ProcessSample> = procs.iter().collect();
    sorted.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sorted
        .into_iter()
        .take(limit)
        .filter(|p| p.cpu_pct >= 0.5)
        .map(|p| format!("{}:{:.1}%({:.0}M)", p.name, p.cpu_pct, p.rss_mb()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn zone_color(zone: Zone) -> &'static str {
    match zone {
        Zone::Normal => "1;32",           // bold green
        Zone::Soft => "1;33",             // bold yellow
        Zone::High => "1;35",             // bold magenta
        Zone::Cap => "1;31",              // bold red
        Zone::MemoryEmergency => "1;37;41", // bold white on red
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

fn event_color(ev: &str) -> &'static str {
    if ev.contains("MEM/SSD guard paused") {
        "1;31"
    } else if ev.contains("Demoted") || ev.contains("duty-cycle") {
        "33"
    } else if ev.contains("Restored") || ev.contains("resumed") {
        "32"
    } else if ev.contains("[DRY-RUN]") {
        "36"
    } else {
        "0"
    }
}

fn run(cfg: Config) -> Result<(), String> {
    install_signal_handlers();

    let mut backend = create_backend()?;
    let mut log = Logger::new(&cfg)?;
    let self_pid = process::id();
    let mut gov = Governor::new(self_pid);

    let banner = format!(
        "resguard v{} | backend={} | cores={} | dry_run={} | CPU soft={:.0}% high={:.0}% cap={:.0}% | RAM high={:.0}% crit={:.0}% | SSD min={:.1}GB",
        env!("CARGO_PKG_VERSION"),
        backend.backend_name(),
        backend.num_cores(),
        cfg.dry_run,
        cfg.cpu_soft,
        cfg.cpu_high,
        cfg.cpu_cap,
        cfg.ram_high_pct,
        cfg.ram_critical_pct,
        cfg.disk_min_free_gb,
    );
    log.emit(&banner, &log.paint("1;36", &banner));

    // Prime process CPU tick baseline before first real measurement
    let _ = backend.sample_processes(0, backend.num_cores());
    let mut last_sample_time = Instant::now();
    sleep_interruptible(Duration::from_millis(cfg.tick_ms));

    // Optional background auto-updater (non-blocking thread)
    let auto_update_msg: std::sync::Arc<std::sync::Mutex<Option<(String, bool)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    if cfg.auto_update && !cfg.once && !cfg.dry_run {
        let slot = std::sync::Arc::clone(&auto_update_msg);
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
                        if let Ok(mut g) = slot.lock() {
                            *g = Some((
                                format!("Auto-updated resguard {} -> {}", from, to),
                                running_under_service,
                            ));
                        }
                        if running_under_service {
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
    }

    let mut last_status_print = Instant::now() - Duration::from_secs(10);
    let mut prev_zone = Zone::Normal;

    while !STOP_REQUESTED.load(Ordering::Relaxed) {
        if let Ok(mut g) = auto_update_msg.lock() {
            if let Some((msg, restart_pending)) = g.take() {
                let full = if restart_pending {
                    format!("{} | {} (restarting service into new binary)", local_hms(), msg)
                } else {
                    format!("{} | {} (restart resguard to run new binary)", local_hms(), msg)
                };
                log.emit(&full, &log.paint("1;32", &full));
            }
        }
        let now = Instant::now();
        let elapsed_ns = now.duration_since(last_sample_time).as_nanos() as u64;
        last_sample_time = now;

        let metrics = backend.sample_system()?;
        let procs = backend.sample_processes(elapsed_ns.max(1), metrics.num_cores);

        let action = gov.evaluate_tick(now, &cfg, &metrics, &procs, backend.as_ref());
        let ts = local_hms();

        for ev in &action.events {
            let plain = format!("{} | {}", ts, ev);
            let colored = format!(
                "{} | {}",
                log.paint("90", &ts),
                log.paint(event_color(ev), ev)
            );
            log.emit(&plain, &colored);
        }

        let zone_changed = action.zone != prev_zone;
        let print_periodic =
            !cfg.quiet && now.duration_since(last_status_print) >= Duration::from_secs(2);
        if cfg.once || zone_changed || print_periodic {
            last_status_print = now;
            let top = format_top_procs(&procs, 3);
            let top_str = if top.is_empty() { "-" } else { &top };
            let zone_txt = format!("{:>13}", action.zone.to_string());
            let cpu_txt = format!("{:5.1}%", metrics.cpu_total_pct);
            let ram_txt = format!("{:5.1}%", metrics.ram_used_pct);
            let mp_txt = format!("{}", metrics.memory_pressure);
            let swap_txt = format!(
                "{:5.0}/{:.0}M",
                metrics.swap_used_mb(),
                metrics.swap_total_mb()
            );
            let ssd_txt = format!("{:5.1}G", metrics.disk_free_gb());
            let load_txt = format!("{:.1}", metrics.load_1m);
            let gpu_plain = metrics
                .gpu_pct
                .map(|g| format!(" | GPU {:3.0}%", g))
                .unwrap_or_default();
            let gpu_colored = metrics
                .gpu_pct
                .map(|g| {
                    format!(
                        " | {} {}",
                        log.paint("1", "GPU"),
                        log.paint(pct_color(g, cfg.cpu_soft, cfg.cpu_high), &format!("{:3.0}%", g))
                    )
                })
                .unwrap_or_default();

            let plain = format!(
                "{} [{}] CPU {} (u:{:4.1}% s:{:4.1}%) load {}{} | RAM {} ({:.1}/{:.1}G {}) | Swap {} | SSD {} | bg={} paused={} duty={:.0}% | top: {}",
                ts,
                zone_txt,
                cpu_txt,
                metrics.cpu_user_pct,
                metrics.cpu_sys_pct,
                load_txt,
                gpu_plain,
                ram_txt,
                metrics.ram_used_gb(),
                metrics.ram_total_gb(),
                mp_txt,
                swap_txt,
                ssd_txt,
                gov.demoted_count(),
                gov.emergency_paused_count(),
                action.throttle_ratio * 100.0,
                top_str
            );

            let ssd_col = if metrics.disk_free_gb() < cfg.disk_min_free_gb {
                "1;31"
            } else if metrics.disk_free_gb() < cfg.disk_min_free_gb * 2.0 {
                "1;33"
            } else {
                "32"
            };
            let swap_col = if metrics.swap_used_mb() > cfg.swap_warn_mb {
                "1;33"
            } else {
                "36"
            };

            let colored = format!(
                "{} [{}] {} {} {} {} {}{} | {} {} ({:.1}/{:.1}G {}) | {} {} | {} {} | bg={} paused={} duty={:.0}% | {} {}",
                log.paint("90", &ts),
                log.paint(zone_color(action.zone), &zone_txt),
                log.paint("1", "CPU"),
                log.paint(pct_color(metrics.cpu_total_pct, cfg.cpu_soft, cfg.cpu_high), &cpu_txt),
                log.paint("90", &format!("(u:{:4.1}% s:{:4.1}%)", metrics.cpu_user_pct, metrics.cpu_sys_pct)),
                log.paint("90", "load"),
                log.paint(pct_color(metrics.load_1m, metrics.num_cores as f64, (metrics.num_cores as f64) * 3.0), &load_txt),
                gpu_colored,
                log.paint("1", "RAM"),
                log.paint(pct_color(metrics.ram_used_pct, cfg.ram_high_pct, cfg.ram_critical_pct), &ram_txt),
                metrics.ram_used_gb(),
                metrics.ram_total_gb(),
                log.paint(pressure_color(metrics.memory_pressure), &mp_txt),
                log.paint("1", "Swap"),
                log.paint(swap_col, &swap_txt),
                log.paint("1", "SSD"),
                log.paint(ssd_col, &ssd_txt),
                gov.demoted_count(),
                gov.emergency_paused_count(),
                action.throttle_ratio * 100.0,
                log.paint("90", "top:"),
                log.paint("37", top_str),
            );
            log.emit(&plain, &colored);
        }
        prev_zone = action.zone;

        if cfg.once {
            break;
        }

        if !cfg.dry_run
            && !action.duty_pause_pids.is_empty()
            && action.pause_duration > Duration::ZERO
        {
            let mut active_paused = Vec::with_capacity(action.duty_pause_pids.len());
            for &pid in &action.duty_pause_pids {
                if backend.pause_process(pid).is_ok() {
                    active_paused.push(pid);
                }
            }
            sleep_interruptible(action.pause_duration);
            for pid in active_paused {
                let _ = backend.resume_process(pid);
            }
            if !STOP_REQUESTED.load(Ordering::Relaxed) && action.run_duration > Duration::ZERO {
                sleep_interruptible(action.run_duration);
            }
        } else {
            sleep_interruptible(Duration::from_millis(cfg.tick_ms));
        }
    }

    cleanup_on_exit(&mut gov, &cfg, backend.as_ref(), &mut log);
    Ok(())
}

fn cleanup_on_exit(
    gov: &mut Governor,
    cfg: &Config,
    backend: &dyn PlatformBackend,
    log: &mut Logger,
) {
    let demoted = gov.demoted_count();
    let paused = gov.emergency_paused_count();
    gov.cleanup(cfg, backend);
    if demoted > 0 || paused > 0 {
        let msg = format!(
            "{} | resguard exiting cleanly: restored {} demoted and {} paused process(es)",
            local_hms(),
            demoted,
            paused
        );
        log.emit(&msg, &log.paint("32", &msg));
    }
}

fn main() {
    match config::parse_args_and_load() {
        Ok(CliAction::ExitOk) => {}
        Ok(CliAction::Run(cfg)) => {
            if let Err(e) = run(cfg) {
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
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(2);
        }
    }
}
