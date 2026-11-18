use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use crate::model::{MemoryPressure, ProcessSample, SystemMetrics};
use crate::platform::PlatformBackend;

type Handle = *mut c_void;
type Dword = u32;
type Bool = i32;

const INVALID_HANDLE_VALUE: Handle = (-1isize) as Handle;
const TH32CS_SNAPPROCESS: Dword = 0x0000_0002;

const PROCESS_TERMINATE: Dword = 0x0001;
const PROCESS_SET_INFORMATION: Dword = 0x0200;
const PROCESS_SET_QUOTA: Dword = 0x0100;
const PROCESS_SUSPEND_RESUME: Dword = 0x0800;
const PROCESS_QUERY_LIMITED_INFORMATION: Dword = 0x1000;

const NORMAL_PRIORITY_CLASS: Dword = 0x0000_0020;
const BELOW_NORMAL_PRIORITY_CLASS: Dword = 0x0000_4000;

const JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION_CLASS: i32 = 15;
const JOB_OBJECT_CPU_RATE_CONTROL_ENABLE: Dword = 0x1;
const JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP: Dword = 0x4;

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct FileTime {
    dw_low_date_time: Dword,
    dw_high_date_time: Dword,
}

impl FileTime {
    fn to_u64(self) -> u64 {
        ((self.dw_high_date_time as u64) << 32) | (self.dw_low_date_time as u64)
    }
}

#[repr(C)]
struct MemoryStatusEx {
    dw_length: Dword,
    dw_memory_load: Dword,
    ull_total_phys: u64,
    ull_avail_phys: u64,
    ull_total_page_file: u64,
    ull_avail_page_file: u64,
    ull_total_virtual: u64,
    ull_avail_virtual: u64,
    ull_avail_extended_virtual: u64,
}

#[repr(C)]
struct ProcessEntry32W {
    dw_size: Dword,
    cnt_usage: Dword,
    th32_process_id: Dword,
    th32_default_heap_id: usize,
    th32_module_id: Dword,
    cnt_threads: Dword,
    th32_parent_process_id: Dword,
    pc_pri_class_base: i32,
    dw_flags: Dword,
    sz_exe_file: [u16; 260],
}

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    cb: Dword,
    page_fault_count: Dword,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[repr(C)]
struct JobObjectCpuRateControlInformation {
    control_flags: Dword,
    cpu_rate: Dword, // in hundredths of a percent (7500 = 75.00%)
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcessId() -> Dword;
    fn GetSystemTimes(
        idle_time: *mut FileTime,
        kernel_time: *mut FileTime,
        user_time: *mut FileTime,
    ) -> Bool;
    fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> Bool;
    fn GetDiskFreeSpaceExW(
        directory_name: *const u16,
        free_bytes_available_to_caller: *mut u64,
        total_number_of_bytes: *mut u64,
        total_number_of_free_bytes: *mut u64,
    ) -> Bool;
    fn CreateToolhelp32Snapshot(flags: Dword, th32_process_id: Dword) -> Handle;
    fn Process32FirstW(snapshot: Handle, lppe: *mut ProcessEntry32W) -> Bool;
    fn Process32NextW(snapshot: Handle, lppe: *mut ProcessEntry32W) -> Bool;
    fn OpenProcess(desired_access: Dword, inherit_handle: Bool, process_id: Dword) -> Handle;
    fn CloseHandle(object: Handle) -> Bool;
    fn GetProcessTimes(
        process: Handle,
        creation_time: *mut FileTime,
        exit_time: *mut FileTime,
        kernel_time: *mut FileTime,
        user_time: *mut FileTime,
    ) -> Bool;
    fn QueryFullProcessImageNameW(
        process: Handle,
        flags: Dword,
        exe_name: *mut u16,
        size: *mut Dword,
    ) -> Bool;
    fn SetPriorityClass(process: Handle, priority_class: Dword) -> Bool;
    fn CreateJobObjectW(job_attributes: *mut c_void, name: *const u16) -> Handle;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
    fn SetInformationJobObject(
        job: Handle,
        info_class: i32,
        info: *const c_void,
        info_len: Dword,
    ) -> Bool;
    fn K32GetProcessMemoryInfo(
        process: Handle,
        ppsmem_counters: *mut ProcessMemoryCounters,
        cb: Dword,
    ) -> Bool;
}

#[link(name = "ntdll")]
extern "system" {
    fn NtSuspendProcess(process_handle: Handle) -> i32;
    fn NtResumeProcess(process_handle: Handle) -> i32;
}

struct SendHandle(Handle);
unsafe impl Send for SendHandle {}

pub struct WindowsBackend {
    num_cores: usize,
    self_pid: u32,
    prev_sys_times: Option<(u64, u64, u64)>, // (idle, kernel, user) in 100ns
    last_cpu_pct: (f64, f64, f64),
    prev_proc_time_100ns: HashMap<u32, u64>,
    pid_exe_cache: HashMap<u32, String>,
    job_handles: Mutex<HashMap<u32, SendHandle>>,
}

impl WindowsBackend {
    pub fn new() -> Result<Self, String> {
        let num_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let self_pid = unsafe { GetCurrentProcessId() };

        let mut b = Self {
            num_cores,
            self_pid,
            prev_sys_times: None,
            last_cpu_pct: (0.0, 0.0, 0.0),
            prev_proc_time_100ns: HashMap::with_capacity(512),
            pid_exe_cache: HashMap::with_capacity(512),
            job_handles: Mutex::new(HashMap::new()),
        };
        if let Ok(t) = read_sys_times() {
            b.prev_sys_times = Some(t);
        }
        Ok(b)
    }
}

impl Drop for WindowsBackend {
    fn drop(&mut self) {
        if let Ok(mut map) = self.job_handles.lock() {
            for (_, h) in map.drain() {
                if !h.0.is_null() {
                    unsafe { CloseHandle(h.0) };
                }
            }
        }
    }
}

impl PlatformBackend for WindowsBackend {
    fn backend_name(&self) -> &'static str {
        "windows (Win32 + Job Objects + NtSuspendProcess)"
    }

    fn num_cores(&self) -> usize {
        self.num_cores
    }

    fn current_uid(&self) -> u32 {
        0 // Ownership filtering is handled by OpenProcess permissions on Windows
    }

    fn sample_system(&mut self) -> Result<SystemMetrics, String> {
        let cur = read_sys_times()?;
        if let Some(prev) = self.prev_sys_times {
            let d_idle = cur.0.saturating_sub(prev.0) as f64;
            let d_kernel = cur.1.saturating_sub(prev.1) as f64;
            let d_user = cur.2.saturating_sub(prev.2) as f64;
            // On Windows, kernel time includes idle time
            let d_total = d_kernel + d_user;
            if d_total > 0.0 {
                let d_sys = (d_kernel - d_idle).max(0.0);
                let user_p = (d_user / d_total) * 100.0;
                let sys_p = (d_sys / d_total) * 100.0;
                self.last_cpu_pct = ((user_p + sys_p).clamp(0.0, 100.0), user_p, sys_p);
                self.prev_sys_times = Some(cur);
            }
        } else {
            self.prev_sys_times = Some(cur);
        }
        let (cpu_total_pct, cpu_user_pct, cpu_sys_pct) = self.last_cpu_pct;

        let mut mem = MemoryStatusEx {
            dw_length: std::mem::size_of::<MemoryStatusEx>() as Dword,
            dw_memory_load: 0,
            ull_total_phys: 0,
            ull_avail_phys: 0,
            ull_total_page_file: 0,
            ull_avail_page_file: 0,
            ull_total_virtual: 0,
            ull_avail_virtual: 0,
            ull_avail_extended_virtual: 0,
        };
        if unsafe { GlobalMemoryStatusEx(&mut mem) } == 0 {
            return Err("GlobalMemoryStatusEx failed".into());
        }

        let ram_total_bytes = mem.ull_total_phys;
        let ram_used_bytes = ram_total_bytes.saturating_sub(mem.ull_avail_phys);
        let ram_used_pct = if ram_total_bytes > 0 {
            ((ram_used_bytes as f64 / ram_total_bytes as f64) * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        let swap_total_bytes = mem.ull_total_page_file.saturating_sub(ram_total_bytes);
        let commit_used = mem.ull_total_page_file.saturating_sub(mem.ull_avail_page_file);
        let swap_used_bytes = commit_used.saturating_sub(ram_used_bytes);

        let memory_pressure = if ram_used_pct >= 93.0 {
            MemoryPressure::Critical
        } else if ram_used_pct >= 85.0 {
            MemoryPressure::Warning
        } else {
            MemoryPressure::Normal
        };

        let disk_free_bytes = read_system_drive_free_bytes().unwrap_or(u64::MAX);

        Ok(SystemMetrics {
            cpu_total_pct,
            cpu_user_pct,
            cpu_sys_pct,
            gpu_pct: None,
            load_1m: (cpu_total_pct / 100.0) * (self.num_cores as f64),
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
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return Vec::new();
        }

        let mut pe: ProcessEntry32W = unsafe { std::mem::zeroed() };
        pe.dw_size = std::mem::size_of::<ProcessEntry32W>() as Dword;

        let mut out = Vec::with_capacity(256);
        let mut next_times = HashMap::with_capacity(256);
        let mut next_exes = HashMap::with_capacity(256);
        let denom_ns = (elapsed_ns as f64) * (num_cores.max(1) as f64);

        let mut ok = unsafe { Process32FirstW(snap, &mut pe) };
        while ok != 0 {
            let pid = pe.th32_process_id;
            let ppid = pe.th32_parent_process_id;
            let name = utf16_z_to_string(&pe.sz_exe_file);

            if pid > 4 {
                let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
                if !h.is_null() {
                    let mut creation = FileTime::default();
                    let mut exit = FileTime::default();
                    let mut kernel = FileTime::default();
                    let mut user = FileTime::default();
                    let total_100ns =
                        if unsafe { GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) }
                            != 0
                        {
                            kernel.to_u64().saturating_add(user.to_u64())
                        } else {
                            0
                        };

                    let mut pmc = ProcessMemoryCounters {
                        cb: std::mem::size_of::<ProcessMemoryCounters>() as Dword,
                        ..Default::default()
                    };
                    let rss_bytes = if unsafe { K32GetProcessMemoryInfo(h, &mut pmc, pmc.cb) } != 0
                    {
                        pmc.working_set_size as u64
                    } else {
                        0
                    };

                    let exe_path = if let Some(cached) = self.pid_exe_cache.get(&pid) {
                        cached.clone()
                    } else {
                        read_exe_path_win(h)
                    };
                    next_exes.insert(pid, exe_path.clone());
                    unsafe { CloseHandle(h) };

                    next_times.insert(pid, total_100ns);
                    let cpu_pct = if denom_ns > 0.0 {
                        if let Some(&prev) = self.prev_proc_time_100ns.get(&pid) {
                            let delta_ns = total_100ns.saturating_sub(prev) as f64 * 100.0;
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
                        uid: 0,
                        name,
                        exe_path,
                        cpu_pct,
                        rss_bytes,
                        is_stopped: false,
                    });
                } else if pid == self.self_pid {
                    out.push(ProcessSample {
                        pid,
                        ppid,
                        uid: 0,
                        name,
                        exe_path: String::new(),
                        cpu_pct: 0.0,
                        rss_bytes: 0,
                        is_stopped: false,
                    });
                }
            }

            ok = unsafe { Process32NextW(snap, &mut pe) };
        }

        unsafe { CloseHandle(snap) };
        self.prev_proc_time_100ns = next_times;
        self.pid_exe_cache = next_exes;

        // Prune dead job handles
        if let Ok(mut jobs) = self.job_handles.lock() {
            jobs.retain(|pid, h| {
                let alive = self.prev_proc_time_100ns.contains_key(pid);
                if !alive && !h.0.is_null() {
                    unsafe { CloseHandle(h.0) };
                }
                alive
            });
        }

        out
    }

    fn set_background(&self, pid: u32, enable: bool) -> Result<(), String> {
        let h = unsafe {
            OpenProcess(
                PROCESS_SET_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if h.is_null() {
            return Err(format!("OpenProcess(pid={}) failed", pid));
        }

        let prio = if enable {
            BELOW_NORMAL_PRIORITY_CLASS
        } else {
            NORMAL_PRIORITY_CLASS
        };
        let prio_ok = unsafe { SetPriorityClass(h, prio) } != 0;

        // Also apply reversible Win32 Job Object CPU rate cap (70% hard cap when demoted)
        if let Ok(mut jobs) = self.job_handles.lock() {
            if enable {
                let job = if let Some(existing) = jobs.get(&pid) {
                    existing.0
                } else {
                    let j = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
                    if !j.is_null() {
                        if unsafe { AssignProcessToJobObject(j, h) } != 0 {
                            jobs.insert(pid, SendHandle(j));
                            j
                        } else {
                            unsafe { CloseHandle(j) };
                            std::ptr::null_mut()
                        }
                    } else {
                        std::ptr::null_mut()
                    }
                };
                if !job.is_null() {
                    let info = JobObjectCpuRateControlInformation {
                        control_flags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE
                            | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
                        cpu_rate: 7000, // 70.00% hard cap
                    };
                    unsafe {
                        SetInformationJobObject(
                            job,
                            JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION_CLASS,
                            &info as *const _ as *const c_void,
                            std::mem::size_of::<JobObjectCpuRateControlInformation>() as Dword,
                        );
                    }
                }
            } else if let Some(existing) = jobs.get(&pid) {
                let info = JobObjectCpuRateControlInformation {
                    control_flags: 0,
                    cpu_rate: 0,
                };
                unsafe {
                    SetInformationJobObject(
                        existing.0,
                        JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION_CLASS,
                        &info as *const _ as *const c_void,
                        std::mem::size_of::<JobObjectCpuRateControlInformation>() as Dword,
                    );
                }
            }
        }

        unsafe { CloseHandle(h) };
        if !prio_ok {
            return Err(format!("SetPriorityClass(pid={}) failed", pid));
        }
        Ok(())
    }

    fn pause_process(&self, pid: u32) -> Result<(), String> {
        let h = unsafe { OpenProcess(PROCESS_SUSPEND_RESUME, 0, pid) };
        if h.is_null() {
            return Err(format!("OpenProcess(SUSPEND, pid={}) failed", pid));
        }
        let status = unsafe { NtSuspendProcess(h) };
        unsafe { CloseHandle(h) };
        if status != 0 {
            return Err(format!("NtSuspendProcess(pid={}) status={}", pid, status));
        }
        Ok(())
    }

    fn resume_process(&self, pid: u32) -> Result<(), String> {
        let h = unsafe { OpenProcess(PROCESS_SUSPEND_RESUME, 0, pid) };
        if h.is_null() {
            return Err(format!("OpenProcess(RESUME, pid={}) failed", pid));
        }
        let status = unsafe { NtResumeProcess(h) };
        unsafe { CloseHandle(h) };
        if status != 0 {
            return Err(format!("NtResumeProcess(pid={}) status={}", pid, status));
        }
        Ok(())
    }

    fn send_notification(&self, _title: &str, _body: &str) {}
}

fn read_sys_times() -> Result<(u64, u64, u64), String> {
    let mut idle = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return Err("GetSystemTimes failed".into());
    }
    Ok((idle.to_u64(), kernel.to_u64(), user.to_u64()))
}

fn read_system_drive_free_bytes() -> Result<u64, String> {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let mut wide: Vec<u16> = format!("{}\\", drive).encode_utf16().collect();
    wide.push(0);
    let mut free_avail: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_avail,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err("GetDiskFreeSpaceExW failed".into());
    }
    Ok(free_avail)
}

fn read_exe_path_win(h: Handle) -> String {
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as Dword;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) };
    if ok != 0 && len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

fn utf16_z_to_string(slice: &[u16]) -> String {
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    String::from_utf16_lossy(&slice[..end])
}
