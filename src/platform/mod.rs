use crate::model::{ProcessSample, SystemMetrics};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod stub;

pub trait PlatformBackend {
    fn backend_name(&self) -> &'static str;
    fn num_cores(&self) -> usize;
    fn current_uid(&self) -> u32;
    fn sample_system(&mut self) -> Result<SystemMetrics, String>;
    fn sample_processes(&mut self, elapsed_ns: u64, num_cores: usize) -> Vec<ProcessSample>;
    fn set_background(&self, pid: u32, enable: bool) -> Result<(), String>;
    fn pause_process(&self, pid: u32) -> Result<(), String>;
    fn resume_process(&self, pid: u32) -> Result<(), String>;
    fn send_notification(&self, title: &str, body: &str);
}

pub fn create_backend() -> Result<Box<dyn PlatformBackend>, String> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacosBackend::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxBackend::new()?))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsBackend::new()?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Ok(Box::new(stub::StubBackend::new()))
    }
}
