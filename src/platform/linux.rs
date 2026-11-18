use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_ulong, CString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::{MemoryPressure, ProcessSample, SystemMetrics};
use crate::platform::PlatformBackend;

const PRIO_PROCESS: c_int = 0;
const SIGSTOP: c_int = 19;
const SIGCONT: c_int = 18;
const _SC_CLK_TCK: c_int = 2;
const _SC_PAGESIZE: c_int = 30;

#[repr(C)]
struct StatVfs {
    f_bsize: c_ulong,
    f_frsize: c_ulong,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    _rest: [u64; 16],
}

extern "C" {
    fn statvfs(path: *const c_char, buf: *mut StatVfs) -> c_int;
    fn sysconf(name: c_int) -> i64;
    fn setpriority(which: c_int, who: u32, prio: c_int) -> c_int;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn getuid() -> u32;
}

pub struct LinuxBackend {
    uid: u32,
    num_cores: usize,
    clk_tck: u64,
    page_size: u64,
    prev_cpu: Option<(u64, u64, u64)>, // (user, sys, total)
    last_cpu_pct: (f64, f64, f64),
    prev_proc_ticks: HashMap<u32, u64>,
    pid_exe_cache: HashMap<u32, String>,
    cgroup_parent: Option<PathBuf>,
    cgroup_bg: Option<PathBuf>,
}

impl LinuxBackend {
    pub fn new() -> Result<Self, String> {
        let uid = unsafe { getuid() };
        let clk_tck = unsafe { sysconf(_SC_CLK_TCK) }.max(1) as u64;
        let page_size = unsafe { sysconf(_SC_PAGESIZE) }.max(4096) as u64;
        let num_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        let (cgroup_parent, cgroup_bg) = detect_or_create_bg_cgroup(uid);

        let mut b = Self {
            uid,
            num_cores,
            clk_tck,
            page_size,
            prev_cpu: None,
            last_cpu_pct: (0.0, 0.0, 0.0),
            prev_proc_ticks: HashMap::with_capacity(512),
            pid_exe_cache: HashMap::with_capacity(512),
            cgroup_parent,
            cgroup_bg,
        };
        if let Ok(t) = read_proc_stat_cpu() {
            b.prev_cpu = Some(t);
        }
        Ok(b)
    }
}

impl PlatformBackend for LinuxBackend {
    fn backend_name(&self) -> &'static str {
        if self.cgroup_bg.is_some() {
            "linux (/proc + cgroups v2 + SIGSTOP/SIGCONT)"
        } else {
            "linux (/proc + nice + SIGSTOP/SIGCONT)"
        }
    }

    fn num_cores(&self) -> usize {
        self.num_cores
    }

    fn current_uid(&self) -> u32 {
        self.uid
    }

    fn sample_system(&mut self) -> Result<SystemMetrics, String> {
        let cur = read_proc_stat_cpu()?;
        if let Some(prev) = self.prev_cpu {
            let d_user = cur.0.saturating_sub(prev.0) as f64;
            let d_sys = cur.1.saturating_sub(prev.1) as f64;
            let d_tot = cur.2.saturating_sub(prev.2) as f64;
            if d_tot > 0.0 {
                let u = (d_user / d_tot) * 100.0;
                let s = (d_sys / d_tot) * 100.0;
                self.last_cpu_pct = ((u + s).clamp(0.0, 100.0), u, s);
                self.prev_cpu = Some(cur);
            }
        } else {
            self.prev_cpu = Some(cur);
        }
        let (cpu_total_pct, cpu_user_pct, cpu_sys_pct) = self.last_cpu_pct;

        let (ram_total_bytes, ram_used_bytes, swap_total_bytes, swap_used_bytes) =
            read_meminfo()?;
        let ram_used_pct = if ram_total_bytes > 0 {
            ((ram_used_bytes as f64 / ram_total_bytes as f64) * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };

        let memory_pressure = read_psi_memory();
        let load_1m = read_loadavg();
        let gpu_pct = read_drm_gpu_pct();
        let disk_free_bytes = read_root_free_bytes().unwrap_or(u64::MAX);

        Ok(SystemMetrics {
            cpu_total_pct,
            cpu_user_pct,
            cpu_sys_pct,
            gpu_pct,
            load_1m,
            ram_total_bytes,
            ram_used_bytes,
            ram_used_pct,
            memory_pressure,
            swap_total_bytes,
            swap_used_bytes,
            disk_free_bytes,
            num_cores: self.num_cores,
        })
    }

    fn sample_processes(&mut self, elapsed_ns: u64, num_cores: usize) -> Vec<ProcessSample> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };

        let mut out = Vec::with_capacity(512);
        let mut next_ticks = HashMap::with_capacity(512);
        let mut next_exes = HashMap::with_capacity(512);
        let denom_ns = (elapsed_ns as f64) * (num_cores.max(1) as f64);
        let ns_per_tick = 1_000_000_000.0 / (self.clk_tck as f64);

        for entry in entries.flatten() {
            let fname = entry.file_name();
            let Some(pid_str) = fname.to_str() else {
                continue;
            };
            let Ok(pid) = pid_str.parse::<u32>() else {
                continue;
            };

            let stat_path = entry.path().join("stat");
            let Ok(stat_txt) = fs::read_to_string(&stat_path) else {
                continue;
            };
            let Some((name, rest)) = parse_proc_stat_head(&stat_txt) else {
                continue;
            };
            let fields: Vec<&str> = rest.split_whitespace().collect();
            if fields.len() < 22 {
                continue;
            }
            let state = fields[0];
            if state == "Z" {
                continue;
            }
            let ppid = fields[1].parse::<u32>().unwrap_or(0);
            let utime = fields[11].parse::<u64>().unwrap_or(0);
            let stime = fields[12].parse::<u64>().unwrap_or(0);
            let rss_pages = fields[21].parse::<u64>().unwrap_or(0);
            let total_ticks = utime.saturating_add(stime);

            let uid = read_proc_uid(&entry.path().join("status")).unwrap_or(u32::MAX);
            let exe_path = if self.uid == 0 || uid == self.uid {
                let p = if let Some(cached) = self.pid_exe_cache.get(&pid) {
                    cached.clone()
                } else {
                    fs::read_link(entry.path().join("exe"))
                        .map(|pb| pb.to_string_lossy().into_owned())
                        .unwrap_or_default()
                };
                next_exes.insert(pid, p.clone());
                p
            } else {
                String::new()
            };

            next_ticks.insert(pid, total_ticks);
            let cpu_pct = if denom_ns > 0.0 {
                if let Some(&prev) = self.prev_proc_ticks.get(&pid) {
                    let delta_ns = (total_ticks.saturating_sub(prev) as f64) * ns_per_tick;
                    ((delta_ns / denom_ns) * 100.0).clamp(0.0, 100.0)
                } else {
                    0.0
                }
            } else {
                0.0
            };

            out.push(ProcessSample {
                pid,
                ppid,
                uid,
                name,
                exe_path,
                cpu_pct,
                rss_bytes: rss_pages.saturating_mul(self.page_size),
                is_stopped: state == "T" || state == "t",
            });
        }

        self.prev_proc_ticks = next_ticks;
        self.pid_exe_cache = next_exes;
        out
    }

    fn set_background(&self, pid: u32, enable: bool) -> Result<(), String> {
        if let (Some(ref parent), Some(ref bg)) = (&self.cgroup_parent, &self.cgroup_bg) {
            let target = if enable { bg } else { parent };
            if fs::write(target.join("cgroup.procs"), format!("{}\n", pid)).is_ok() {
                return Ok(());
            }
        }
        let prio = if enable { 15 } else { 0 };
        let rc = unsafe { setpriority(PRIO_PROCESS, pid, prio) };
        if rc != 0 && enable {
            return Err(format!(
                "setpriority(pid={}, prio={}) failed: {}",
                pid,
                prio,
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn pause_process(&self, pid: u32) -> Result<(), String> {
        let rc = unsafe { kill(pid as c_int, SIGSTOP) };
        if rc != 0 {
            return Err(format!(
                "kill({}, SIGSTOP) failed: {}",
                pid,
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn resume_process(&self, pid: u32) -> Result<(), String> {
        let rc = unsafe { kill(pid as c_int, SIGCONT) };
        if rc != 0 {
            return Err(format!(
                "kill({}, SIGCONT) failed: {}",
                pid,
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn send_notification(&self, title: &str, body: &str) {
        let _ = std::process::Command::new("notify-send")
            .arg(title)
            .arg(body)
            .spawn();
    }
}

fn detect_or_create_bg_cgroup(uid: u32) -> (Option<PathBuf>, Option<PathBuf>) {
    let Ok(self_cg) = fs::read_to_string("/proc/self/cgroup") else {
        return (None, None);
    };
    let rel = self_cg
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .unwrap_or("")
        .trim();
    if rel.is_empty() {
        return (None, None);
    }
    let cur = PathBuf::from("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
    let Some(parent) = cur.parent() else {
        return (None, None);
    };
    let user_root = PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{}.slice", uid));
    let base = if parent.starts_with(&user_root) && is_writable_dir(parent) {
        parent.to_path_buf()
    } else if is_writable_dir(&cur) {
        cur
    } else {
        return (None, None);
    };

    let bg = base.join("resguard-bg.scope");
    if fs::create_dir_all(&bg).is_ok() {
        let _ = fs::write(bg.join("cpu.weight"), "10\n");
        let _ = fs::write(bg.join("io.weight"), "10\n");
        return (Some(base), Some(bg));
    }
    (None, None)
}

fn is_writable_dir(p: &Path) -> bool {
    p.join("cgroup.procs").exists()
        && fs::OpenOptions::new()
            .write(true)
            .open(p.join("cgroup.procs"))
            .is_ok()
}

fn read_proc_stat_cpu() -> Result<(u64, u64, u64), String> {
    let txt = fs::read_to_string("/proc/stat").map_err(|e| format!("/proc/stat: {}", e))?;
    let line = txt
        .lines()
        .find(|l| l.starts_with("cpu "))
        .ok_or("Missing cpu line in /proc/stat")?;
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();
    if vals.len() < 4 {
        return Err("Unexpected /proc/stat format".into());
    }
    let user = vals[0].saturating_add(vals[1]);
    let sys = vals[2]
        .saturating_add(*vals.get(5).unwrap_or(&0))
        .saturating_add(*vals.get(6).unwrap_or(&0));
    let idle = vals[3].saturating_add(*vals.get(4).unwrap_or(&0));
    let steal = *vals.get(7).unwrap_or(&0);
    let total = user + sys + idle + steal;
    Ok((user, sys + steal, total))
}

fn read_meminfo() -> Result<(u64, u64, u64, u64), String> {
    let txt = fs::read_to_string("/proc/meminfo").map_err(|e| format!("/proc/meminfo: {}", e))?;
    let mut mem_total = 0u64;
    let mut mem_avail = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;
    for line in txt.lines() {
        let mut it = line.split_whitespace();
        let Some(k) = it.next() else { continue };
        let Some(v) = it.next().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        match k {
            "MemTotal:" => mem_total = v * 1024,
            "MemAvailable:" => mem_avail = v * 1024,
            "SwapTotal:" => swap_total = v * 1024,
            "SwapFree:" => swap_free = v * 1024,
            _ => {}
        }
    }
    Ok((
        mem_total,
        mem_total.saturating_sub(mem_avail),
        swap_total,
        swap_total.saturating_sub(swap_free),
    ))
}

fn read_psi_memory() -> MemoryPressure {
    let Ok(txt) = fs::read_to_string("/proc/pressure/memory") else {
        return MemoryPressure::Normal;
    };
    let mut some10 = 0.0f64;
    let mut full10 = 0.0f64;
    for line in txt.lines() {
        for tok in line.split_whitespace() {
            if let Some(val) = tok.strip_prefix("avg10=").and_then(|v| v.parse::<f64>().ok()) {
                if line.starts_with("full ") {
                    full10 = val;
                } else if line.starts_with("some ") {
                    some10 = val;
                }
            }
        }
    }
    if full10 >= 5.0 {
        MemoryPressure::Critical
    } else if some10 >= 10.0 || full10 >= 1.0 {
        MemoryPressure::Warning
    } else {
        MemoryPressure::Normal
    }
}

fn read_loadavg() -> f64 {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn read_root_free_bytes() -> Result<u64, String> {
    let cpath = CString::new("/").unwrap();
    let mut buf: StatVfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { statvfs(cpath.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(format!("statvfs: {}", std::io::Error::last_os_error()));
    }
    Ok((buf.f_frsize as u64).saturating_mul(buf.f_bavail))
}

fn parse_proc_stat_head(line: &str) -> Option<(String, &str)> {
    let l = line.find('(')?;
    let r = line.rfind(')')?;
    if r <= l {
        return None;
    }
    Some((line[l + 1..r].to_string(), line[r + 1..].trim_start()))
}

fn read_proc_uid(status_path: &Path) -> Option<u32> {
    let txt = fs::read_to_string(status_path).ok()?;
    for line in txt.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            return rest.split_whitespace().next()?.parse::<u32>().ok();
        }
    }
    None
}

fn read_drm_gpu_pct() -> Option<f64> {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return None;
    };
    for entry in entries.flatten() {
        let p = entry.path().join("device/gpu_busy_percent");
        if let Ok(txt) = fs::read_to_string(&p) {
            if let Ok(v) = txt.trim().parse::<f64>() {
                return Some(v.clamp(0.0, 100.0));
            }
        }
    }
    None
}
