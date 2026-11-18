#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPressure {
    Normal,
    Warning,
    Critical,
}

impl std::fmt::Display for MemoryPressure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryPressure::Normal => write!(f, "NORM"),
            MemoryPressure::Warning => write!(f, "WARN"),
            MemoryPressure::Critical => write!(f, "CRIT"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SystemMetrics {
    pub cpu_total_pct: f64,
    pub cpu_user_pct: f64,
    pub cpu_sys_pct: f64,
    pub gpu_pct: Option<f64>,
    pub load_1m: f64,
    pub ram_total_bytes: u64,
    pub ram_used_bytes: u64,
    pub ram_used_pct: f64,
    pub memory_pressure: MemoryPressure,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub disk_free_bytes: u64,
    pub num_cores: usize,
}

impl SystemMetrics {
    pub fn disk_free_gb(&self) -> f64 {
        self.disk_free_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    pub fn swap_used_mb(&self) -> f64 {
        self.swap_used_bytes as f64 / (1024.0 * 1024.0)
    }

    pub fn swap_total_mb(&self) -> f64 {
        self.swap_total_bytes as f64 / (1024.0 * 1024.0)
    }

    pub fn ram_used_gb(&self) -> f64 {
        self.ram_used_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    pub fn ram_total_gb(&self) -> f64 {
        self.ram_total_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }
}

#[derive(Debug, Clone)]
pub struct ProcessSample {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub name: String,
    pub exe_path: String,
    /// CPU usage normalized to [0.0 .. 100.0] across total machine capacity.
    pub cpu_pct: f64,
    pub rss_bytes: u64,
    pub is_stopped: bool,
}

impl ProcessSample {
    pub fn rss_mb(&self) -> f64 {
        self.rss_bytes as f64 / (1024.0 * 1024.0)
    }
}
