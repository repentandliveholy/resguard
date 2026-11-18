use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use resguard::config::{self, Config};
use resguard::governor::{build_protected_pids, Governor, TickAction, Zone};
use resguard::model::{MemoryPressure, ProcessSample, SystemMetrics};
use resguard::platform::{create_backend, PlatformBackend};

pub const HISTORY_CAP: usize = 180;
pub const EVENTS_CAP: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    /// Observes telemetry & simulates governor actions (dry-run) without modifying processes.
    Monitor,
    /// Actively enforces QoS demotion, duty-cycle throttling, and MEM/SSD emergency pauses.
    Active,
    /// Telemetry sampling only; governor is paused.
    TelemetryOnly,
}

impl GuardMode {
    pub fn label(self) -> &'static str {
        match self {
            GuardMode::Monitor => "Monitor (Dry-Run)",
            GuardMode::Active => "Active Guard",
            GuardMode::TelemetryOnly => "Telemetry Only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Intervention,
    Restore,
    Emergency,
    DryRun,
    Manual,
    System,
}

#[derive(Debug, Clone)]
pub struct EventEntry {
    pub timestamp: String,
    pub kind: EventKind,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct HistorySample {
    pub timestamp: String,
    pub cpu_total: f64,
    pub cpu_user: f64,
    pub cpu_sys: f64,
    pub gpu: Option<f64>,
    pub ram_pct: f64,
    pub swap_pct: f64,
    pub throttle_pct: f64,
}

#[derive(Debug, Clone)]
pub struct ProcessRow {
    pub sample: ProcessSample,
    pub is_protected: bool,
    pub is_demoted: bool,
    pub is_emergency_paused: bool,
}

#[derive(Debug, Clone)]
pub struct ExternalDaemon {
    pub pid: u32,
    pub exe_path: String,
}

#[derive(Debug, Clone)]
pub struct DashSnapshot {
    pub backend_name: String,
    pub num_cores: usize,
    pub guard_mode: GuardMode,
    pub config: Config,
    pub metrics: SystemMetrics,
    pub zone: Zone,
    pub throttle_ratio: f64,
    pub demoted: Vec<(u32, String)>,
    pub emergency_paused: Vec<(u32, String)>,
    pub protected_count: usize,
    pub external_daemon: Option<ExternalDaemon>,
    pub processes: Vec<ProcessRow>,
    pub history: VecDeque<HistorySample>,
    pub events: VecDeque<EventEntry>,
    pub last_tick_ts: String,
    pub last_error: Option<String>,
}

impl DashSnapshot {
    fn placeholder(cfg: Config) -> Self {
        Self {
            backend_name: "initializing...".to_string(),
            num_cores: 1,
            guard_mode: GuardMode::Monitor,
            config: cfg,
            metrics: SystemMetrics {
                cpu_total_pct: 0.0,
                cpu_user_pct: 0.0,
                cpu_sys_pct: 0.0,
                gpu_pct: None,
                load_1m: 0.0,
                ram_total_bytes: 0,
                ram_used_bytes: 0,
                ram_used_pct: 0.0,
                memory_pressure: MemoryPressure::Normal,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
                disk_free_bytes: 0,
                num_cores: 1,
            },
            zone: Zone::Normal,
            throttle_ratio: 0.0,
            demoted: Vec::new(),
            emergency_paused: Vec::new(),
            protected_count: 0,
            external_daemon: None,
            processes: Vec::new(),
            history: VecDeque::with_capacity(HISTORY_CAP),
            events: VecDeque::with_capacity(EVENTS_CAP),
            last_tick_ts: "--:--:--".to_string(),
            last_error: None,
        }
    }
}

pub enum EngineCmd {
    SetMode(GuardMode),
    UpdateConfig(Config),
    ToggleProtectName(String),
    ManualSetBg { pid: u32, enable: bool },
    ManualPause { pid: u32 },
    ManualResume { pid: u32 },
    ClearEvents,
    Shutdown,
}

pub struct EngineHandle {
    pub state: Arc<Mutex<DashSnapshot>>,
    cmd_tx: Sender<EngineCmd>,
    worker: Option<JoinHandle<()>>,
}

impl EngineHandle {
    pub fn start(ctx: egui::Context, initial_cfg: Config, start_active: bool) -> Self {
        let state = Arc::new(Mutex::new(DashSnapshot::placeholder(initial_cfg.clone())));
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let state_bg = Arc::clone(&state);

        let worker = thread::Builder::new()
            .name("resguard-engine".to_string())
            .spawn(move || {
                run_engine(ctx, state_bg, cmd_rx, initial_cfg, start_active);
            })
            .expect("failed to spawn resguard-engine thread");

        Self {
            state,
            cmd_tx,
            worker: Some(worker),
        }
    }

    pub fn send(&self, cmd: EngineCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn snapshot(&self) -> DashSnapshot {
        self.state.lock().unwrap().clone()
    }

    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(EngineCmd::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_engine(
    ctx: egui::Context,
    state: Arc<Mutex<DashSnapshot>>,
    cmd_rx: Receiver<EngineCmd>,
    mut cfg: Config,
    start_active: bool,
) {
    let mut backend = match create_backend() {
        Ok(b) => b,
        Err(e) => {
            let mut s = state.lock().unwrap();
            s.last_error = Some(format!("Backend init failed: {}", e));
            ctx.request_repaint();
            return;
        }
    };

    let self_pid = process::id();
    let mut gov = Governor::new(self_pid);
    let mut mode = if start_active {
        GuardMode::Active
    } else {
        GuardMode::Monitor
    };

    {
        let mut s = state.lock().unwrap();
        s.backend_name = backend.backend_name().to_string();
        s.num_cores = backend.num_cores();
        s.guard_mode = mode;
        push_event(
            &mut s.events,
            EventKind::System,
            format!(
                "RS Dash started (backend={}, cores={})",
                backend.backend_name(),
                backend.num_cores()
            ),
        );
    }

    // Prime CPU process delta baseline
    let prime_procs = backend.sample_processes(0, backend.num_cores());
    let ext = detect_external_daemon(self_pid, &prime_procs);
    if let Some(ref d) = ext {
        if mode == GuardMode::Active {
            mode = GuardMode::Monitor;
        }
        let mut s = state.lock().unwrap();
        s.external_daemon = Some(d.clone());
        s.guard_mode = mode;
        push_event(
            &mut s.events,
            EventKind::System,
            format!(
                "Detected running resguard daemon (PID {}) — using Monitor (Dry-Run) to avoid conflict",
                d.pid
            ),
        );
    }

    let mut last_sample = Instant::now();
    let mut manual_bg_pids: HashSet<u32> = HashSet::new();
    let mut latest_protected: HashSet<u32> = HashSet::new();

    if !sleep_or_cmd(
        Duration::from_millis(cfg.tick_ms.min(250)),
        &cmd_rx,
        &mut |cmd| {
            handle_cmd(
                cmd,
                &mut mode,
                &mut cfg,
                &mut gov,
                backend.as_ref(),
                &state,
                &latest_protected,
                &mut manual_bg_pids,
            )
        },
    ) {
        gov.cleanup(&cfg, backend.as_ref());
        return;
    }

    loop {
        // Drain pending UI commands before tick
        let mut stop = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if !handle_cmd(
                cmd,
                &mut mode,
                &mut cfg,
                &mut gov,
                backend.as_ref(),
                &state,
                &latest_protected,
                &mut manual_bg_pids,
            ) {
                stop = true;
                break;
            }
        }
        if stop {
            break;
        }

        let now = Instant::now();
        let elapsed_ns = now.duration_since(last_sample).as_nanos() as u64;
        last_sample = now;

        let metrics_res = backend.sample_system();
        let procs = backend.sample_processes(elapsed_ns.max(1), backend.num_cores());
        let ext_daemon = detect_external_daemon(self_pid, &procs);

        let live_pids: HashSet<u32> = procs.iter().map(|p| p.pid).collect();
        manual_bg_pids.retain(|pid| live_pids.contains(pid));

        let protected = build_protected_pids(self_pid, backend.current_uid(), &procs, &cfg);
        latest_protected = protected.clone();

        match metrics_res {
            Ok(metrics) => {
                let mut eval_cfg = cfg.clone();
                eval_cfg.dry_run = mode != GuardMode::Active;

                let action: TickAction = match mode {
                    GuardMode::TelemetryOnly => TickAction {
                        zone: Zone::Normal,
                        throttle_ratio: 0.0,
                        duty_pause_pids: Vec::new(),
                        pause_duration: Duration::ZERO,
                        run_duration: Duration::from_millis(cfg.tick_ms),
                        events: Vec::new(),
                    },
                    GuardMode::Monitor | GuardMode::Active => {
                        gov.evaluate_tick(now, &eval_cfg, &metrics, &procs, backend.as_ref())
                    }
                };

                let ts = local_hms();
                let swap_pct = if metrics.swap_total_bytes > 0 {
                    (metrics.swap_used_bytes as f64 / metrics.swap_total_bytes as f64) * 100.0
                } else {
                    0.0
                };

                let mut rows: Vec<ProcessRow> = procs
                    .into_iter()
                    .map(|p| {
                        let pid = p.pid;
                        ProcessRow {
                            is_protected: protected.contains(&pid),
                            is_demoted: gov.is_demoted(pid) || manual_bg_pids.contains(&pid),
                            is_emergency_paused: gov.is_emergency_paused(pid),
                            sample: p,
                        }
                    })
                    .collect();

                rows.sort_by(|a, b| {
                    b.sample
                        .cpu_pct
                        .partial_cmp(&a.sample.cpu_pct)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.sample.rss_bytes.cmp(&a.sample.rss_bytes))
                });

                {
                    let mut s = state.lock().unwrap();
                    s.guard_mode = mode;
                    s.config = cfg.clone();
                    s.zone = action.zone;
                    s.throttle_ratio = action.throttle_ratio;
                    s.demoted = gov
                        .demoted_pids()
                        .iter()
                        .map(|(&pid, name)| (pid, name.clone()))
                        .collect();
                    s.demoted.sort_by_key(|(pid, _)| *pid);
                    s.emergency_paused = gov
                        .emergency_paused_pids()
                        .iter()
                        .map(|(&pid, name)| (pid, name.clone()))
                        .collect();
                    s.emergency_paused.sort_by_key(|(pid, _)| *pid);
                    s.protected_count = protected.len();
                    s.external_daemon = ext_daemon;
                    s.last_tick_ts = ts.clone();
                    s.last_error = None;

                    for ev in &action.events {
                        let kind = classify_event(ev);
                        push_event(&mut s.events, kind, ev.clone());
                    }

                    if s.history.len() >= HISTORY_CAP {
                        s.history.pop_front();
                    }
                    s.history.push_back(HistorySample {
                        timestamp: ts,
                        cpu_total: metrics.cpu_total_pct,
                        cpu_user: metrics.cpu_user_pct,
                        cpu_sys: metrics.cpu_sys_pct,
                        gpu: metrics.gpu_pct,
                        ram_pct: metrics.ram_used_pct,
                        swap_pct,
                        throttle_pct: action.throttle_ratio * 100.0,
                    });

                    s.metrics = metrics;
                    s.processes = rows;
                }

                ctx.request_repaint();

                if mode == GuardMode::Active
                    && !action.duty_pause_pids.is_empty()
                    && action.pause_duration > Duration::ZERO
                {
                    let mut active_paused = Vec::with_capacity(action.duty_pause_pids.len());
                    for &pid in &action.duty_pause_pids {
                        if backend.pause_process(pid).is_ok() {
                            active_paused.push(pid);
                        }
                    }
                    let keep_going = sleep_or_cmd(action.pause_duration, &cmd_rx, &mut |cmd| {
                        handle_cmd(
                            cmd,
                            &mut mode,
                            &mut cfg,
                            &mut gov,
                            backend.as_ref(),
                            &state,
                            &latest_protected,
                            &mut manual_bg_pids,
                        )
                    });
                    for pid in active_paused {
                        let _ = backend.resume_process(pid);
                    }
                    if !keep_going {
                        break;
                    }
                    if action.run_duration > Duration::ZERO
                        && !sleep_or_cmd(action.run_duration, &cmd_rx, &mut |cmd| {
                            handle_cmd(
                                cmd,
                                &mut mode,
                                &mut cfg,
                                &mut gov,
                                backend.as_ref(),
                                &state,
                                &latest_protected,
                                &mut manual_bg_pids,
                            )
                        })
                    {
                        break;
                    }
                } else if !sleep_or_cmd(
                    Duration::from_millis(cfg.tick_ms),
                    &cmd_rx,
                    &mut |cmd| {
                        handle_cmd(
                            cmd,
                            &mut mode,
                            &mut cfg,
                            &mut gov,
                            backend.as_ref(),
                            &state,
                            &latest_protected,
                            &mut manual_bg_pids,
                        )
                    },
                ) {
                    break;
                }
            }
            Err(e) => {
                {
                    let mut s = state.lock().unwrap();
                    s.last_error = Some(e);
                }
                ctx.request_repaint();
                if !sleep_or_cmd(Duration::from_millis(1000), &cmd_rx, &mut |cmd| {
                    handle_cmd(
                        cmd,
                        &mut mode,
                        &mut cfg,
                        &mut gov,
                        backend.as_ref(),
                        &state,
                        &latest_protected,
                        &mut manual_bg_pids,
                    )
                }) {
                    break;
                }
            }
        }
    }

    // Restore any governed or manually backgrounded processes on clean shutdown
    let mut clean_cfg = cfg.clone();
    clean_cfg.dry_run = false;
    gov.cleanup(&clean_cfg, backend.as_ref());
    for pid in manual_bg_pids.drain() {
        let _ = backend.set_background(pid, false);
    }
}

fn handle_cmd(
    cmd: EngineCmd,
    mode: &mut GuardMode,
    cfg: &mut Config,
    gov: &mut Governor,
    backend: &dyn PlatformBackend,
    state: &Arc<Mutex<DashSnapshot>>,
    protected: &HashSet<u32>,
    manual_bg_pids: &mut HashSet<u32>,
) -> bool {
    match cmd {
        EngineCmd::Shutdown => return false,
        EngineCmd::SetMode(new_mode) => {
            if *mode != new_mode {
                if *mode == GuardMode::Active && new_mode != GuardMode::Active {
                    let mut clean_cfg = cfg.clone();
                    clean_cfg.dry_run = false;
                    gov.cleanup(&clean_cfg, backend);
                } else if new_mode == GuardMode::TelemetryOnly {
                    let mut clean_cfg = cfg.clone();
                    clean_cfg.dry_run = true;
                    gov.cleanup(&clean_cfg, backend);
                }
                *mode = new_mode;
                let mut s = state.lock().unwrap();
                s.guard_mode = new_mode;
                push_event(
                    &mut s.events,
                    EventKind::System,
                    format!("Switched mode to {}", new_mode.label()),
                );
            }
        }
        EngineCmd::UpdateConfig(new_cfg) => {
            if let Err(err) = config::validate_config(&new_cfg) {
                let mut s = state.lock().unwrap();
                s.last_error = Some(err);
            } else {
                *cfg = new_cfg.clone();
                let mut s = state.lock().unwrap();
                s.config = new_cfg;
                s.last_error = None;
                push_event(
                    &mut s.events,
                    EventKind::System,
                    "Updated live governor configuration".to_string(),
                );
            }
        }
        EngineCmd::ToggleProtectName(name) => {
            if let Some(pos) = cfg
                .protect_names
                .iter()
                .position(|p| p.eq_ignore_ascii_case(&name))
            {
                cfg.protect_names.remove(pos);
                let mut s = state.lock().unwrap();
                s.config = cfg.clone();
                push_event(
                    &mut s.events,
                    EventKind::Manual,
                    format!("Removed '{}' from protected list", name),
                );
            } else {
                cfg.protect_names.push(name.clone());
                let mut s = state.lock().unwrap();
                s.config = cfg.clone();
                push_event(
                    &mut s.events,
                    EventKind::Manual,
                    format!("Added '{}' to protected list", name),
                );
            }
        }
        EngineCmd::ManualSetBg { pid, enable } => {
            if enable && protected.contains(&pid) {
                let mut s = state.lock().unwrap();
                push_event(
                    &mut s.events,
                    EventKind::System,
                    format!("Refused to demote protected PID {}", pid),
                );
            } else {
                match backend.set_background(pid, enable) {
                    Ok(()) => {
                        if enable {
                            manual_bg_pids.insert(pid);
                        } else {
                            manual_bg_pids.remove(&pid);
                        }
                        let mut s = state.lock().unwrap();
                        push_event(
                            &mut s.events,
                            EventKind::Manual,
                            format!(
                                "Manual {}: PID {}",
                                if enable { "set background QoS" } else { "restored normal QoS" },
                                pid
                            ),
                        );
                    }
                    Err(e) => {
                        let mut s = state.lock().unwrap();
                        s.last_error = Some(e);
                    }
                }
            }
        }
        EngineCmd::ManualPause { pid } => {
            if protected.contains(&pid) {
                let mut s = state.lock().unwrap();
                push_event(
                    &mut s.events,
                    EventKind::System,
                    format!("Refused to pause protected PID {}", pid),
                );
            } else {
                match backend.pause_process(pid) {
                    Ok(()) => {
                        let mut s = state.lock().unwrap();
                        push_event(
                            &mut s.events,
                            EventKind::Manual,
                            format!("Manual SIGSTOP paused PID {}", pid),
                        );
                    }
                    Err(e) => {
                        let mut s = state.lock().unwrap();
                        s.last_error = Some(e);
                    }
                }
            }
        }
        EngineCmd::ManualResume { pid } => match backend.resume_process(pid) {
            Ok(()) => {
                let mut s = state.lock().unwrap();
                push_event(
                    &mut s.events,
                    EventKind::Restore,
                    format!("Manual SIGCONT resumed PID {}", pid),
                );
            }
            Err(e) => {
                let mut s = state.lock().unwrap();
                s.last_error = Some(e);
            }
        },
        EngineCmd::ClearEvents => {
            let mut s = state.lock().unwrap();
            s.events.clear();
        }
    }
    true
}

fn sleep_or_cmd<F>(dur: Duration, rx: &Receiver<EngineCmd>, on_cmd: &mut F) -> bool
where
    F: FnMut(EngineCmd) -> bool,
{
    let deadline = Instant::now() + dur;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        let rem = deadline - now;
        match rx.recv_timeout(rem.min(Duration::from_millis(80))) {
            Ok(cmd) => {
                if !on_cmd(cmd) {
                    return false;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn detect_external_daemon(self_pid: u32, procs: &[ProcessSample]) -> Option<ExternalDaemon> {
    for p in procs {
        if p.pid == self_pid {
            continue;
        }
        let base_name = p.name.rsplit(['/', '\\']).next().unwrap_or(&p.name);
        let base_exe = p.exe_path.rsplit(['/', '\\']).next().unwrap_or("");
        if base_name.eq_ignore_ascii_case("resguard") || base_exe.eq_ignore_ascii_case("resguard") {
            return Some(ExternalDaemon {
                pid: p.pid,
                exe_path: if p.exe_path.is_empty() {
                    p.name.clone()
                } else {
                    p.exe_path.clone()
                },
            });
        }
    }
    None
}

fn classify_event(ev: &str) -> EventKind {
    if ev.contains("[DRY-RUN]") {
        EventKind::DryRun
    } else if ev.contains("MEM/SSD guard paused") {
        EventKind::Emergency
    } else if ev.contains("Restored") || ev.contains("resumed") {
        EventKind::Restore
    } else {
        EventKind::Intervention
    }
}

fn push_event(events: &mut VecDeque<EventEntry>, kind: EventKind, message: String) {
    if events.len() >= EVENTS_CAP {
        events.pop_back();
    }
    events.push_front(EventEntry {
        timestamp: local_hms(),
        kind,
        message,
    });
}

pub fn read_tail_lines(path: &Path, max_bytes: u64, max_lines: usize) -> Result<Vec<String>, String> {
    let mut f = File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))
        .map_err(|e| format!("seek failed: {}", e))?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)
        .map_err(|e| format!("read failed: {}", e))?;
    let mut lines: Vec<String> = buf.lines().map(|l| l.to_string()).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.len() > max_lines {
        let drop_n = lines.len() - max_lines;
        lines.drain(0..drop_n);
    }
    Ok(lines)
}

#[cfg(unix)]
pub fn local_hms() -> String {
    use std::ffi::{c_int, c_long};

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
        fn time(tloc: *mut c_long) -> c_long;
        fn localtime_r(timep: *const c_long, result: *mut Ctm) -> *mut Ctm;
    }

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

#[cfg(windows)]
pub fn local_hms() -> String {
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
        fn GetLocalTime(lp_system_time: *mut SystemTime);
    }

    let mut st = SystemTime::default();
    unsafe { GetLocalTime(&mut st) };
    format!("{:02}:{:02}:{:02}", st.w_hour, st.w_minute, st.w_second)
}

#[cfg(not(any(unix, windows)))]
pub fn local_hms() -> String {
    "00:00:00".to_string()
}
