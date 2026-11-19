//! Policy engine.
//!
//! Safety model (v0.2):
//! - The only automatic intervention for CPU load is *lowering priority* of heavy
//!   command-line / batch processes (macOS background QoS, Linux nice/cgroup weight,
//!   Windows below-normal priority). A slowed process keeps running; nothing freezes.
//! - GUI apps (anything inside a macOS `.app` bundle), system binaries, terminals,
//!   shells, our own ancestors, other users' processes and the protect list are never
//!   touched — see [`classify`].
//! - Memory / swap / low-disk pressure produces alerts. Pausing (SIGSTOP / suspend) is
//!   opt-in (`emergency_pause`), one process at a time, and always time-bounded.
//! - Every live intervention is visible through [`Governor::applied`] so the runtime can
//!   persist it and undo it after a crash.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::model::{MemoryPressure, ProcessSample, SystemMetrics};
use crate::platform::PlatformBackend;

/// A demoted process is not restored sooner than this, to avoid demote/restore flapping.
const MIN_DEMOTE_DWELL: Duration = Duration::from_secs(20);
/// After an automatic emergency pause expires, the same PID is not paused again for this long.
const PAUSE_COOLDOWN: Duration = Duration::from_secs(120);
/// Upper bound on any pause, manual or automatic.
pub const MAX_PAUSE_SECS: u64 = 300;
const MAX_NEW_DEMOTIONS_PER_TICK: usize = 8;
const MAX_DEMOTED: usize = 48;
const MEM_ALERT_EVERY: Duration = Duration::from_secs(60);
const NOTIFY_EVERY: Duration = Duration::from_secs(300);
const UNREACHABLE_NOTE_EVERY: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Normal,
    /// CPU above `cpu_soft`.
    Soft,
    /// CPU above `cpu_high` (saturated).
    High,
    MemoryEmergency,
}

impl Zone {
    pub fn label(self) -> &'static str {
        match self {
            Zone::Normal => "NORMAL",
            Zone::Soft => "BUSY",
            Zone::High => "SATURATED",
            Zone::MemoryEmergency => "MEMORY",
        }
    }
}

impl std::fmt::Display for Zone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// resguard changed a process (demote / pause).
    Action,
    /// resguard undid a change.
    Restore,
    /// Something needs the user's attention (memory, swap, disk).
    Alert,
    /// What would have happened in dry-run mode.
    DryRun,
    /// A user-initiated action.
    Manual,
    /// Informational.
    System,
    Error,
}

impl EventKind {
    pub fn code(self) -> &'static str {
        match self {
            EventKind::Action => "action",
            EventKind::Restore => "restore",
            EventKind::Alert => "alert",
            EventKind::DryRun => "dryrun",
            EventKind::Manual => "manual",
            EventKind::System => "system",
            EventKind::Error => "error",
        }
    }

    pub fn from_code(s: &str) -> EventKind {
        match s {
            "action" => EventKind::Action,
            "restore" => EventKind::Restore,
            "alert" => EventKind::Alert,
            "dryrun" => EventKind::DryRun,
            "manual" => EventKind::Manual,
            "error" => EventKind::Error,
            _ => EventKind::System,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GovEvent {
    pub kind: EventKind,
    pub msg: String,
}

impl GovEvent {
    fn new(kind: EventKind, msg: impl Into<String>) -> Self {
        Self {
            kind,
            msg: msg.into(),
        }
    }
}

/// Whether resguard is allowed to touch a process, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility {
    Governable,
    Protected(&'static str),
}

impl Eligibility {
    pub fn is_governable(self) -> bool {
        self == Eligibility::Governable
    }

    pub fn reason(self) -> &'static str {
        match self {
            Eligibility::Governable => "governable",
            Eligibility::Protected(r) => r,
        }
    }
}

/// Path fragments of OS components. Matched case-insensitively against the executable path
/// on every platform (they never collide with user build tools).
const SYSTEM_PATH_PREFIXES: &[&str] = &[
    "/system/",
    "/usr/libexec/",
    "/usr/sbin/",
    "/sbin/",
    "/library/apple/",
    "/usr/lib/systemd/",
    "/lib/systemd/",
    "/usr/lib/xorg/",
    "/usr/bin/xwayland",
    "/usr/bin/gnome-shell",
    "/usr/bin/kwin",
    "/usr/bin/plasmashell",
];

/// Classify every process: may resguard touch it?
pub fn classify(
    self_pid: u32,
    current_uid: u32,
    procs: &[ProcessSample],
    cfg: &Config,
) -> HashMap<u32, Eligibility> {
    let ppid_map: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();

    // Our own ancestor chain (shell -> terminal -> launchd/systemd/explorer).
    let mut ancestors = HashSet::new();
    let mut cur = self_pid;
    for _ in 0..64 {
        match ppid_map.get(&cur) {
            Some(&parent) if parent != 0 && ancestors.insert(parent) => cur = parent,
            _ => break,
        }
    }

    let mut out = HashMap::with_capacity(procs.len());
    for p in procs {
        let e = if p.pid <= 1 {
            Eligibility::Protected("system")
        } else if p.pid == self_pid {
            Eligibility::Protected("resguard")
        } else if current_uid != 0 && p.uid != current_uid {
            Eligibility::Protected("other user")
        } else if ancestors.contains(&p.pid) {
            Eligibility::Protected("our terminal")
        } else if matches_protect_list(p, cfg) {
            Eligibility::Protected("protect list")
        } else if p.exe_path.is_empty() {
            Eligibility::Protected("unknown executable")
        } else {
            classify_path(&p.exe_path)
        };
        out.insert(p.pid, e);
    }
    out
}

fn classify_path(exe_path: &str) -> Eligibility {
    let lower = exe_path.replace('\\', "/").to_ascii_lowercase();
    // macOS GUI apps and all their helpers live inside .app bundles. Lowering their QoS
    // also throttles their disk I/O and can make the UI hang, so they are off limits.
    if lower.contains(".app/") {
        return Eligibility::Protected("app bundle");
    }
    if lower.contains(":/windows/") || lower.starts_with("/windows/") {
        return Eligibility::Protected("system path");
    }
    if SYSTEM_PATH_PREFIXES.iter().any(|pre| lower.starts_with(pre)) {
        return Eligibility::Protected("system path");
    }
    Eligibility::Governable
}

fn matches_protect_list(p: &ProcessSample, cfg: &Config) -> bool {
    let base_name = p.name.rsplit(['/', '\\']).next().unwrap_or(&p.name);
    let stem_name = strip_exe(base_name);
    let base_exe = p.exe_path.rsplit(['/', '\\']).next().unwrap_or("");
    let stem_exe = strip_exe(base_exe);
    cfg.protect_names.iter().any(|prot| {
        base_name.eq_ignore_ascii_case(prot)
            || stem_name.eq_ignore_ascii_case(prot)
            || p.name.eq_ignore_ascii_case(prot)
            || (!base_exe.is_empty()
                && (base_exe.eq_ignore_ascii_case(prot) || stem_exe.eq_ignore_ascii_case(prot)))
            || p
                .exe_path
                .split(['/', '\\'])
                .any(|seg| !seg.is_empty() && seg.eq_ignore_ascii_case(prot))
    })
}

fn strip_exe(s: &str) -> &str {
    s.strip_suffix(".exe")
        .or_else(|| s.strip_suffix(".EXE"))
        .unwrap_or(s)
}

#[derive(Debug, Clone)]
pub struct Demotion {
    pub name: String,
    pub since: Instant,
    pub manual: bool,
}

#[derive(Debug, Clone)]
pub struct Pause {
    pub name: String,
    pub until: Instant,
    pub manual: bool,
}

/// One live intervention, as persisted in the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    Background { pid: u32, name: String },
    Paused { pid: u32, name: String },
}

#[derive(Debug, Clone)]
pub struct TickAction {
    pub zone: Zone,
    pub events: Vec<GovEvent>,
    /// CPU (% of machine) used by the current user's processes.
    pub own_cpu: f64,
    /// CPU (% of machine) used by processes resguard is allowed to govern.
    pub governable_cpu: f64,
    /// CPU (% of machine) resguard cannot attribute or act on: kernel, system services,
    /// other users.
    pub other_cpu: f64,
}

pub struct Governor {
    self_pid: u32,
    demoted: HashMap<u32, Demotion>,
    paused: HashMap<u32, Pause>,
    pause_cooldown: HashMap<u32, Instant>,
    above_soft_since: Option<Instant>,
    below_release_since: Option<Instant>,
    mem_emergency_since: Option<Instant>,
    last_mem_alert: Option<Instant>,
    last_notify: Option<Instant>,
    last_unreachable_note: Option<Instant>,
    dirty: bool,
    pub last_zone: Zone,
}

impl Governor {
    pub fn new(self_pid: u32) -> Self {
        Self {
            self_pid,
            demoted: HashMap::new(),
            paused: HashMap::new(),
            pause_cooldown: HashMap::new(),
            above_soft_since: None,
            below_release_since: None,
            mem_emergency_since: None,
            last_mem_alert: None,
            last_notify: None,
            last_unreachable_note: None,
            dirty: false,
            last_zone: Zone::Normal,
        }
    }

    pub fn self_pid(&self) -> u32 {
        self.self_pid
    }

    pub fn demoted(&self) -> &HashMap<u32, Demotion> {
        &self.demoted
    }

    pub fn paused(&self) -> &HashMap<u32, Pause> {
        &self.paused
    }

    pub fn is_demoted(&self, pid: u32) -> bool {
        self.demoted.contains_key(&pid)
    }

    pub fn is_paused(&self, pid: u32) -> bool {
        self.paused.contains_key(&pid)
    }

    /// Live interventions (only meaningful when not in dry-run).
    pub fn applied(&self) -> Vec<Applied> {
        let mut out: Vec<Applied> = self
            .demoted
            .iter()
            .map(|(&pid, d)| Applied::Background {
                pid,
                name: d.name.clone(),
            })
            .chain(self.paused.iter().map(|(&pid, p)| Applied::Paused {
                pid,
                name: p.name.clone(),
            }))
            .collect();
        out.sort_by_key(|a| match a {
            Applied::Background { pid, .. } | Applied::Paused { pid, .. } => *pid,
        });
        out
    }

    /// True once after the set of live interventions changed.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    pub fn evaluate_tick(
        &mut self,
        now: Instant,
        cfg: &Config,
        metrics: &SystemMetrics,
        procs: &[ProcessSample],
        platform: &dyn PlatformBackend,
    ) -> TickAction {
        let eligibility = classify(self.self_pid, platform.current_uid(), procs, cfg);
        self.evaluate_with(now, cfg, metrics, procs, &eligibility, platform)
    }

    pub fn evaluate_with(
        &mut self,
        now: Instant,
        cfg: &Config,
        metrics: &SystemMetrics,
        procs: &[ProcessSample],
        eligibility: &HashMap<u32, Eligibility>,
        platform: &dyn PlatformBackend,
    ) -> TickAction {
        let mut events = Vec::new();
        let live: HashMap<u32, &ProcessSample> = procs.iter().map(|p| (p.pid, p)).collect();
        let uid = platform.current_uid();

        // Forget processes that exited.
        let before = self.demoted.len() + self.paused.len();
        self.demoted.retain(|pid, _| live.contains_key(pid));
        self.paused.retain(|pid, _| live.contains_key(pid));
        self.pause_cooldown.retain(|_, until| *until > now);
        if self.demoted.len() + self.paused.len() != before {
            self.dirty = true;
        }

        let governable = |p: &ProcessSample| {
            eligibility
                .get(&p.pid)
                .copied()
                .unwrap_or(Eligibility::Protected("unknown"))
                .is_governable()
        };

        // Attribution: how much of the load can resguard actually do anything about?
        let (own_cpu, governable_cpu, other_cpu) = attribution(metrics, procs, eligibility, uid);
        let cpu = metrics.cpu_total_pct;

        // 1. Expire bounded pauses (manual and automatic).
        let expired: Vec<u32> = self
            .paused
            .iter()
            .filter(|(_, p)| p.until <= now)
            .map(|(&pid, _)| pid)
            .collect();
        for pid in expired {
            if let Some(p) = self.paused.remove(&pid) {
                self.dirty = true;
                if !cfg.dry_run {
                    let _ = platform.resume_process(pid);
                }
                if !p.manual {
                    self.pause_cooldown.insert(pid, now + PAUSE_COOLDOWN);
                }
                events.push(GovEvent::new(
                    EventKind::Restore,
                    format!("Resumed pid={} ({}) — pause time limit reached", pid, p.name),
                ));
            }
        }

        // 2. Memory / swap / low-disk guard: alert first; pause only if opted in.
        let disk_low = metrics.disk_free_gb() < cfg.disk_min_free_gb;
        if memory_emergency(metrics, cfg) {
            self.mem_emergency_since.get_or_insert(now);
            let top_hog = procs
                .iter()
                .filter(|p| governable(p) && !p.is_stopped && p.rss_mb() >= cfg.min_proc_rss_mb)
                .max_by_key(|p| p.rss_bytes);
            let top_any = procs
                .iter()
                .filter(|p| (uid == 0 || p.uid == uid) && p.pid != self.self_pid)
                .max_by_key(|p| p.rss_bytes);

            let due = self
                .last_mem_alert
                .map_or(true, |t| now.duration_since(t) >= MEM_ALERT_EVERY);
            if due {
                self.last_mem_alert = Some(now);
                let biggest = top_any
                    .map(|p| format!("largest: {} (pid {}, {:.0} MB)", p.name, p.pid, p.rss_mb()))
                    .unwrap_or_else(|| "no large process found".to_string());
                let msg = format!(
                    "Memory pressure: RAM {:.0}% ({}), swap {:.0} MB, disk free {:.1} GB — {}",
                    metrics.ram_used_pct,
                    metrics.memory_pressure,
                    metrics.swap_used_mb(),
                    metrics.disk_free_gb(),
                    biggest
                );
                if cfg.notify
                    && !cfg.dry_run
                    && self
                        .last_notify
                        .map_or(true, |t| now.duration_since(t) >= NOTIFY_EVERY)
                {
                    self.last_notify = Some(now);
                    platform.send_notification("resguard: memory pressure", &msg);
                }
                events.push(GovEvent::new(EventKind::Alert, msg));
            }

            let auto_pause_active = self.paused.values().any(|p| !p.manual);
            if cfg.emergency_pause && !auto_pause_active {
                if let Some(hog) = top_hog.filter(|h| !self.pause_cooldown.contains_key(&h.pid)) {
                    let secs = cfg.emergency_pause_secs.clamp(5, MAX_PAUSE_SECS);
                    if cfg.dry_run {
                        events.push(GovEvent::new(
                            EventKind::DryRun,
                            format!(
                                "Would pause pid={} ({}) RSS={:.0} MB for {} s",
                                hog.pid,
                                hog.name,
                                hog.rss_mb(),
                                secs
                            ),
                        ));
                    } else if platform.pause_process(hog.pid).is_ok() {
                        self.paused.insert(
                            hog.pid,
                            Pause {
                                name: hog.name.clone(),
                                until: now + Duration::from_secs(secs),
                                manual: false,
                            },
                        );
                        self.dirty = true;
                        events.push(GovEvent::new(
                            EventKind::Action,
                            format!(
                                "Paused pid={} ({}) RSS={:.0} MB for at most {} s (memory emergency)",
                                hog.pid,
                                hog.name,
                                hog.rss_mb(),
                                secs
                            ),
                        ));
                    }
                }
            }
        } else {
            if self.mem_emergency_since.take().is_some() {
                events.push(GovEvent::new(EventKind::System, "Memory pressure cleared"));
                self.last_mem_alert = None;
            }
            let release_ok = metrics.memory_pressure != MemoryPressure::Critical
                && metrics.ram_used_pct < (cfg.ram_high_pct - 6.0)
                && (!disk_low || metrics.swap_used_mb() < cfg.swap_warn_mb * 0.75);
            if release_ok {
                let auto: Vec<u32> = self
                    .paused
                    .iter()
                    .filter(|(_, p)| !p.manual)
                    .map(|(&pid, _)| pid)
                    .collect();
                for pid in auto {
                    if let Some(p) = self.paused.remove(&pid) {
                        self.dirty = true;
                        if !cfg.dry_run {
                            let _ = platform.resume_process(pid);
                        }
                        self.pause_cooldown.insert(pid, now + PAUSE_COOLDOWN);
                        events.push(GovEvent::new(
                            EventKind::Restore,
                            format!("Resumed pid={} ({}) — memory pressure dropped", pid, p.name),
                        ));
                    }
                }
            }
        }

        // 3. CPU: lower priority of heavy batch work under *sustained* load.
        //    Never on RAM pressure alone: background QoS also throttles swap/disk I/O.
        if cpu >= cfg.cpu_soft {
            self.below_release_since = None;
            let soft_since = *self.above_soft_since.get_or_insert(now);
            let sustained =
                now.duration_since(soft_since) >= Duration::from_millis(cfg.soft_sustain_ms);
            let already_acting = self.demoted.values().any(|d| !d.manual);
            if sustained && governable_cpu < cfg.min_governable_cpu_pct && !already_acting {
                let due = self
                    .last_unreachable_note
                    .map_or(true, |t| now.duration_since(t) >= UNREACHABLE_NOTE_EVERY);
                if due {
                    self.last_unreachable_note = Some(now);
                    events.push(GovEvent::new(
                        EventKind::System,
                        format!(
                            "CPU {:.0}% but only {:.0}% comes from processes resguard may slow down \
                             ({:.0}% is kernel/system/other users) — not acting",
                            cpu, governable_cpu, other_cpu
                        ),
                    ));
                }
            } else if sustained && governable_cpu >= cfg.min_governable_cpu_pct {
                let candidates = select_cpu_candidates(procs, &governable, cfg);
                let mut added = 0;
                for p in candidates {
                    if added >= MAX_NEW_DEMOTIONS_PER_TICK || self.demoted.len() >= MAX_DEMOTED {
                        break;
                    }
                    if self.demoted.contains_key(&p.pid) || self.paused.contains_key(&p.pid) {
                        continue;
                    }
                    let entry = Demotion {
                        name: p.name.clone(),
                        since: now,
                        manual: false,
                    };
                    if cfg.dry_run {
                        events.push(GovEvent::new(
                            EventKind::DryRun,
                            format!(
                                "Would lower priority of pid={} ({}) cpu={:.1}%",
                                p.pid, p.name, p.cpu_pct
                            ),
                        ));
                        self.demoted.insert(p.pid, entry);
                        added += 1;
                    } else if platform.set_background(p.pid, true).is_ok() {
                        events.push(GovEvent::new(
                            EventKind::Action,
                            format!(
                                "Lowered priority of pid={} ({}) cpu={:.1}%",
                                p.pid, p.name, p.cpu_pct
                            ),
                        ));
                        self.demoted.insert(p.pid, entry);
                        self.dirty = true;
                        added += 1;
                    }
                }
            }
        } else {
            self.above_soft_since = None;
            if cpu >= cfg.cpu_release {
                self.below_release_since = None;
            } else if self.demoted.values().any(|d| !d.manual) {
                let since = *self.below_release_since.get_or_insert(now);
                if now.duration_since(since) >= Duration::from_millis(cfg.release_cooldown_ms) {
                    let quiet_cutoff = cfg.min_proc_cpu_pct * 0.5;
                    let to_restore: Vec<u32> = self
                        .demoted
                        .iter()
                        .filter(|(pid, d)| {
                            !d.manual
                                && now.duration_since(d.since) >= MIN_DEMOTE_DWELL
                                // Keep sustained heavy jobs demoted so they don't oscillate.
                                && live.get(pid).map_or(0.0, |p| p.cpu_pct) < quiet_cutoff
                        })
                        .map(|(&pid, _)| pid)
                        .collect();
                    for pid in to_restore {
                        if let Some(d) = self.demoted.remove(&pid) {
                            if !cfg.dry_run {
                                let _ = platform.set_background(pid, false);
                                self.dirty = true;
                            }
                            events.push(GovEvent::new(
                                EventKind::Restore,
                                format!("Restored normal priority for pid={} ({})", pid, d.name),
                            ));
                        }
                    }
                }
            }
        }

        let zone = zone_for(metrics, cfg);
        self.last_zone = zone;

        TickAction {
            zone,
            events,
            own_cpu,
            governable_cpu,
            other_cpu,
        }
    }

    /// User request: lower / restore priority of one process.
    pub fn manual_background(
        &mut self,
        now: Instant,
        pid: u32,
        name: &str,
        enable: bool,
        eligibility: Eligibility,
        platform: &dyn PlatformBackend,
    ) -> Result<String, String> {
        if enable {
            if !eligibility.is_governable() {
                return Err(format!(
                    "Refused: {} ({}) is protected ({})",
                    name,
                    pid,
                    eligibility.reason()
                ));
            }
            platform.set_background(pid, true)?;
            self.demoted.insert(
                pid,
                Demotion {
                    name: name.to_string(),
                    since: now,
                    manual: true,
                },
            );
            self.dirty = true;
            Ok(format!("Lowered priority of {} ({})", name, pid))
        } else {
            platform.set_background(pid, false)?;
            if self.demoted.remove(&pid).is_some() {
                self.dirty = true;
            }
            Ok(format!("Restored normal priority for {} ({})", name, pid))
        }
    }

    /// User request: pause one process for a bounded time.
    pub fn manual_pause(
        &mut self,
        now: Instant,
        pid: u32,
        name: &str,
        secs: u64,
        eligibility: Eligibility,
        platform: &dyn PlatformBackend,
    ) -> Result<String, String> {
        if !eligibility.is_governable() {
            return Err(format!(
                "Refused: {} ({}) is protected ({})",
                name,
                pid,
                eligibility.reason()
            ));
        }
        let secs = secs.clamp(5, MAX_PAUSE_SECS);
        platform.pause_process(pid)?;
        self.paused.insert(
            pid,
            Pause {
                name: name.to_string(),
                until: now + Duration::from_secs(secs),
                manual: true,
            },
        );
        self.dirty = true;
        Ok(format!("Paused {} ({}) for {} s — resumes automatically", name, pid, secs))
    }

    /// User request: resume a process (ours or one that is stopped for another reason).
    pub fn manual_resume(
        &mut self,
        pid: u32,
        name: &str,
        platform: &dyn PlatformBackend,
    ) -> Result<String, String> {
        platform.resume_process(pid)?;
        if self.paused.remove(&pid).is_some() {
            self.dirty = true;
        }
        Ok(format!("Resumed {} ({})", name, pid))
    }

    /// Undo every live intervention. Dry-run state is simply forgotten.
    pub fn restore_all(&mut self, cfg: &Config, platform: &dyn PlatformBackend) -> Vec<GovEvent> {
        let mut events = Vec::new();
        for (pid, p) in self.paused.drain() {
            if !cfg.dry_run {
                let _ = platform.resume_process(pid);
                events.push(GovEvent::new(
                    EventKind::Restore,
                    format!("Resumed pid={} ({})", pid, p.name),
                ));
            }
        }
        for (pid, d) in self.demoted.drain() {
            if !cfg.dry_run {
                let _ = platform.set_background(pid, false);
                events.push(GovEvent::new(
                    EventKind::Restore,
                    format!("Restored normal priority for pid={} ({})", pid, d.name),
                ));
            }
        }
        self.above_soft_since = None;
        self.below_release_since = None;
        self.dirty = true;
        events
    }
}

fn memory_emergency(m: &SystemMetrics, cfg: &Config) -> bool {
    let disk_low = m.disk_free_gb() < cfg.disk_min_free_gb;
    let swap_high = m.swap_used_mb() > cfg.swap_warn_mb;
    let ram_high =
        m.ram_used_pct >= cfg.ram_high_pct || m.memory_pressure == MemoryPressure::Warning;
    let ram_crit =
        m.ram_used_pct >= cfg.ram_critical_pct || m.memory_pressure == MemoryPressure::Critical;
    ram_crit || (disk_low && (swap_high || ram_high))
}

/// Zone for display, derived from metrics alone.
pub fn zone_for(m: &SystemMetrics, cfg: &Config) -> Zone {
    if memory_emergency(m, cfg) {
        Zone::MemoryEmergency
    } else if m.cpu_total_pct >= cfg.cpu_high {
        Zone::High
    } else if m.cpu_total_pct >= cfg.cpu_soft {
        Zone::Soft
    } else {
        Zone::Normal
    }
}

/// (own_cpu, governable_cpu, other_cpu), all in % of total machine capacity.
pub fn attribution(
    m: &SystemMetrics,
    procs: &[ProcessSample],
    eligibility: &HashMap<u32, Eligibility>,
    uid: u32,
) -> (f64, f64, f64) {
    let own: f64 = procs
        .iter()
        .filter(|p| uid == 0 || p.uid == uid)
        .map(|p| p.cpu_pct)
        .sum();
    let governable: f64 = procs
        .iter()
        .filter(|p| !p.is_stopped && eligibility.get(&p.pid).map_or(false, |e| e.is_governable()))
        .map(|p| p.cpu_pct)
        .sum();
    (own, governable, (m.cpu_total_pct - own).max(0.0))
}

fn select_cpu_candidates<'a, F>(
    procs: &'a [ProcessSample],
    governable: &F,
    cfg: &Config,
) -> Vec<&'a ProcessSample>
where
    F: Fn(&ProcessSample) -> bool,
{
    let mut chosen = HashSet::new();
    let mut heavy_parents = HashSet::new();
    for p in procs {
        if governable(p) && !p.is_stopped && p.cpu_pct >= cfg.min_proc_cpu_pct {
            chosen.insert(p.pid);
            if p.ppid > 1 {
                heavy_parents.insert(p.ppid);
            }
        }
    }
    // Sibling workers of a heavy process (e.g. parallel rustc / test shards).
    for p in procs {
        if governable(p)
            && !p.is_stopped
            && heavy_parents.contains(&p.ppid)
            && p.cpu_pct >= cfg.min_proc_cpu_pct * 0.5
        {
            chosen.insert(p.pid);
        }
    }
    let mut out: Vec<&ProcessSample> = procs.iter().filter(|p| chosen.contains(&p.pid)).collect();
    out.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    pub struct MockPlatform {
        pub uid: u32,
        pub bg_calls: RefCell<Vec<(u32, bool)>>,
        pub paused: RefCell<Vec<u32>>,
        pub resumed: RefCell<Vec<u32>>,
        pub notified: RefCell<Vec<String>>,
    }

    impl MockPlatform {
        pub fn new() -> Self {
            Self {
                uid: 501,
                bg_calls: RefCell::new(Vec::new()),
                paused: RefCell::new(Vec::new()),
                resumed: RefCell::new(Vec::new()),
                notified: RefCell::new(Vec::new()),
            }
        }
    }

    impl PlatformBackend for MockPlatform {
        fn backend_name(&self) -> &'static str {
            "mock"
        }
        fn num_cores(&self) -> usize {
            10
        }
        fn current_uid(&self) -> u32 {
            self.uid
        }
        fn sample_system(&mut self) -> Result<SystemMetrics, String> {
            unreachable!()
        }
        fn sample_processes(&mut self, _e: u64, _c: usize) -> Vec<ProcessSample> {
            unreachable!()
        }
        fn set_background(&self, pid: u32, enable: bool) -> Result<(), String> {
            self.bg_calls.borrow_mut().push((pid, enable));
            Ok(())
        }
        fn pause_process(&self, pid: u32) -> Result<(), String> {
            self.paused.borrow_mut().push(pid);
            Ok(())
        }
        fn resume_process(&self, pid: u32) -> Result<(), String> {
            self.resumed.borrow_mut().push(pid);
            Ok(())
        }
        fn send_notification(&self, title: &str, _body: &str) {
            self.notified.borrow_mut().push(title.to_string());
        }
    }

    pub fn metrics(
        cpu: f64,
        ram_pct: f64,
        pressure: MemoryPressure,
        swap_mb: f64,
        disk_gb: f64,
    ) -> SystemMetrics {
        let ram_total = 16 * 1024 * 1024 * 1024;
        SystemMetrics {
            cpu_total_pct: cpu,
            cpu_user_pct: cpu,
            cpu_sys_pct: 0.0,
            gpu_pct: None,
            load_1m: 1.0,
            ram_total_bytes: ram_total,
            ram_used_bytes: (ram_total as f64 * ram_pct / 100.0) as u64,
            ram_used_pct: ram_pct,
            memory_pressure: pressure,
            swap_total_bytes: 8 * 1024 * 1024 * 1024,
            swap_used_bytes: (swap_mb * 1024.0 * 1024.0) as u64,
            disk_free_bytes: (disk_gb * 1024.0 * 1024.0 * 1024.0) as u64,
            num_cores: 10,
        }
    }

    /// A command-line process with a governable path.
    pub fn cli(pid: u32, ppid: u32, name: &str, cpu_pct: f64, rss_mb: u64) -> ProcessSample {
        ProcessSample {
            pid,
            ppid,
            uid: 501,
            name: name.to_string(),
            exe_path: format!("/Users/me/.cargo/bin/{}", name),
            cpu_pct,
            rss_bytes: rss_mb * 1024 * 1024,
            is_stopped: false,
        }
    }

    fn with_path(mut p: ProcessSample, path: &str) -> ProcessSample {
        p.exe_path = path.to_string();
        p
    }

    fn cfg() -> Config {
        Config::default()
    }

    fn sustain(cfg: &Config) -> Duration {
        Duration::from_millis(cfg.soft_sustain_ms + 1)
    }

    #[test]
    fn gui_apps_terminals_system_and_ancestors_are_never_governable() {
        let cfg = cfg();
        let procs = vec![
            cli(100, 1, "zsh", 0.0, 4),
            cli(101, 100, "resguard", 0.1, 4),
            with_path(cli(200, 1, "warp-oss", 30.0, 3000), "/Applications/WarpOss.app/Contents/MacOS/warp-oss"),
            with_path(cli(201, 1, "stable", 25.0, 2000), "/Applications/Warp.app/Contents/MacOS/stable"),
            with_path(
                cli(202, 1, "Google Chrome Helper (Renderer)", 40.0, 900),
                "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/Google Chrome Helper (Renderer)",
            ),
            with_path(cli(203, 1, "mds_stores", 20.0, 100), "/System/Library/Frameworks/CoreServices.framework/mds_stores"),
            with_path(cli(204, 1, "SearchIndexer", 20.0, 100), "C:\\Windows\\System32\\SearchIndexer.exe"),
            with_path(cli(205, 1, "mystery", 20.0, 100), ""),
            cli(300, 200, "rustc", 40.0, 1000),
            with_path(cli(301, 200, "flutter_tester", 12.0, 300), "/Users/me/flutter/bin/cache/artifacts/engine/darwin-x64/flutter_tester"),
        ];
        let mut other = cli(400, 1, "rootd", 50.0, 10);
        other.uid = 0;
        let mut all = procs.clone();
        all.push(other);

        let e = classify(101, 501, &all, &cfg);
        assert_eq!(e[&100], Eligibility::Protected("our terminal"));
        assert_eq!(e[&101], Eligibility::Protected("resguard"));
        assert_eq!(e[&200], Eligibility::Protected("protect list"));
        assert_eq!(e[&201], Eligibility::Protected("app bundle"));
        assert_eq!(e[&202], Eligibility::Protected("app bundle"));
        assert!(!e[&203].is_governable());
        assert_eq!(e[&204], Eligibility::Protected("system path"));
        assert_eq!(e[&205], Eligibility::Protected("unknown executable"));
        assert_eq!(e[&400], Eligibility::Protected("other user"));
        assert!(e[&300].is_governable());
        assert!(e[&301].is_governable());
    }

    #[test]
    fn sustained_load_lowers_priority_of_batch_work_only_and_never_pauses() {
        let cfg = cfg();
        let mock = MockPlatform::new();
        let procs = vec![
            cli(102, 1, "resguard", 0.1, 4),
            with_path(cli(200, 1, "warp-oss", 22.0, 800), "/Applications/WarpOss.app/Contents/MacOS/warp-oss"),
            cli(300, 200, "rustc", 40.0, 1000),
            cli(301, 200, "rustc", 30.0, 900),
        ];
        let now = Instant::now();
        let mut gov = Governor::new(102);
        let m = metrics(100.0, 70.0, MemoryPressure::Normal, 0.0, 100.0);
        gov.evaluate_tick(now, &cfg, &m, &procs, &mock);
        let act = gov.evaluate_tick(now + sustain(&cfg), &cfg, &m, &procs, &mock);

        assert_eq!(act.zone, Zone::High);
        let mut calls = mock.bg_calls.borrow().clone();
        calls.sort();
        assert_eq!(calls, vec![(300, true), (301, true)]);
        assert!(mock.paused.borrow().is_empty(), "CPU load must never pause anything");
        assert!(gov.take_dirty());
        assert_eq!(gov.applied().len(), 2);
    }

    #[test]
    fn load_from_unreachable_processes_is_reported_not_acted_on() {
        let cfg = cfg();
        let mock = MockPlatform::new();
        let procs = vec![
            cli(102, 1, "resguard", 0.1, 4),
            cli(300, 1, "rustc", 9.0, 100),
            with_path(cli(200, 1, "warp-oss", 10.0, 800), "/Applications/WarpOss.app/Contents/MacOS/warp-oss"),
        ];
        let now = Instant::now();
        let mut gov = Governor::new(102);
        let m = metrics(100.0, 70.0, MemoryPressure::Normal, 0.0, 100.0);
        gov.evaluate_tick(now, &cfg, &m, &procs, &mock);
        let act = gov.evaluate_tick(now + sustain(&cfg), &cfg, &m, &procs, &mock);

        assert!(mock.bg_calls.borrow().is_empty());
        assert!(act.events.iter().any(|e| e.msg.contains("not acting")));
        assert!(act.other_cpu > 80.0);
    }

    #[test]
    fn demotions_are_restored_after_cooldown_and_dwell() {
        let cfg = cfg();
        let mock = MockPlatform::new();
        let busy = vec![cli(102, 1, "resguard", 0.1, 4), cli(300, 1, "cargo", 60.0, 500)];
        let quiet = vec![cli(102, 1, "resguard", 0.1, 4), cli(300, 1, "cargo", 0.5, 500)];
        let t0 = Instant::now();
        let mut gov = Governor::new(102);
        let hot = metrics(90.0, 50.0, MemoryPressure::Normal, 0.0, 100.0);
        let cool = metrics(20.0, 50.0, MemoryPressure::Normal, 0.0, 100.0);

        gov.evaluate_tick(t0, &cfg, &hot, &busy, &mock);
        gov.evaluate_tick(t0 + sustain(&cfg), &cfg, &hot, &busy, &mock);
        assert!(gov.is_demoted(300));

        // Cool, but within the dwell window: stays demoted.
        let t1 = t0 + Duration::from_secs(2);
        gov.evaluate_tick(t1, &cfg, &cool, &quiet, &mock);
        gov.evaluate_tick(t1 + Duration::from_millis(cfg.release_cooldown_ms + 1), &cfg, &cool, &quiet, &mock);
        assert!(gov.is_demoted(300));

        // After the dwell: restored.
        let t2 = t0 + MIN_DEMOTE_DWELL + Duration::from_secs(10);
        gov.evaluate_tick(t2, &cfg, &cool, &quiet, &mock);
        assert!(!gov.is_demoted(300));
        assert_eq!(*mock.bg_calls.borrow(), vec![(300, true), (300, false)]);
    }

    #[test]
    fn high_ram_alone_does_not_demote() {
        let cfg = cfg();
        let mock = MockPlatform::new();
        let procs = vec![cli(102, 1, "resguard", 0.1, 4), cli(300, 1, "node", 5.0, 4000)];
        let now = Instant::now();
        let mut gov = Governor::new(102);
        let m = metrics(30.0, 93.0, MemoryPressure::Warning, 1000.0, 100.0);
        gov.evaluate_tick(now, &cfg, &m, &procs, &mock);
        gov.evaluate_tick(now + Duration::from_secs(5), &cfg, &m, &procs, &mock);
        assert!(mock.bg_calls.borrow().is_empty());
    }

    #[test]
    fn memory_emergency_alerts_but_does_not_pause_by_default() {
        let cfg = cfg();
        assert!(!cfg.emergency_pause);
        let mock = MockPlatform::new();
        let procs = vec![cli(102, 1, "resguard", 0.1, 4), cli(500, 1, "node", 5.0, 6000)];
        let now = Instant::now();
        let mut gov = Governor::new(102);
        let m = metrics(40.0, 96.0, MemoryPressure::Critical, 5000.0, 5.0);
        let act = gov.evaluate_tick(now, &cfg, &m, &procs, &mock);
        assert_eq!(act.zone, Zone::MemoryEmergency);
        assert!(act.events.iter().any(|e| e.kind == EventKind::Alert && e.msg.contains("node")));
        assert!(mock.paused.borrow().is_empty());
        assert_eq!(mock.notified.borrow().len(), 1);

        // Alerts are rate limited.
        let act2 = gov.evaluate_tick(now + Duration::from_secs(1), &cfg, &m, &procs, &mock);
        assert!(act2.events.iter().all(|e| e.kind != EventKind::Alert));
    }

    #[test]
    fn opt_in_emergency_pause_is_single_bounded_and_never_hits_apps() {
        let mut cfg = cfg();
        cfg.emergency_pause = true;
        cfg.emergency_pause_secs = 20;
        let mock = MockPlatform::new();
        let procs = vec![
            cli(102, 1, "resguard", 0.1, 4),
            with_path(cli(200, 1, "warp-oss", 5.0, 9000), "/Applications/WarpOss.app/Contents/MacOS/warp-oss"),
            cli(500, 1, "node", 5.0, 6000),
            cli(501, 1, "python3", 5.0, 5000),
        ];
        let t0 = Instant::now();
        let mut gov = Governor::new(102);
        let m = metrics(40.0, 96.0, MemoryPressure::Critical, 5000.0, 5.0);

        gov.evaluate_tick(t0, &cfg, &m, &procs, &mock);
        assert_eq!(*mock.paused.borrow(), vec![500], "biggest governable, not the .app");

        // Still in emergency: no cascade onto a second process.
        gov.evaluate_tick(t0 + Duration::from_secs(1), &cfg, &m, &procs, &mock);
        gov.evaluate_tick(t0 + Duration::from_secs(2), &cfg, &m, &procs, &mock);
        assert_eq!(*mock.paused.borrow(), vec![500]);

        // Time limit: resumed even though pressure persists, and put on cooldown.
        let mut stopped = procs.clone();
        stopped[2].is_stopped = true;
        gov.evaluate_tick(t0 + Duration::from_secs(21), &cfg, &m, &stopped, &mock);
        assert_eq!(*mock.resumed.borrow(), vec![500]);
        assert!(!gov.is_paused(500));
    }

    #[test]
    fn emergency_pause_released_when_pressure_drops() {
        let mut cfg = cfg();
        cfg.emergency_pause = true;
        let mock = MockPlatform::new();
        let procs = vec![cli(102, 1, "resguard", 0.1, 4), cli(500, 1, "node", 5.0, 6000)];
        let t0 = Instant::now();
        let mut gov = Governor::new(102);
        gov.evaluate_tick(t0, &cfg, &metrics(40.0, 96.0, MemoryPressure::Critical, 5000.0, 5.0), &procs, &mock);
        assert!(gov.is_paused(500));
        gov.evaluate_tick(t0 + Duration::from_secs(1), &cfg, &metrics(40.0, 60.0, MemoryPressure::Normal, 500.0, 50.0), &procs, &mock);
        assert!(!gov.is_paused(500));
        assert_eq!(*mock.resumed.borrow(), vec![500]);
    }

    #[test]
    fn manual_actions_refuse_protected_and_pauses_expire() {
        let cfg = cfg();
        let mock = MockPlatform::new();
        let t0 = Instant::now();
        let mut gov = Governor::new(102);
        let err = gov
            .manual_pause(t0, 200, "warp-oss", 30, Eligibility::Protected("app bundle"), &mock)
            .unwrap_err();
        assert!(err.contains("app bundle"));
        assert!(mock.paused.borrow().is_empty());

        gov.manual_pause(t0, 300, "rustc", 10_000, Eligibility::Governable, &mock).unwrap();
        let procs = vec![cli(102, 1, "resguard", 0.1, 4), cli(300, 1, "rustc", 0.0, 100)];
        let m = metrics(10.0, 50.0, MemoryPressure::Normal, 0.0, 100.0);
        gov.evaluate_tick(t0 + Duration::from_secs(MAX_PAUSE_SECS - 1), &cfg, &m, &procs, &mock);
        assert!(gov.is_paused(300), "clamped to MAX_PAUSE_SECS, not expired yet");
        gov.evaluate_tick(t0 + Duration::from_secs(MAX_PAUSE_SECS + 1), &cfg, &m, &procs, &mock);
        assert!(!gov.is_paused(300));
        assert_eq!(*mock.resumed.borrow(), vec![300]);
    }

    #[test]
    fn restore_all_undoes_everything() {
        let cfg = cfg();
        let mock = MockPlatform::new();
        let t0 = Instant::now();
        let mut gov = Governor::new(102);
        gov.manual_background(t0, 300, "rustc", true, Eligibility::Governable, &mock).unwrap();
        gov.manual_pause(t0, 301, "node", 30, Eligibility::Governable, &mock).unwrap();
        let events = gov.restore_all(&cfg, &mock);
        assert_eq!(events.len(), 2);
        assert!(gov.applied().is_empty());
        assert_eq!(*mock.resumed.borrow(), vec![301]);
        assert!(mock.bg_calls.borrow().contains(&(300, false)));
    }

    #[test]
    fn dry_run_never_touches_processes() {
        let mut cfg = cfg();
        cfg.dry_run = true;
        cfg.emergency_pause = true;
        let mock = MockPlatform::new();
        let procs = vec![cli(102, 1, "resguard", 0.1, 4), cli(300, 1, "rustc", 80.0, 6000)];
        let t0 = Instant::now();
        let mut gov = Governor::new(102);
        let m = metrics(100.0, 96.0, MemoryPressure::Critical, 5000.0, 5.0);
        gov.evaluate_tick(t0, &cfg, &m, &procs, &mock);
        let act = gov.evaluate_tick(t0 + sustain(&cfg), &cfg, &m, &procs, &mock);
        assert!(act.events.iter().any(|e| e.kind == EventKind::DryRun));
        assert!(mock.bg_calls.borrow().is_empty());
        assert!(mock.paused.borrow().is_empty());
    }
}
