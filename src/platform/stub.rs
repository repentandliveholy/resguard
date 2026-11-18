use crate::model::{ProcessSample, SystemMetrics};
use crate::platform::PlatformBackend;

/// Cross-platform placeholder backend for non-macOS targets.
///
/// Implementation roadmap:
/// - Linux: `/proc/stat`, `/proc/meminfo`, `/proc/<pid>/stat`, `statvfs("/")`,
///   cgroups v2 (`cpu.max`, `memory.high`, `memory.max`), `SIGSTOP`/`SIGCONT`.
/// - Windows: `GetSystemTimes`, `GlobalMemoryStatusEx`, `GetDiskFreeSpaceExW`,
///   Win32 Job Objects (`JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`), `NtSuspendProcess`.
pub struct StubBackend;

impl StubBackend {
    pub fn new() -> Self {
        Self
    }
}

impl PlatformBackend for StubBackend {
    fn backend_name(&self) -> &'static str {
        "stub (unsupported OS — implement Linux cgroups v2 or Windows Job Objects here)"
    }

    fn num_cores(&self) -> usize {
        1
    }

    fn current_uid(&self) -> u32 {
        0
    }

    fn sample_system(&mut self) -> Result<SystemMetrics, String> {
        Err("Platform backend not implemented for this OS yet".into())
    }

    fn sample_processes(&mut self, _elapsed_ns: u64, _num_cores: usize) -> Vec<ProcessSample> {
        Vec::new()
    }

    fn set_background(&self, _pid: u32, _enable: bool) -> Result<(), String> {
        Err("Not implemented on this OS".into())
    }

    fn pause_process(&self, _pid: u32) -> Result<(), String> {
        Err("Not implemented on this OS".into())
    }

    fn resume_process(&self, _pid: u32) -> Result<(), String> {
        Err("Not implemented on this OS".into())
    }

    fn send_notification(&self, _title: &str, _body: &str) {}
}
