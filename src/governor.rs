use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::model::{MemoryPressure, ProcessSample, SystemMetrics};
use crate::platform::PlatformBackend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Normal,
    Soft,
    High,
    Cap,
    MemoryEmergency,
}

impl std::fmt::Display for Zone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Zone::Normal => write!(f, "NORMAL"),
            Zone::Soft => write!(f, "SOFT(75+)"),
            Zone::High => write!(f, "HIGH(95+)"),
            Zone::Cap => write!(f, "CAP(97+)"),
            Zone::MemoryEmergency => write!(f, "MEM/SSD-GUARD"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TickAction {
    pub zone: Zone,
    pub throttle_ratio: f64,
    pub duty_pause_pids: Vec<u32>,
    pub pause_duration: Duration,
    pub run_duration: Duration,
    pub events: Vec<String>,
}

pub struct Governor {
    self_pid: u32,
    demoted: HashMap<u32, String>,
    emergency_paused: HashMap<u32, String>,
    throttle_ratio: f64,
    above_soft_since: Option<Instant>,
    below_release_since: Option<Instant>,
    last_notify: Option<Instant>,
    pub last_zone: Zone,
}

impl Governor {
    pub fn new(self_pid: u32) -> Self {
        Self {
            self_pid,
            demoted: HashMap::new(),
            emergency_paused: HashMap::new(),
            throttle_ratio: 0.0,
            above_soft_since: None,
            below_release_since: None,
            last_notify: None,
            last_zone: Zone::Normal,
        }
    }

    pub fn demoted_count(&self) -> usize {
        self.demoted.len()
    }

    pub fn emergency_paused_count(&self) -> usize {
        self.emergency_paused.len()
    }

    pub fn is_demoted(&self, pid: u32) -> bool {
        self.demoted.contains_key(&pid)
    }

    pub fn is_emergency_paused(&self, pid: u32) -> bool {
        self.emergency_paused.contains_key(&pid)
    }

    pub fn demoted_pids(&self) -> &HashMap<u32, String> {
        &self.demoted
    }

    pub fn emergency_paused_pids(&self) -> &HashMap<u32, String> {
        &self.emergency_paused
    }

    pub fn throttle_ratio(&self) -> f64 {
        self.throttle_ratio
    }

    pub fn evaluate_tick(
        &mut self,
        now: Instant,
        cfg: &Config,
        metrics: &SystemMetrics,
        procs: &[ProcessSample],
        platform: &dyn PlatformBackend,
    ) -> TickAction {
        let mut events = Vec::new();
        let live_pids: HashSet<u32> = procs.iter().map(|p| p.pid).collect();

        // Prune exited processes from tracked sets
        self.demoted.retain(|pid, _| live_pids.contains(pid));
        self.emergency_paused.retain(|pid, _| live_pids.contains(pid));

        let protected = build_protected_pids(self.self_pid, platform.current_uid(), procs, cfg);

        let disk_low = metrics.disk_free_gb() < cfg.disk_min_free_gb;
        let swap_high = metrics.swap_used_mb() > cfg.swap_warn_mb;
        let ram_high = metrics.ram_used_pct >= cfg.ram_high_pct
            || metrics.memory_pressure == MemoryPressure::Warning;
        let ram_crit = metrics.ram_used_pct >= cfg.ram_critical_pct
            || metrics.memory_pressure == MemoryPressure::Critical;
        let disk_swap_emergency = disk_low && (swap_high || ram_high);
        let memory_emergency = ram_crit || disk_swap_emergency;

        // 1. Memory + Swap + Low-SSD Emergency Guard
        if memory_emergency {
            let mut ram_hogs: Vec<&ProcessSample> = procs
                .iter()
                .filter(|p| {
                    !protected.contains(&p.pid)
                        && !p.is_stopped
                        && !self.emergency_paused.contains_key(&p.pid)
                        && p.rss_mb() >= cfg.min_proc_rss_mb
                })
                .collect();
            ram_hogs.sort_by(|a, b| b.rss_bytes.cmp(&a.rss_bytes));

            if let Some(hog) = ram_hogs.first() {
                if cfg.dry_run {
                    events.push(format!(
                        "[DRY-RUN] MEM/SSD guard would pause pid={} ({}) RSS={:.0}MB (RAM={:.1}% {}, Swap={:.0}MB, DiskFree={:.1}GB)",
                        hog.pid,
                        hog.name,
                        hog.rss_mb(),
                        metrics.ram_used_pct,
                        metrics.memory_pressure,
                        metrics.swap_used_mb(),
                        metrics.disk_free_gb()
                    ));
                } else if platform.pause_process(hog.pid).is_ok() {
                    let msg = format!(
                        "MEM/SSD guard paused pid={} ({}) RSS={:.0}MB (RAM={:.1}% {}, Swap={:.0}MB, DiskFree={:.1}GB)",
                        hog.pid,
                        hog.name,
                        hog.rss_mb(),
                        metrics.ram_used_pct,
                        metrics.memory_pressure,
                        metrics.swap_used_mb(),
                        metrics.disk_free_gb()
                    );
                    self.emergency_paused.insert(hog.pid, hog.name.clone());
                    events.push(msg.clone());

                    if cfg.notify
                        && self
                            .last_notify
                            .map_or(true, |t| now.duration_since(t) >= Duration::from_secs(60))
                    {
                        self.last_notify = Some(now);
                        platform.send_notification("resguard: Memory/SSD Guard", &msg);
                    }
                }
            }
        } else if !self.emergency_paused.is_empty() {
            let release_ok = metrics.memory_pressure != MemoryPressure::Critical
                && metrics.ram_used_pct < (cfg.ram_high_pct - 6.0)
                && (!disk_low || metrics.swap_used_mb() < cfg.swap_warn_mb * 0.75);
            if release_ok {
                let to_resume: Vec<(u32, String)> = self.emergency_paused.drain().collect();
                for (pid, name) in to_resume {
                    if !cfg.dry_run {
                        let _ = platform.resume_process(pid);
                    }
                    events.push(format!(
                        "MEM/SSD guard resumed pid={} ({}) after pressure dropped",
                        pid, name
                    ));
                }
            }
        }

        // 2. Soft Zone: Background QoS / E-core demotion ONLY on sustained CPU load
        // Note: never demote purely on RAM pressure, because PRIO_DARWIN_BG throttles swap/disk I/O.
        let cpu = metrics.cpu_total_pct;
        if cpu >= cfg.cpu_soft {
            self.below_release_since = None;
            let soft_since = *self.above_soft_since.get_or_insert(now);
            if now.duration_since(soft_since) >= Duration::from_millis(cfg.soft_sustain_ms) {
                let candidates = select_cpu_candidates(procs, &protected, cfg);
                for p in candidates {
                    if self.demoted.contains_key(&p.pid) {
                        continue;
                    }
                    if cfg.dry_run {
                        events.push(format!(
                            "[DRY-RUN] would demote pid={} ({}) cpu={:.1}% rss={:.0}MB to background/E-cores",
                            p.pid,
                            p.name,
                            p.cpu_pct,
                            p.rss_mb()
                        ));
                        self.demoted.insert(p.pid, p.name.clone());
                    } else if platform.set_background(p.pid, true).is_ok() {
                        events.push(format!(
                            "Demoted pid={} ({}) cpu={:.1}% to background QoS",
                            p.pid, p.name, p.cpu_pct
                        ));
                        self.demoted.insert(p.pid, p.name.clone());
                    }
                }
            }
        } else {
            self.above_soft_since = None;
            if cpu >= cfg.cpu_release {
                self.below_release_since = None;
            } else if !self.demoted.is_empty() {
                let since = *self.below_release_since.get_or_insert(now);
                if now.duration_since(since) >= Duration::from_millis(cfg.release_cooldown_ms) {
                    let proc_cpu: HashMap<u32, f64> =
                        procs.iter().map(|p| (p.pid, p.cpu_pct)).collect();
                    let quiet_cutoff = cfg.min_proc_cpu_pct * 0.5;
                    let mut to_restore = Vec::new();
                    for (&pid, name) in &self.demoted {
                        // Keep process demoted if it is still actively pegging background/E-cores
                        // so sustained heavy jobs don't oscillate 99% -> 45% -> 99%.
                        if proc_cpu.get(&pid).copied().unwrap_or(0.0) < quiet_cutoff {
                            to_restore.push((pid, name.clone()));
                        }
                    }
                    for (pid, name) in to_restore {
                        self.demoted.remove(&pid);
                        if !cfg.dry_run {
                            let _ = platform.set_background(pid, false);
                        }
                        events.push(format!("Restored normal QoS for pid={} ({})", pid, name));
                    }
                }
            }
        }

        // 3. High / Cap Zone: Adaptive Duty-Cycle Throttling
        let non_protected_cpu: f64 = procs
            .iter()
            .filter(|p| {
                !protected.contains(&p.pid)
                    && !p.is_stopped
                    && !self.emergency_paused.contains_key(&p.pid)
            })
            .map(|p| p.cpu_pct)
            .sum();

        // Only pulse user processes when non-protected workloads are actually driving high CPU
        let user_driven_load = non_protected_cpu >= 25.0;

        if cpu >= cfg.cpu_cap && user_driven_load {
            self.throttle_ratio = (self.throttle_ratio + 0.24).min(0.75);
        } else if cpu >= cfg.cpu_high && user_driven_load {
            self.throttle_ratio = (self.throttle_ratio + 0.10).min(0.55);
        } else if cpu >= cfg.cpu_soft && user_driven_load {
            self.throttle_ratio = (self.throttle_ratio - 0.04).max(0.0);
        } else {
            self.throttle_ratio = (self.throttle_ratio - 0.22).max(0.0);
        }

        let mut duty_pause_pids = Vec::new();
        let mut pause_duration = Duration::ZERO;
        let mut run_duration = Duration::from_millis(cfg.tick_ms);

        if self.throttle_ratio > 0.02 && user_driven_load {
            let mut active_hogs: Vec<&ProcessSample> = procs
                .iter()
                .filter(|p| {
                    !protected.contains(&p.pid)
                        && !p.is_stopped
                        && !self.emergency_paused.contains_key(&p.pid)
                        && p.cpu_pct >= cfg.min_proc_cpu_pct
                })
                .collect();
            active_hogs.sort_by(|a, b| {
                b.cpu_pct
                    .partial_cmp(&a.cpu_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let excess_needed = (cpu - cfg.cpu_soft).max(cfg.min_proc_cpu_pct);
            let mut covered = 0.0;
            for p in active_hogs.into_iter().take(16) {
                duty_pause_pids.push(p.pid);
                covered += p.cpu_pct;
                if covered >= excess_needed && duty_pause_pids.len() >= 2 {
                    break;
                }
            }

            if !duty_pause_pids.is_empty() {
                let raw_pause_ms = ((cfg.tick_ms as f64) * self.throttle_ratio).round() as u64;
                let max_pause = cfg.tick_ms.saturating_sub(25).max(20);
                let pause_ms = raw_pause_ms.clamp(20, max_pause);
                let run_ms = cfg.tick_ms.saturating_sub(pause_ms);
                pause_duration = Duration::from_millis(pause_ms);
                run_duration = Duration::from_millis(run_ms);

                if cfg.dry_run {
                    events.push(format!(
                        "[DRY-RUN] duty-cycle {:.0}% ({}ms pause) on PIDs {:?}",
                        self.throttle_ratio * 100.0,
                        pause_ms,
                        duty_pause_pids
                    ));
                }
            }
        }

        let zone = if memory_emergency {
            Zone::MemoryEmergency
        } else if cpu >= cfg.cpu_cap {
            Zone::Cap
        } else if cpu >= cfg.cpu_high {
            Zone::High
        } else if cpu >= cfg.cpu_soft {
            Zone::Soft
        } else {
            Zone::Normal
        };
        self.last_zone = zone;

        TickAction {
            zone,
            throttle_ratio: self.throttle_ratio,
            duty_pause_pids,
            pause_duration,
            run_duration,
            events,
        }
    }

    pub fn cleanup(&mut self, cfg: &Config, platform: &dyn PlatformBackend) {
        if cfg.dry_run {
            self.emergency_paused.clear();
            self.demoted.clear();
            return;
        }
        for (pid, _) in self.emergency_paused.drain() {
            let _ = platform.resume_process(pid);
        }
        for (pid, _) in self.demoted.drain() {
            let _ = platform.set_background(pid, false);
        }
    }
}

pub fn build_protected_pids(
    self_pid: u32,
    current_uid: u32,
    procs: &[ProcessSample],
    cfg: &Config,
) -> HashSet<u32> {
    let mut protected = HashSet::new();
    protected.insert(0);
    protected.insert(1);
    protected.insert(self_pid);

    let ppid_map: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();

    // Protect direct ancestor chain (our shell -> terminal -> launchd)
    let mut cur = self_pid;
    for _ in 0..64 {
        if let Some(&parent) = ppid_map.get(&cur) {
            if parent == 0 || !protected.insert(parent) {
                break;
            }
            cur = parent;
        } else {
            break;
        }
    }

    for p in procs {
        if current_uid != 0 && p.uid != current_uid {
            protected.insert(p.pid);
            continue;
        }
        let base_name = p.name.rsplit(['/', '\\']).next().unwrap_or(&p.name);
        let stem_name = base_name.strip_suffix(".exe").or_else(|| base_name.strip_suffix(".EXE")).unwrap_or(base_name);
        let base_exe = p.exe_path.rsplit(['/', '\\']).next().unwrap_or("");
        let stem_exe = base_exe.strip_suffix(".exe").or_else(|| base_exe.strip_suffix(".EXE")).unwrap_or(base_exe);
        let matches_protected = cfg.protect_names.iter().any(|prot| {
            base_name.eq_ignore_ascii_case(prot)
                || stem_name.eq_ignore_ascii_case(prot)
                || p.name.eq_ignore_ascii_case(prot)
                || (!base_exe.is_empty() && (base_exe.eq_ignore_ascii_case(prot) || stem_exe.eq_ignore_ascii_case(prot)))
                || p.exe_path
                    .split(['/', '\\'])
                    .any(|seg| !seg.is_empty() && seg.eq_ignore_ascii_case(prot))
        });
        if matches_protected {
            protected.insert(p.pid);
        }
    }

    protected
}

fn select_cpu_candidates<'a>(
    procs: &'a [ProcessSample],
    protected: &HashSet<u32>,
    cfg: &Config,
) -> Vec<&'a ProcessSample> {
    let mut chosen_pids = HashSet::new();
    let mut heavy_ppids = HashSet::new();

    for p in procs {
        if protected.contains(&p.pid) || p.is_stopped {
            continue;
        }
        if p.cpu_pct >= cfg.min_proc_cpu_pct {
            chosen_pids.insert(p.pid);
            if p.ppid > 1 && !protected.contains(&p.ppid) {
                heavy_ppids.insert(p.ppid);
            }
        }
    }

    // Include active sibling workers sharing a non-protected parent process
    for p in procs {
        if !protected.contains(&p.pid)
            && !p.is_stopped
            && heavy_ppids.contains(&p.ppid)
            && p.cpu_pct >= (cfg.min_proc_cpu_pct * 0.5)
        {
            chosen_pids.insert(p.pid);
        }
    }

    let mut out: Vec<&ProcessSample> = procs
        .iter()
        .filter(|p| chosen_pids.contains(&p.pid))
        .collect();
    out.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct MockPlatform {
        uid: u32,
        bg_calls: RefCell<Vec<(u32, bool)>>,
        paused: RefCell<Vec<u32>>,
        resumed: RefCell<Vec<u32>>,
        notified: RefCell<Vec<String>>,
    }

    impl MockPlatform {
        fn new() -> Self {
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

    fn base_metrics(
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
            load_1m: 2.0,
            ram_total_bytes: ram_total,
            ram_used_bytes: ((ram_total as f64) * (ram_pct / 100.0)) as u64,
            ram_used_pct: ram_pct,
            memory_pressure: pressure,
            swap_total_bytes: 4 * 1024 * 1024 * 1024,
            swap_used_bytes: (swap_mb * 1024.0 * 1024.0) as u64,
            disk_free_bytes: (disk_gb * 1024.0 * 1024.0 * 1024.0) as u64,
            num_cores: 10,
        }
    }

    fn proc(pid: u32, ppid: u32, name: &str, cpu_pct: f64, rss_mb: u64) -> ProcessSample {
        ProcessSample {
            pid,
            ppid,
            uid: 501,
            name: name.to_string(),
            exe_path: String::new(),
            cpu_pct,
            rss_bytes: rss_mb * 1024 * 1024,
            is_stopped: false,
        }
    }

    #[test]
    fn protects_ancestors_and_exe_path_and_catches_sibling_workers() {
        let cfg = Config::default();
        let mut claude_proc = proc(150, 1, "2.1.327", 2.0, 300);
        claude_proc.exe_path = "/Users/u/.local/share/claude/versions/2.1.327".into();

        let procs = vec![
            proc(100, 1, "Terminal", 1.0, 50),
            proc(101, 100, "zsh", 0.1, 10),
            proc(102, 101, "resguard", 0.2, 4),
            claude_proc,
            proc(200, 101, "heavy_job", 82.0, 500),
        ];
        let prot = build_protected_pids(102, 501, &procs, &cfg);
        assert!(prot.contains(&100));
        assert!(prot.contains(&101));
        assert!(prot.contains(&102));
        assert!(prot.contains(&150));
        assert!(!prot.contains(&200));
    }

    #[test]
    fn soft_zone_sustained_demotes_and_releases_after_cooldown() {
        let cfg = Config::default();
        let mock = MockPlatform::new();
        let mut gov = Governor::new(102);
        let t0 = Instant::now();

        let procs = vec![
            proc(101, 1, "zsh", 0.1, 10),
            proc(102, 101, "resguard", 0.1, 4),
            proc(300, 101, "worker", 78.0, 300),
        ];

        // First tick above soft should wait for soft_sustain_ms (1000ms)
        gov.evaluate_tick(
            t0,
            &cfg,
            &base_metrics(80.0, 40.0, MemoryPressure::Normal, 0.0, 50.0),
            &procs,
            &mock,
        );
        assert_eq!(gov.demoted_count(), 0);

        // After sustain elapsed -> demote
        let t1 = t0 + Duration::from_millis(1100);
        let act1 = gov.evaluate_tick(
            t1,
            &cfg,
            &base_metrics(80.0, 40.0, MemoryPressure::Normal, 0.0, 50.0),
            &procs,
            &mock,
        );
        assert_eq!(act1.zone, Zone::Soft);
        assert_eq!(*mock.bg_calls.borrow(), vec![(300, true)]);
        assert_eq!(gov.demoted_count(), 1);

        // Drop below release (68%) but before cooldown (5s) -> keep demoted
        let t2 = t1 + Duration::from_secs(2);
        gov.evaluate_tick(
            t2,
            &cfg,
            &base_metrics(50.0, 40.0, MemoryPressure::Normal, 0.0, 50.0),
            &procs,
            &mock,
        );
        assert_eq!(gov.demoted_count(), 1);

        // After cooldown while worker is still busy on E-cores (10%) -> keep demoted to avoid ping-pong
        let t3 = t1 + Duration::from_secs(8);
        let procs_busy_ecore = vec![
            proc(101, 1, "zsh", 0.1, 10),
            proc(102, 101, "resguard", 0.1, 4),
            proc(300, 101, "worker", 10.0, 300),
        ];
        gov.evaluate_tick(
            t3,
            &cfg,
            &base_metrics(50.0, 40.0, MemoryPressure::Normal, 0.0, 50.0),
            &procs_busy_ecore,
            &mock,
        );
        assert_eq!(gov.demoted_count(), 1);

        // Once worker itself goes quiet (1.0%) -> restore normal QoS
        let t4 = t1 + Duration::from_secs(9);
        let procs_quiet = vec![
            proc(101, 1, "zsh", 0.1, 10),
            proc(102, 101, "resguard", 0.1, 4),
            proc(300, 101, "worker", 1.0, 300),
        ];
        gov.evaluate_tick(
            t4,
            &cfg,
            &base_metrics(50.0, 40.0, MemoryPressure::Normal, 0.0, 50.0),
            &procs_quiet,
            &mock,
        );
        assert_eq!(gov.demoted_count(), 0);
        assert_eq!(*mock.bg_calls.borrow(), vec![(300, true), (300, false)]);
    }

    #[test]
    fn high_ram_alone_does_not_demote_to_swap_throttled_bg() {
        let cfg = Config::default();
        let mock = MockPlatform::new();
        let mut gov = Governor::new(102);
        let t0 = Instant::now();
        let procs = vec![
            proc(102, 1, "resguard", 0.1, 4),
            proc(350, 1, "app", 2.0, 400),
        ];

        gov.evaluate_tick(
            t0,
            &cfg,
            &base_metrics(50.0, 90.0, MemoryPressure::Warning, 12000.0, 40.0),
            &procs,
            &mock,
        );
        assert_eq!(gov.demoted_count(), 0);
        assert!(mock.bg_calls.borrow().is_empty());
    }

    #[test]
    fn cap_triggers_duty_cycle_when_user_driven() {
        let cfg = Config::default();
        let mock = MockPlatform::new();
        let mut gov = Governor::new(102);
        let t0 = Instant::now();
        let procs = vec![
            proc(102, 1, "resguard", 0.1, 4),
            proc(400, 1, "encoder", 96.0, 400),
        ];

        let act = gov.evaluate_tick(
            t0,
            &cfg,
            &base_metrics(98.0, 50.0, MemoryPressure::Normal, 0.0, 50.0),
            &procs,
            &mock,
        );
        assert_eq!(act.zone, Zone::Cap);
        assert_eq!(act.duty_pause_pids, vec![400]);
        assert!(act.pause_duration.as_millis() >= 20);
        assert!(act.run_duration.as_millis() >= 20);
    }

    #[test]
    fn low_ssd_plus_swap_triggers_memory_emergency_and_cleanup_resumes() {
        let cfg = Config::default();
        let mock = MockPlatform::new();
        let mut gov = Governor::new(102);
        let t0 = Instant::now();
        let procs = vec![
            proc(102, 1, "resguard", 0.1, 4),
            proc(500, 1, "linker", 40.0, 3500),
        ];

        let act = gov.evaluate_tick(
            t0,
            &cfg,
            &base_metrics(60.0, 82.0, MemoryPressure::Normal, 3000.0, 4.0),
            &procs,
            &mock,
        );
        assert_eq!(act.zone, Zone::MemoryEmergency);
        assert_eq!(*mock.paused.borrow(), vec![500]);
        assert_eq!(gov.emergency_paused_count(), 1);

        gov.cleanup(&cfg, &mock);
        assert_eq!(*mock.resumed.borrow(), vec![500]);
        assert_eq!(gov.emergency_paused_count(), 0);
    }
}
