//! The one governor loop shared by the daemon, the TUI and the GUI.
//!
//! Each front-end calls [`Runtime::tick`] once per `tick_ms` and renders the returned
//! [`Snapshot`]. The runtime owns the single-governor lock, the crash-recovery ledger,
//! the status file and the command inbox, so front-ends never call platform actions
//! directly.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::config::Config;
use crate::control::{now_unix, Command, ControlDir, Status, StatusEvent};
use crate::governor::{classify, Applied, Eligibility, EventKind, GovEvent, Governor, Zone};
use crate::model::{ProcessSample, SystemMetrics};
use crate::platform::{create_backend, PlatformBackend};

const RECENT_EVENTS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Change processes (requires the governor lock).
    Active,
    /// Simulate and report what would happen.
    DryRun,
    /// Telemetry only.
    Observe,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Active => "Active",
            Mode::DryRun => "Dry-run",
            Mode::Observe => "Observe",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Daemon,
    Tui,
    Gui,
}

impl Role {
    fn code(self) -> &'static str {
        match self {
            Role::Daemon => "daemon",
            Role::Tui => "tui",
            Role::Gui => "gui",
        }
    }
}

/// Who is in charge right now, from this instance's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authority {
    /// This instance holds the lock and may change processes.
    Governing,
    /// Another resguard instance governs; this one shows its state and forwards commands.
    Deferred { pid: u32, role: String },
    /// Nobody (else) governs and this instance only simulates.
    Simulating,
    /// Telemetry only.
    Observing,
}

impl Authority {
    pub fn describe(&self) -> String {
        match self {
            Authority::Governing => "Governing".to_string(),
            Authority::Deferred { pid, role } => format!("Following {} (PID {})", role, pid),
            Authority::Simulating => "Dry-run (no changes)".to_string(),
            Authority::Observing => "Observing".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub seq: u64,
    pub ts: String,
    pub kind: EventKind,
    pub msg: String,
}

#[derive(Debug, Clone)]
pub struct ProcRow {
    pub sample: ProcessSample,
    pub eligibility: Eligibility,
    pub demoted: bool,
    pub paused: bool,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub metrics: Option<SystemMetrics>,
    pub zone: Zone,
    pub own_cpu: f64,
    pub governable_cpu: f64,
    pub other_cpu: f64,
    pub procs: Vec<ProcRow>,
    /// (pid, name)
    pub demoted: Vec<(u32, String)>,
    /// (pid, seconds left, name)
    pub paused: Vec<(u32, u64, String)>,
    pub authority: Authority,
    pub mode: Mode,
    /// Events produced since the previous tick.
    pub events: Vec<Event>,
}

struct ConfigWatch {
    path: PathBuf,
    mtime: Option<SystemTime>,
    content: Option<String>,
    reload: Box<dyn Fn() -> Result<Config, String> + Send>,
}

pub struct Runtime {
    backend: Box<dyn PlatformBackend>,
    gov: Governor,
    cfg: Config,
    mode: Mode,
    role: Role,
    self_pid: u32,
    ctl: ControlDir,
    holds_lock: bool,
    deferred_to: Option<(u32, String)>,
    deferred_seen_seq: u64,
    last_sample: Instant,
    next_seq: u64,
    recent: VecDeque<Event>,
    pending: Vec<Event>,
    eligibility: HashMap<u32, Eligibility>,
    names: HashMap<u32, String>,
    watch: Option<ConfigWatch>,
    shut_down: bool,
}

impl Runtime {
    pub fn new(cfg: Config, mode: Mode, role: Role) -> Result<Self, String> {
        Self::with_control_dir(cfg, mode, role, ControlDir::default())
    }

    pub fn with_control_dir(
        cfg: Config,
        mode: Mode,
        role: Role,
        ctl: ControlDir,
    ) -> Result<Self, String> {
        let mut backend = create_backend()?;
        let cores = backend.num_cores();
        let _ = backend.sample_processes(0, cores); // prime per-process CPU deltas
        let mode = if cfg.dry_run && mode == Mode::Active {
            Mode::DryRun
        } else {
            mode
        };
        let self_pid = std::process::id();
        let mut rt = Self {
            backend,
            gov: Governor::new(self_pid),
            cfg,
            mode,
            role,
            self_pid,
            ctl,
            holds_lock: false,
            deferred_to: None,
            deferred_seen_seq: 0,
            last_sample: Instant::now(),
            next_seq: 1,
            recent: VecDeque::new(),
            pending: Vec::new(),
            eligibility: HashMap::new(),
            names: HashMap::new(),
            watch: None,
            shut_down: false,
        };
        let name = rt.backend.backend_name();
        rt.push(EventKind::System, format!("resguard {} started ({}, mode {})", env!("CARGO_PKG_VERSION"), name, mode.label()));
        Ok(rt)
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.backend_name()
    }

    pub fn num_cores(&self) -> usize {
        self.backend.num_cores()
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn holds_lock(&self) -> bool {
        self.holds_lock
    }

    /// Reload configuration whenever `path` changes on disk.
    pub fn watch_config<F>(&mut self, path: PathBuf, reload: F)
    where
        F: Fn() -> Result<Config, String> + Send + 'static,
    {
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let content = std::fs::read_to_string(&path).ok();
        self.watch = Some(ConfigWatch {
            path,
            mtime,
            content,
            reload: Box::new(reload),
        });
    }

    pub fn set_config(&mut self, cfg: Config) {
        let dry_changed = cfg.dry_run != self.cfg.dry_run;
        self.cfg = cfg;
        if dry_changed && self.cfg.dry_run && self.mode == Mode::Active {
            self.set_mode(Mode::DryRun);
        }
    }

    pub fn set_mode(&mut self, mode: Mode) {
        if mode == self.mode {
            return;
        }
        // Leaving a mode always starts from a clean slate.
        self.restore_everything("mode change");
        if self.holds_lock && mode != Mode::Active {
            self.release_lock();
        }
        self.gov = Governor::new(self.self_pid);
        self.mode = mode;
        self.push(EventKind::System, format!("Mode: {}", mode.label()));
    }

    fn push(&mut self, kind: EventKind, msg: String) {
        let ev = Event {
            seq: self.next_seq,
            ts: local_hms(),
            kind,
            msg,
        };
        self.next_seq += 1;
        if self.recent.len() >= RECENT_EVENTS {
            self.recent.pop_front();
        }
        self.recent.push_back(ev.clone());
        self.pending.push(ev);
    }

    fn push_gov(&mut self, events: Vec<GovEvent>) {
        for e in events {
            self.push(e.kind, e.msg);
        }
    }

    fn release_lock(&mut self) {
        self.ctl.remove_status();
        self.ctl.release(self.self_pid);
        self.holds_lock = false;
    }

    fn restore_everything(&mut self, why: &str) {
        let mut cfg = self.cfg.clone();
        cfg.dry_run = !self.holds_lock;
        let events = self.gov.restore_all(&cfg, self.backend.as_ref());
        if self.holds_lock {
            if !events.is_empty() {
                self.push(EventKind::System, format!("Undoing all changes ({})", why));
            }
            self.push_gov(events);
            let _ = self.ctl.write_ledger(&[]);
        }
    }

    /// Undo changes a previous governor recorded but never cleaned up (crash, kill -9).
    fn recover_ledger(&mut self, procs: &[ProcessSample]) {
        let entries = self.ctl.read_ledger();
        if entries.is_empty() {
            return;
        }
        let live: HashMap<u32, &str> = procs.iter().map(|p| (p.pid, p.name.as_str())).collect();
        let mut restored = 0;
        for e in &entries {
            let (pid, name, paused) = match e {
                Applied::Background { pid, name } => (*pid, name, false),
                Applied::Paused { pid, name } => (*pid, name, true),
            };
            // Only touch the same process (guards against PID reuse).
            if live.get(&pid).map_or(false, |n| *n == name.as_str()) {
                if paused {
                    let _ = self.backend.resume_process(pid);
                } else {
                    let _ = self.backend.set_background(pid, false);
                }
                restored += 1;
            }
        }
        let _ = self.ctl.write_ledger(&[]);
        self.push(
            EventKind::Restore,
            format!(
                "Recovered from an unclean exit: undid {} leftover change(s)",
                restored
            ),
        );
    }

    fn reload_config_if_changed(&mut self) {
        let Some(w) = self.watch.as_mut() else { return };
        let mtime = std::fs::metadata(&w.path).and_then(|m| m.modified()).ok();
        if mtime == w.mtime {
            return;
        }
        w.mtime = mtime;
        // Editors and some special files bump mtime without changing anything.
        let content = std::fs::read_to_string(&w.path).ok();
        if content == w.content {
            return;
        }
        w.content = content;
        match (w.reload)() {
            Ok(cfg) => {
                self.set_config(cfg);
                self.push(EventKind::System, "Configuration reloaded".to_string());
            }
            Err(e) => self.push(EventKind::Error, format!("Config not reloaded: {}", e)),
        }
    }

    /// Ask whoever governs (this instance or another one) to do something.
    pub fn command(&mut self, cmd: Command) -> Result<String, String> {
        if self.holds_lock {
            return self.apply_command(cmd);
        }
        if let Some((pid, role)) = self.deferred_to.clone() {
            self.ctl.send_command(&cmd)?;
            return Ok(format!("Sent to resguard {} (PID {})", role, pid));
        }
        Err("Switch to Active mode to change processes".to_string())
    }

    fn apply_command(&mut self, cmd: Command) -> Result<String, String> {
        let now = Instant::now();
        let name_of = |pid: u32| self.names.get(&pid).cloned().unwrap_or_else(|| format!("pid {}", pid));
        let elig = |pid: u32| {
            self.eligibility
                .get(&pid)
                .copied()
                .unwrap_or(Eligibility::Protected("not found"))
        };
        let result = match cmd {
            Command::Background { pid, on } => {
                let (name, e) = (name_of(pid), elig(pid));
                self.gov.manual_background(now, pid, &name, on, e, self.backend.as_ref())
            }
            Command::Pause { pid, secs } => {
                let (name, e) = (name_of(pid), elig(pid));
                self.gov.manual_pause(now, pid, &name, secs, e, self.backend.as_ref())
            }
            Command::Resume { pid } => {
                let name = name_of(pid);
                self.gov.manual_resume(pid, &name, self.backend.as_ref())
            }
            Command::RestoreAll => {
                self.restore_everything("requested");
                Ok("Undid all changes".to_string())
            }
        };
        match &result {
            Ok(msg) => self.push(EventKind::Manual, msg.clone()),
            Err(msg) => self.push(EventKind::Error, msg.clone()),
        }
        if self.gov.take_dirty() {
            let _ = self.ctl.write_ledger(&self.gov.applied());
        }
        result
    }

    pub fn tick(&mut self) -> Snapshot {
        let now = Instant::now();
        let elapsed_ns = now.duration_since(self.last_sample).as_nanos().max(1) as u64;
        self.last_sample = now;

        self.reload_config_if_changed();

        let metrics = self.backend.sample_system();
        let cores = self.backend.num_cores();
        let procs = self.backend.sample_processes(elapsed_ns, cores);

        let alive = |pid: u32| procs.iter().any(|p| p.pid == pid && is_resguard_process(p));

        // ---- authority
        if self.mode == Mode::Active && !self.holds_lock {
            match self.ctl.try_acquire(self.self_pid, self.role.code(), &alive) {
                Ok(()) => {
                    self.holds_lock = true;
                    self.deferred_to = None;
                    self.recover_ledger(&procs);
                    self.push(EventKind::System, "Governing (this instance holds the lock)".to_string());
                }
                Err(holder) => {
                    if self.deferred_to.as_ref().map(|d| d.0) != Some(holder.pid) {
                        self.push(
                            EventKind::System,
                            format!("resguard {} (PID {}) is governing — following it", holder.role, holder.pid),
                        );
                        self.deferred_seen_seq = 0;
                    }
                    self.deferred_to = Some((holder.pid, holder.role));
                }
            }
        } else if !self.holds_lock {
            self.deferred_to = self
                .ctl
                .read_lock()
                .filter(|l| l.pid != self.self_pid && alive(l.pid))
                .map(|l| (l.pid, l.role));
        }

        // ---- classification (always, for display and manual commands)
        self.eligibility = classify(self.self_pid, self.backend.current_uid(), &procs, &self.cfg);
        self.names = procs.iter().map(|p| (p.pid, p.name.clone())).collect();

        if self.holds_lock {
            for cmd in self.ctl.take_commands() {
                let _ = self.apply_command(cmd);
            }
        }

        let authority = if self.holds_lock {
            Authority::Governing
        } else if let Some((pid, role)) = &self.deferred_to {
            Authority::Deferred {
                pid: *pid,
                role: role.clone(),
            }
        } else if self.mode == Mode::Observe {
            Authority::Observing
        } else {
            Authority::Simulating
        };

        let mut zone = Zone::Normal;
        let (mut own_cpu, mut governable_cpu, mut other_cpu) = (0.0, 0.0, 0.0);

        match &metrics {
            Ok(m) => {
                let evaluate = matches!(authority, Authority::Governing | Authority::Simulating);
                if evaluate {
                    let mut cfg = self.cfg.clone();
                    cfg.dry_run = authority != Authority::Governing;
                    let eligibility = std::mem::take(&mut self.eligibility);
                    let action = self.gov.evaluate_with(now, &cfg, m, &procs, &eligibility, self.backend.as_ref());
                    self.eligibility = eligibility;
                    zone = action.zone;
                    own_cpu = action.own_cpu;
                    governable_cpu = action.governable_cpu;
                    other_cpu = action.other_cpu;
                    self.push_gov(action.events);
                } else {
                    let a = crate::governor::attribution(m, &procs, &self.eligibility, self.backend.current_uid());
                    own_cpu = a.0;
                    governable_cpu = a.1;
                    other_cpu = a.2;
                    zone = crate::governor::zone_for(m, &self.cfg);
                }
            }
            Err(e) => self.push(EventKind::Error, format!("System sampling failed: {}", e)),
        }

        // ---- persistence (governor only)
        if self.holds_lock {
            if self.gov.take_dirty() {
                if let Err(e) = self.ctl.write_ledger(&self.gov.applied()) {
                    self.push(EventKind::Error, format!("Could not write ledger: {}", e));
                }
            }
            let status = Status {
                pid: self.self_pid,
                role: self.role.code().to_string(),
                updated_unix: now_unix(),
                demoted: self.demoted_list(),
                paused: self.paused_list(now),
                events: self
                    .recent
                    .iter()
                    .map(|e| StatusEvent {
                        seq: e.seq,
                        kind: e.kind,
                        ts: e.ts.clone(),
                        msg: e.msg.clone(),
                    })
                    .collect(),
            };
            let _ = self.ctl.write_status(&status);
        }

        // ---- what to show
        let (demoted, paused) = if let Authority::Deferred { .. } = authority {
            let st = self.ctl.read_status().unwrap_or_default();
            for ev in st.events.iter().filter(|e| e.seq > self.deferred_seen_seq) {
                self.pending.push(Event {
                    seq: 0,
                    ts: ev.ts.clone(),
                    kind: ev.kind,
                    msg: format!("[{}] {}", st.role, ev.msg),
                });
            }
            if let Some(last) = st.events.last() {
                self.deferred_seen_seq = last.seq;
            }
            (st.demoted, st.paused)
        } else {
            (self.demoted_list(), self.paused_list(now))
        };

        let demoted_set: std::collections::HashSet<u32> = demoted.iter().map(|d| d.0).collect();
        let paused_set: std::collections::HashSet<u32> = paused.iter().map(|p| p.0).collect();
        let rows = procs
            .into_iter()
            .map(|p| ProcRow {
                eligibility: self
                    .eligibility
                    .get(&p.pid)
                    .copied()
                    .unwrap_or(Eligibility::Protected("unknown")),
                demoted: demoted_set.contains(&p.pid),
                paused: paused_set.contains(&p.pid),
                sample: p,
            })
            .collect();

        Snapshot {
            metrics: metrics.ok(),
            zone,
            own_cpu,
            governable_cpu,
            other_cpu,
            procs: rows,
            demoted,
            paused,
            authority,
            mode: self.mode,
            events: std::mem::take(&mut self.pending),
        }
    }

    fn demoted_list(&self) -> Vec<(u32, String)> {
        let mut v: Vec<(u32, String)> = self
            .gov
            .demoted()
            .iter()
            .map(|(&pid, d)| (pid, d.name.clone()))
            .collect();
        v.sort();
        v
    }

    fn paused_list(&self, now: Instant) -> Vec<(u32, u64, String)> {
        let mut v: Vec<(u32, u64, String)> = self
            .gov
            .paused()
            .iter()
            .map(|(&pid, p)| (pid, p.until.saturating_duration_since(now).as_secs(), p.name.clone()))
            .collect();
        v.sort();
        v
    }

    /// Undo every change, release the lock. Safe to call more than once.
    /// Returns the events produced while shutting down.
    pub fn shutdown(&mut self) -> Vec<Event> {
        if self.shut_down {
            return Vec::new();
        }
        self.shut_down = true;
        if self.holds_lock {
            self.restore_everything("exit");
            self.release_lock();
        }
        std::mem::take(&mut self.pending)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

pub fn is_resguard_process(p: &ProcessSample) -> bool {
    const NAMES: &[&str] = &["resguard", "rs-dash", "rs-tui"];
    let base = |s: &str| {
        let b = s.rsplit(['/', '\\']).next().unwrap_or(s);
        b.strip_suffix(".exe").unwrap_or(b).to_ascii_lowercase()
    };
    let n = base(&p.name);
    let e = base(&p.exe_path);
    NAMES.iter().any(|x| n == *x || e == *x)
}

/// `resguard --restore`: undo every recorded change, whether or not a governor is running.
pub fn restore_cli() -> Result<String, String> {
    let ctl = ControlDir::default();
    let mut backend = create_backend()?;
    let cores = backend.num_cores();
    let procs = backend.sample_processes(0, cores);
    let self_pid = std::process::id();
    let holder = ctl
        .read_lock()
        .filter(|l| l.pid != self_pid && procs.iter().any(|p| p.pid == l.pid && is_resguard_process(p)));

    if let Some(h) = holder {
        ctl.send_command(&Command::RestoreAll)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if ctl.read_ledger().is_empty() {
                return Ok(format!(
                    "resguard {} (PID {}) undid all of its changes. It keeps running; stop it to prevent new ones.",
                    h.role, h.pid
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        return Err(format!(
            "Asked resguard {} (PID {}) to undo its changes but it did not confirm within 5 s",
            h.role, h.pid
        ));
    }

    let entries = ctl.read_ledger();
    let live: HashMap<u32, &str> = procs.iter().map(|p| (p.pid, p.name.as_str())).collect();
    let mut n = 0;
    for e in &entries {
        match e {
            Applied::Background { pid, name } if live.get(pid) == Some(&name.as_str()) => {
                let _ = backend.set_background(*pid, false);
                n += 1;
            }
            Applied::Paused { pid, name } if live.get(pid) == Some(&name.as_str()) => {
                let _ = backend.resume_process(*pid);
                n += 1;
            }
            _ => {}
        }
    }
    let _ = ctl.write_ledger(&[]);
    Ok(format!("Nothing is governing. Undid {} recorded change(s).", n))
}

#[cfg(unix)]
pub fn local_hms() -> String {
    use std::ffi::{c_int, c_long};

    #[repr(C)]
    struct Ctm {
        tm_sec: c_int,
        tm_min: c_int,
        tm_hour: c_int,
        _rest: [u8; 64],
    }

    extern "C" {
        fn time(tloc: *mut c_long) -> c_long;
        fn localtime_r(timep: *const c_long, result: *mut Ctm) -> *mut Ctm;
    }

    unsafe {
        let mut now: c_long = 0;
        time(&mut now);
        let mut tm = Ctm {
            tm_sec: 0,
            tm_min: 0,
            tm_hour: 0,
            _rest: [0u8; 64],
        };
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
    struct SystemTimeW {
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
        fn GetLocalTime(lp_system_time: *mut SystemTimeW);
    }

    let mut st = SystemTimeW::default();
    unsafe { GetLocalTime(&mut st) };
    format!("{:02}:{:02}:{:02}", st.w_hour, st.w_minute, st.w_second)
}

#[cfg(not(any(unix, windows)))]
pub fn local_hms() -> String {
    "00:00:00".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thresholds no machine can reach, so these tests never touch real processes.
    fn inert_cfg() -> Config {
        let mut c = Config::default();
        c.cpu_soft = 100.0;
        c.cpu_high = 100.0;
        c.cpu_release = 99.0;
        c.ram_high_pct = 99.5;
        c.ram_critical_pct = 99.9;
        c.disk_min_free_gb = 0.0;
        c.notify = false;
        c
    }

    fn ctl(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("resguard_rt_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn single_governor_lock_followers_and_inbox() {
        let dir = ctl("lock");
        let mut a = Runtime::with_control_dir(inert_cfg(), Mode::Active, Role::Daemon, ControlDir::new(dir.clone())).unwrap();
        assert_eq!(a.tick().authority, Authority::Governing);

        // Same process, so pretend the holder's PID is a live resguard by using the real lock
        // owner check through a second runtime in this process: it must defer, not govern.
        let mut b = Runtime::with_control_dir(inert_cfg(), Mode::Active, Role::Gui, ControlDir::new(dir.clone())).unwrap();
        b.self_pid = a.self_pid + 1; // distinct identity inside one test process
        // The lock holder is this test binary, which is not named resguard; teach the check.
        let held = ControlDir::new(dir.clone()).try_acquire(b.self_pid, "gui", &|pid| pid == a.self_pid);
        assert_eq!(held.unwrap_err().pid, a.self_pid);

        // Commands from a follower travel through the inbox and are applied by the governor.
        ControlDir::new(dir.clone()).send_command(&Command::RestoreAll).unwrap();
        let snap = a.tick();
        assert!(snap.events.iter().any(|e| e.kind == EventKind::Manual && e.msg.contains("Undid")));

        // Status file is published for followers.
        let st = ControlDir::new(dir.clone()).read_status().unwrap();
        assert_eq!(st.pid, a.self_pid);

        a.shutdown();
        assert!(ControlDir::new(dir.clone()).read_lock().is_none(), "lock released on shutdown");
        b.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leftover_ledger_is_recovered_without_touching_unrelated_pids() {
        let dir = ctl("ledger");
        let c = ControlDir::new(dir.clone());
        // A PID that cannot exist plus a live PID with the wrong name: neither may be touched.
        c.write_ledger(&[
            Applied::Paused { pid: u32::MAX - 7, name: "ghost".into() },
            Applied::Background { pid: std::process::id(), name: "not-this-test".into() },
        ])
        .unwrap();
        let mut rt = Runtime::with_control_dir(inert_cfg(), Mode::Active, Role::Daemon, ControlDir::new(dir.clone())).unwrap();
        let snap = rt.tick();
        assert!(snap
            .events
            .iter()
            .any(|e| e.kind == EventKind::Restore && e.msg.contains("undid 0 leftover")));
        assert!(c.read_ledger().is_empty());
        rt.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_never_takes_the_lock() {
        let dir = ctl("dry");
        let mut rt = Runtime::with_control_dir(inert_cfg(), Mode::DryRun, Role::Tui, ControlDir::new(dir.clone())).unwrap();
        assert_eq!(rt.tick().authority, Authority::Simulating);
        assert!(ControlDir::new(dir.clone()).read_lock().is_none());
        assert!(rt.command(Command::RestoreAll).is_err());
        rt.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
