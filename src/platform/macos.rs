use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::process::Command;

use crate::model::{MemoryPressure, ProcessSample, SystemMetrics};
use crate::platform::PlatformBackend;

const KERN_SUCCESS: i32 = 0;
const HOST_CPU_LOAD_INFO: i32 = 3;
const HOST_CPU_LOAD_INFO_COUNT: u32 = 4;
const HOST_VM_INFO64: i32 = 4;
const HOST_VM_INFO64_COUNT: u32 = 104;

const PRIO_DARWIN_PROCESS: c_int = 4;
const PRIO_DARWIN_BG: c_int = 0x1000;

const PROC_PIDTASKALLINFO: c_int = 2;
const PROC_TASKALLINFO_SIZE: usize = 232;

const SSTOP: u32 = 4;
const SZOMB: u32 = 5;

const SIGSTOP: c_int = 17;
const SIGCONT: c_int = 19;

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct XswUsage {
    xsu_total: u64,
    xsu_avail: u64,
    xsu_used: u64,
    xsu_pagesize: u32,
    xsu_encrypted: u8,
    _pad: [u8; 3],
}

#[link(name = "System", kind = "framework")]
extern "C" {
    fn mach_host_self() -> u32;
    fn host_page_size(host: u32, page_size: *mut usize) -> i32;
    fn host_statistics64(
        host: u32,
        flavor: i32,
        host_info_out: *mut u32,
        host_info_out_cnt: *mut u32,
    ) -> i32;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;

    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *const c_void,
        newlen: usize,
    ) -> c_int;

    fn statvfs(path: *const c_char, buf: *mut u8) -> c_int;

    fn proc_listallpids(buffer: *mut c_void, buffersize: c_int) -> c_int;
    fn proc_pidinfo(
        pid: c_int,
        flavor: c_int,
        arg: u64,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    fn getloadavg(loadavg: *mut f64, nelem: c_int) -> c_int;

    fn setpriority(which: c_int, who: u32, prio: c_int) -> c_int;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn getuid() -> u32;
}

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT64_TYPE: isize = 4;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(
        main_port: u32,
        matching: *mut c_void,
        existing: *mut u32,
    ) -> i32;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegistryEntryCreateCFProperties(
        entry: u32,
        properties: *mut *mut c_void,
        allocator: *const c_void,
        options: u32,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFDictionaryGetValue(the_dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFNumberGetValue(number: *const c_void, the_type: isize, value_ptr: *mut c_void) -> u8;
    fn CFRelease(cf: *const c_void);
}

pub struct MacosBackend {
    host_port: u32,
    page_size: u64,
    ram_total_bytes: u64,
    num_cores: usize,
    uid: u32,
    timebase: MachTimebaseInfo,
    prev_cpu_ticks: Option<[u32; 4]>,
    last_cpu_pct: (f64, f64, f64),
    prev_proc_cpu_ticks: HashMap<u32, u64>,
    pid_path_cache: HashMap<u32, String>,
    pid_buf: Vec<i32>,
    cf_perf_key: *const c_void,
    cf_util_key: *const c_void,
}

unsafe impl Send for MacosBackend {}

impl Drop for MacosBackend {
    fn drop(&mut self) {
        unsafe {
            if !self.cf_perf_key.is_null() {
                CFRelease(self.cf_perf_key);
            }
            if !self.cf_util_key.is_null() {
                CFRelease(self.cf_util_key);
            }
        }
    }
}

impl MacosBackend {
    pub fn new() -> Result<Self, String> {
        let host_port = unsafe { mach_host_self() };

        let mut page_size_usize: usize = 0;
        let kr = unsafe { host_page_size(host_port, &mut page_size_usize) };
        if kr != KERN_SUCCESS || page_size_usize == 0 {
            return Err(format!("host_page_size failed: {}", kr));
        }

        let mut tb = MachTimebaseInfo::default();
        let kr_tb = unsafe { mach_timebase_info(&mut tb) };
        if kr_tb != KERN_SUCCESS || tb.denom == 0 {
            return Err(format!("mach_timebase_info failed: {}", kr_tb));
        }

        let ram_total_bytes = sysctl_u64(b"hw.memsize\0")?;
        let logical_cpus = sysctl_i32(b"hw.logicalcpu\0").unwrap_or(1).max(1) as usize;
        let uid = unsafe { getuid() };

        let cf_perf_key = unsafe {
            CFStringCreateWithCString(
                std::ptr::null(),
                b"PerformanceStatistics\0".as_ptr() as *const c_char,
                CF_STRING_ENCODING_UTF8,
            )
        };
        let cf_util_key = unsafe {
            CFStringCreateWithCString(
                std::ptr::null(),
                b"Device Utilization %\0".as_ptr() as *const c_char,
                CF_STRING_ENCODING_UTF8,
            )
        };

        let mut backend = Self {
            host_port,
            page_size: page_size_usize as u64,
            ram_total_bytes,
            num_cores: logical_cpus,
            uid,
            timebase: tb,
            prev_cpu_ticks: None,
            last_cpu_pct: (0.0, 0.0, 0.0),
            prev_proc_cpu_ticks: HashMap::with_capacity(512),
            pid_path_cache: HashMap::with_capacity(512),
            pid_buf: vec![0; 1024],
            cf_perf_key,
            cf_util_key,
        };

        // Prime baseline CPU ticks so the first real sample has an accurate delta.
        if let Ok(ticks) = backend.read_cpu_ticks() {
            backend.prev_cpu_ticks = Some(ticks);
        }

        Ok(backend)
    }

    fn read_cpu_ticks(&self) -> Result<[u32; 4], String> {
        let mut info = [0u32; 4];
        let mut count = HOST_CPU_LOAD_INFO_COUNT;
        let kr = unsafe {
            host_statistics64(
                self.host_port,
                HOST_CPU_LOAD_INFO,
                info.as_mut_ptr(),
                &mut count,
            )
        };
        if kr != KERN_SUCCESS {
            return Err(format!("host_statistics64(CPU) failed: {}", kr));
        }
        Ok(info)
    }

    fn read_ram_used_bytes(&self) -> Result<u64, String> {
        // Verified layout: sizeof(struct vm_statistics64) = 416 bytes (104 x u32)
        let mut buf = [0u32; HOST_VM_INFO64_COUNT as usize];
        let mut count = HOST_VM_INFO64_COUNT;
        let kr = unsafe {
            host_statistics64(
                self.host_port,
                HOST_VM_INFO64,
                buf.as_mut_ptr(),
                &mut count,
            )
        };
        if kr != KERN_SUCCESS {
            return Err(format!("host_statistics64(VM) failed: {}", kr));
        }
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 4)
        };
        // Verified byte offsets on arm64/x86_64 macOS SDK:
        // wire_count=12, purgeable_count=88, compressor_page_count=128, internal_page_count=140
        let wire = read_u32_le(bytes, 12) as u64;
        let purgeable = read_u32_le(bytes, 88) as u64;
        let compressor = read_u32_le(bytes, 128) as u64;
        let internal = read_u32_le(bytes, 140) as u64;

        let app_pages = internal.saturating_sub(purgeable);
        let used_pages = app_pages.saturating_add(wire).saturating_add(compressor);
        Ok(used_pages.saturating_mul(self.page_size))
    }

    fn ticks_to_ns(&self, ticks: u64) -> u64 {
        ((ticks as u128 * self.timebase.numer as u128) / self.timebase.denom as u128) as u64
    }

    fn read_gpu_pct(&self) -> Option<f64> {
        if self.cf_perf_key.is_null() || self.cf_util_key.is_null() {
            return None;
        }
        unsafe {
            let matching = IOServiceMatching(b"IOAccelerator\0".as_ptr() as *const c_char);
            if matching.is_null() {
                return None;
            }
            let mut iter: u32 = 0;
            // IOServiceGetMatchingServices consumes `matching`
            if IOServiceGetMatchingServices(0, matching, &mut iter) != KERN_SUCCESS {
                return None;
            }

            let mut best: Option<f64> = None;
            loop {
                let entry = IOIteratorNext(iter);
                if entry == 0 {
                    break;
                }
                let mut props: *mut c_void = std::ptr::null_mut();
                if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0)
                    == KERN_SUCCESS
                    && !props.is_null()
                {
                    let perf = CFDictionaryGetValue(props, self.cf_perf_key);
                    if !perf.is_null() && CFGetTypeID(perf) == CFDictionaryGetTypeID() {
                        let num = CFDictionaryGetValue(perf, self.cf_util_key);
                        if !num.is_null() && CFGetTypeID(num) == CFNumberGetTypeID() {
                            let mut val: i64 = -1;
                            if CFNumberGetValue(
                                num,
                                CF_NUMBER_SINT64_TYPE,
                                &mut val as *mut i64 as *mut c_void,
                            ) != 0
                                && val >= 0
                            {
                                let pct = (val as f64).clamp(0.0, 100.0);
                                best = Some(best.map_or(pct, |b| b.max(pct)));
                            }
                        }
                    }
                    CFRelease(props);
                }
                IOObjectRelease(entry);
            }
            IOObjectRelease(iter);
            best
        }
    }
}

impl PlatformBackend for MacosBackend {
    fn backend_name(&self) -> &'static str {
        "macos (Mach + libproc + PRIO_DARWIN_BG)"
    }

    fn num_cores(&self) -> usize {
        self.num_cores
    }

    fn current_uid(&self) -> u32 {
        self.uid
    }

    fn sample_system(&mut self) -> Result<SystemMetrics, String> {
        let cur_ticks = self.read_cpu_ticks()?;
        match self.prev_cpu_ticks {
            Some(prev) => {
                let d_user = cur_ticks[0].wrapping_sub(prev[0]) as f64;
                let d_sys = cur_ticks[1].wrapping_sub(prev[1]) as f64;
                let d_idle = cur_ticks[2].wrapping_sub(prev[2]) as f64;
                let d_nice = cur_ticks[3].wrapping_sub(prev[3]) as f64;
                let total = d_user + d_sys + d_idle + d_nice;
                if total > 0.0 {
                    let user_p = ((d_user + d_nice) / total) * 100.0;
                    let sys_p = (d_sys / total) * 100.0;
                    self.last_cpu_pct = ((user_p + sys_p).clamp(0.0, 100.0), user_p, sys_p);
                    self.prev_cpu_ticks = Some(cur_ticks);
                }
            }
            None => {
                self.prev_cpu_ticks = Some(cur_ticks);
            }
        }
        let (cpu_total_pct, cpu_user_pct, cpu_sys_pct) = self.last_cpu_pct;

        let ram_used_bytes = self.read_ram_used_bytes()?;
        let ram_used_pct = if self.ram_total_bytes > 0 {
            ((ram_used_bytes as f64 / self.ram_total_bytes as f64) * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };

        let swap = read_swap_usage().unwrap_or_default();
        let disk_free_bytes = read_root_free_bytes().unwrap_or(u64::MAX);
        let memory_pressure = read_memory_pressure();
        let load_1m = read_loadavg_1m();
        let gpu_pct = self.read_gpu_pct();

        Ok(SystemMetrics {
            cpu_total_pct,
            cpu_user_pct,
            cpu_sys_pct,
            gpu_pct,
            load_1m,
            ram_total_bytes: self.ram_total_bytes,
            ram_used_bytes,
            ram_used_pct,
            memory_pressure,
            swap_total_bytes: swap.xsu_total,
            swap_used_bytes: swap.xsu_used,
            disk_free_bytes,
            num_cores: self.num_cores,
        })
    }

    fn sample_processes(&mut self, elapsed_ns: u64, num_cores: usize) -> Vec<ProcessSample> {
        let count_bytes = unsafe { proc_listallpids(std::ptr::null_mut(), 0) };
        if count_bytes <= 0 {
            return Vec::new();
        }
        let needed = (count_bytes as usize) + 128;
        if self.pid_buf.len() < needed {
            self.pid_buf.resize(needed, 0);
        }

        let filled_count = unsafe {
            proc_listallpids(
                self.pid_buf.as_mut_ptr() as *mut c_void,
                (self.pid_buf.len() * std::mem::size_of::<i32>()) as c_int,
            )
        };
        if filled_count <= 0 {
            return Vec::new();
        }

        let n_pids = (filled_count as usize).min(self.pid_buf.len());
        let mut out = Vec::with_capacity(n_pids);
        let mut next_ticks = HashMap::with_capacity(n_pids);
        let mut next_paths = HashMap::with_capacity(n_pids);
        let denom_ns = (elapsed_ns as f64) * (num_cores.max(1) as f64);

        let mut info_buf = [0u8; PROC_TASKALLINFO_SIZE];

        for i in 0..n_pids {
            let pid = self.pid_buf[i];
            if pid <= 0 {
                continue;
            }
            let ret = unsafe {
                proc_pidinfo(
                    pid,
                    PROC_PIDTASKALLINFO,
                    0,
                    info_buf.as_mut_ptr() as *mut c_void,
                    PROC_TASKALLINFO_SIZE as c_int,
                )
            };
            if ret != PROC_TASKALLINFO_SIZE as c_int {
                continue;
            }

            // Verified offsets in proc_bsdinfo (starts at 0):
            // pbi_status=4, pbi_pid=12, pbi_ppid=16, pbi_uid=20, pbi_comm=48 (16B), pbi_name=64 (32B)
            let pbi_status = read_u32_le(&info_buf, 4);
            if pbi_status == SZOMB {
                continue;
            }
            let pbi_pid = read_u32_le(&info_buf, 12);
            let pbi_ppid = read_u32_le(&info_buf, 16);
            let pbi_uid = read_u32_le(&info_buf, 20);

            let name_long = read_cstr(&info_buf[64..96]);
            let name = if !name_long.is_empty() {
                name_long
            } else {
                read_cstr(&info_buf[48..64])
            };

            let exe_path = if self.uid == 0 || pbi_uid == self.uid {
                let p = if let Some(cached) = self.pid_path_cache.get(&pbi_pid) {
                    cached.clone()
                } else {
                    read_proc_path(pbi_pid)
                };
                next_paths.insert(pbi_pid, p.clone());
                p
            } else {
                String::new()
            };

            // proc_taskinfo starts at offset 136 (sizeof(proc_bsdinfo) = 136):
            // pti_resident_size = 136 + 8 = 144
            // pti_total_user    = 136 + 16 = 152
            // pti_total_system  = 136 + 24 = 160
            let rss_bytes = read_u64_le(&info_buf, 144);
            let total_user = read_u64_le(&info_buf, 152);
            let total_sys = read_u64_le(&info_buf, 160);
            let total_ticks = total_user.saturating_add(total_sys);

            next_ticks.insert(pbi_pid, total_ticks);

            let cpu_pct = if denom_ns > 0.0 {
                if let Some(&prev) = self.prev_proc_cpu_ticks.get(&pbi_pid) {
                    let delta_ticks = total_ticks.saturating_sub(prev);
                    let delta_ns = self.ticks_to_ns(delta_ticks) as f64;
                    ((delta_ns / denom_ns) * 100.0).clamp(0.0, 100.0)
                } else {
                    0.0
                }
            } else {
                0.0
            };

            out.push(ProcessSample {
                pid: pbi_pid,
                ppid: pbi_ppid,
                uid: pbi_uid,
                name,
                exe_path,
                cpu_pct,
                rss_bytes,
                is_stopped: pbi_status == SSTOP,
            });
        }

        self.prev_proc_cpu_ticks = next_ticks;
        self.pid_path_cache = next_paths;
        out
    }

    fn set_background(&self, pid: u32, enable: bool) -> Result<(), String> {
        let val = if enable { PRIO_DARWIN_BG } else { 0 };
        let rc = unsafe { setpriority(PRIO_DARWIN_PROCESS, pid, val) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("setpriority(pid={}, bg={}) failed: {}", pid, enable, err));
        }
        Ok(())
    }

    fn pause_process(&self, pid: u32) -> Result<(), String> {
        let rc = unsafe { kill(pid as c_int, SIGSTOP) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("kill({}, SIGSTOP) failed: {}", pid, err));
        }
        Ok(())
    }

    fn resume_process(&self, pid: u32) -> Result<(), String> {
        let rc = unsafe { kill(pid as c_int, SIGCONT) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("kill({}, SIGCONT) failed: {}", pid, err));
        }
        Ok(())
    }

    fn send_notification(&self, title: &str, body: &str) {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            esc(body),
            esc(title)
        );
        let _ = Command::new("osascript").arg("-e").arg(script).spawn();
    }
}

fn sysctl_u64(name: &[u8]) -> Result<u64, String> {
    let mut val: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let rc = unsafe {
        sysctlbyname(
            name.as_ptr() as *const c_char,
            &mut val as *mut u64 as *mut c_void,
            &mut len,
            std::ptr::null(),
            0,
        )
    };
    if rc != 0 {
        return Err(format!(
            "sysctlbyname failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(val)
}

fn sysctl_i32(name: &[u8]) -> Result<i32, String> {
    let mut val: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    let rc = unsafe {
        sysctlbyname(
            name.as_ptr() as *const c_char,
            &mut val as *mut i32 as *mut c_void,
            &mut len,
            std::ptr::null(),
            0,
        )
    };
    if rc != 0 {
        return Err(format!(
            "sysctlbyname failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(val)
}

fn read_swap_usage() -> Result<XswUsage, String> {
    let mut swap = XswUsage::default();
    let mut len = std::mem::size_of::<XswUsage>();
    let rc = unsafe {
        sysctlbyname(
            b"vm.swapusage\0".as_ptr() as *const c_char,
            &mut swap as *mut XswUsage as *mut c_void,
            &mut len,
            std::ptr::null(),
            0,
        )
    };
    if rc != 0 {
        return Err(format!(
            "sysctlbyname(vm.swapusage) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(swap)
}

fn read_root_free_bytes() -> Result<u64, String> {
    // Verified layout: sizeof(struct statvfs)=64, f_frsize at 8 (u64), f_bavail at 24 (u32)
    let mut buf = [0u8; 64];
    let rc = unsafe { statvfs(b"/\0".as_ptr() as *const c_char, buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(format!("statvfs(\"/\") failed: {}", std::io::Error::last_os_error()));
    }
    let f_frsize = read_u64_le(&buf, 8);
    let f_bavail = read_u32_le(&buf, 24) as u64;
    Ok(f_frsize.saturating_mul(f_bavail))
}

fn read_memory_pressure() -> MemoryPressure {
    match sysctl_i32(b"kern.memorystatus_vm_pressure_level\0") {
        Ok(lvl) if lvl >= 4 => MemoryPressure::Critical,
        Ok(lvl) if lvl >= 2 => MemoryPressure::Warning,
        _ => MemoryPressure::Normal,
    }
}

fn read_loadavg_1m() -> f64 {
    let mut avg = [0.0f64; 1];
    let rc = unsafe { getloadavg(avg.as_mut_ptr(), 1) };
    if rc == 1 {
        avg[0].max(0.0)
    } else {
        0.0
    }
}

fn read_proc_path(pid: u32) -> String {
    let mut buf = [0u8; 4096];
    let len = unsafe {
        proc_pidpath(
            pid as c_int,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
        )
    };
    if len > 0 {
        let n = (len as usize).min(buf.len());
        String::from_utf8_lossy(&buf[..n]).into_owned()
    } else {
        String::new()
    }
}

#[inline]
fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
}

#[inline]
fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
}

fn read_cstr(slice: &[u8]) -> String {
    let nul_pos = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    let c_bytes = &slice[..nul_pos];
    if c_bytes.is_empty() {
        return String::new();
    }
    // Safe fallback even if bytes aren't valid UTF-8
    CStr::from_bytes_with_nul(&[c_bytes, &[0]].concat())
        .ok()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from_utf8_lossy(c_bytes).into_owned())
}
