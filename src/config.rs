use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub cpu_soft: f64,
    pub cpu_high: f64,
    pub cpu_cap: f64,
    pub cpu_release: f64,
    pub soft_sustain_ms: u64,
    pub release_cooldown_ms: u64,
    pub ram_high_pct: f64,
    pub ram_critical_pct: f64,
    pub disk_min_free_gb: f64,
    pub swap_warn_mb: f64,
    pub tick_ms: u64,
    pub min_proc_cpu_pct: f64,
    pub min_proc_rss_mb: f64,
    pub dry_run: bool,
    pub once: bool,
    pub notify: bool,
    pub quiet: bool,
    pub auto_update: bool,
    pub auto_update_interval_hours: u64,
    pub color: ColorMode,
    pub log_file: Option<String>,
    pub protect_names: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cpu_soft: 75.0,
            cpu_high: 95.0,
            cpu_cap: 97.0,
            cpu_release: 68.0,
            soft_sustain_ms: 1000,
            release_cooldown_ms: 5000,
            ram_high_pct: 90.0,
            ram_critical_pct: 95.0,
            disk_min_free_gb: 8.0,
            swap_warn_mb: 2048.0,
            tick_ms: 500,
            min_proc_cpu_pct: 8.0,
            min_proc_rss_mb: 256.0,
            dry_run: false,
            once: false,
            notify: true,
            quiet: false,
            auto_update: true,
            auto_update_interval_hours: 24,
            color: ColorMode::Auto,
            log_file: None,
            protect_names: default_protected_names(),
        }
    }
}

pub fn default_protected_names() -> Vec<String> {
    [
        // Kernel & core OS services (macOS / Linux / Windows generic names)
        "kernel_task",
        "launchd",
        "systemd",
        "init",
        "WindowServer",
        "loginwindow",
        "Finder",
        "Dock",
        "SystemUIServer",
        "ControlCenter",
        "NotificationCenter",
        "UserEventAgent",
        "coreaudiod",
        "audiomxd",
        "pulseaudio",
        "pipewire",
        "bluetoothd",
        "airportd",
        "powerd",
        "thermald",
        "syslogd",
        "notifyd",
        "distnoted",
        "cfprefsd",
        "trustd",
        "securityd",
        "sshd",
        "Activity Monitor",
        "Stats",
        "BetterDisplay",
        "resguard",
        "rs-dash",
        "rs-tui",
        "RS Dash",
        "Resguard Dashboard",
        // Interactive shells & terminals (prevent locking out user input)
        "Terminal",
        "iTerm2",
        "Alacritty",
        "kitty",
        "WezTerm",
        "wezterm-gui",
        "Ghostty",
        "WindowsTerminal",
        "conhost",
        "cmd",
        "powershell",
        "pwsh",
        "zsh",
        "bash",
        "fish",
        "tmux",
        "screen",
        // Windows core OS services
        "System",
        "Registry",
        "smss",
        "csrss",
        "wininit",
        "services",
        "lsass",
        "svchost",
        "dwm",
        "explorer",
        "winlogon",
        "audiodg",
        // Editors / IDEs / main browser & UI apps (worker tabs/builds can still be governed)
        "Cursor",
        "Code",
        "Xcode",
        "zed",
        "Google Chrome",
        "Safari",
        "Arc",
        "firefox",
        "claude",
        "agy",
        // Real-time calls
        "zoom.us",
        "FaceTime",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn default_config_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(|h| PathBuf::from(h).join(".config/resguard/config.toml"))
}

pub enum CliAction {
    Run(Config),
    RunTui(Config),
    CheckUpdate,
    SelfUpdate { force: bool },
    InstallService,
    UninstallService,
    ServiceStatus,
    ExitOk,
}

pub fn parse_args_and_load() -> Result<CliAction, String> {
    let args: Vec<String> = env::args().skip(1).collect();

    let mut config_path: Option<PathBuf> = default_config_path();
    let mut explicit_config = false;
    let mut init_config = false;

    // First pass: check help/version/config path/init-config
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(CliAction::ExitOk);
            }
            "-V" | "--version" => {
                println!("resguard {}", env!("CARGO_PKG_VERSION"));
                return Ok(CliAction::ExitOk);
            }
            "--init-config" => {
                init_config = true;
            }
            "--check-update" | "check-update" => return Ok(CliAction::CheckUpdate),
            "--update" | "update" => return Ok(CliAction::SelfUpdate { force: false }),
            "--force-update" => return Ok(CliAction::SelfUpdate { force: true }),
            "--install-service" | "install-service" => return Ok(CliAction::InstallService),
            "--uninstall-service" | "uninstall-service" => return Ok(CliAction::UninstallService),
            "--service-status" | "service-status" => return Ok(CliAction::ServiceStatus),
            "--config" => {
                i += 1;
                let p = args.get(i).ok_or("--config requires a path")?;
                config_path = Some(PathBuf::from(p));
                explicit_config = true;
            }
            _ => {}
        }
        i += 1;
    }

    if init_config {
        let path = config_path.ok_or("Cannot determine HOME for default config path")?;
        write_default_config(&path)?;
        println!("Wrote default config to {}", path.display());
        return Ok(CliAction::ExitOk);
    }

    let mut cfg = Config::default();
    if let Some(ref path) = config_path {
        if path.exists() {
            let text = fs::read_to_string(path)
                .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
            apply_toml_text(&mut cfg, &text)?;
        } else if explicit_config {
            return Err(format!("Config file not found: {}", path.display()));
        }
    }

    // Second pass: CLI overrides
    let mut tui_mode = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--config" => {
                i += 1;
            }
            "--tui" | "-t" | "--dash" => tui_mode = true,
            "--dry-run" => cfg.dry_run = true,
            "--once" => cfg.once = true,
            "--no-notify" => cfg.notify = false,
            "--quiet" | "-q" => cfg.quiet = true,
            "--auto-update" => cfg.auto_update = true,
            "--no-auto-update" => cfg.auto_update = false,
            "--color" => cfg.color = ColorMode::Always,
            "--no-color" => cfg.color = ColorMode::Never,
            "--log-file" => {
                i += 1;
                let p = args.get(i).ok_or("--log-file requires a path")?;
                cfg.log_file = Some(p.clone());
            }
            "--soft" => {
                i += 1;
                cfg.cpu_soft = parse_f64(&args, i, "--soft")?;
            }
            "--high" => {
                i += 1;
                cfg.cpu_high = parse_f64(&args, i, "--high")?;
            }
            "--cap" => {
                i += 1;
                cfg.cpu_cap = parse_f64(&args, i, "--cap")?;
            }
            "--release" => {
                i += 1;
                cfg.cpu_release = parse_f64(&args, i, "--release")?;
            }
            "--ram-high" => {
                i += 1;
                cfg.ram_high_pct = parse_f64(&args, i, "--ram-high")?;
            }
            "--ram-crit" => {
                i += 1;
                cfg.ram_critical_pct = parse_f64(&args, i, "--ram-crit")?;
            }
            "--disk-min-gb" => {
                i += 1;
                cfg.disk_min_free_gb = parse_f64(&args, i, "--disk-min-gb")?;
            }
            "--swap-warn-mb" => {
                i += 1;
                cfg.swap_warn_mb = parse_f64(&args, i, "--swap-warn-mb")?;
            }
            "--tick-ms" => {
                i += 1;
                cfg.tick_ms = parse_u64(&args, i, "--tick-ms")?;
            }
            "--min-proc-cpu" => {
                i += 1;
                cfg.min_proc_cpu_pct = parse_f64(&args, i, "--min-proc-cpu")?;
            }
            "--protect" => {
                i += 1;
                let val = args.get(i).ok_or("--protect requires a process name")?;
                for name in val.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    if !cfg.protect_names.iter().any(|p| p.eq_ignore_ascii_case(name)) {
                        cfg.protect_names.push(name.to_string());
                    }
                }
            }
            other => {
                return Err(format!("Unknown argument: {} (try --help)", other));
            }
        }
        i += 1;
    }

    validate_config(&cfg)?;
    if tui_mode {
        Ok(CliAction::RunTui(cfg))
    } else {
        Ok(CliAction::Run(cfg))
    }
}

fn parse_f64(args: &[String], idx: usize, flag: &str) -> Result<f64, String> {
    let s = args
        .get(idx)
        .ok_or_else(|| format!("{} requires a numeric value", flag))?;
    s.parse::<f64>()
        .map_err(|_| format!("Invalid number for {}: {}", flag, s))
}

fn parse_u64(args: &[String], idx: usize, flag: &str) -> Result<u64, String> {
    let s = args
        .get(idx)
        .ok_or_else(|| format!("{} requires an integer value", flag))?;
    s.parse::<u64>()
        .map_err(|_| format!("Invalid integer for {}: {}", flag, s))
}

pub fn validate_config(cfg: &Config) -> Result<(), String> {
    if !(1.0..=100.0).contains(&cfg.cpu_soft)
        || !(1.0..=100.0).contains(&cfg.cpu_high)
        || !(1.0..=100.0).contains(&cfg.cpu_cap)
    {
        return Err("CPU thresholds must be in [1.0, 100.0]".into());
    }
    if cfg.cpu_release >= cfg.cpu_soft {
        return Err("cpu_release must be strictly less than cpu_soft for hysteresis".into());
    }
    if cfg.cpu_soft > cfg.cpu_high || cfg.cpu_high > cfg.cpu_cap {
        return Err("Expected cpu_soft <= cpu_high <= cpu_cap".into());
    }
    if !(10.0..=99.5).contains(&cfg.ram_high_pct)
        || !(10.0..=99.9).contains(&cfg.ram_critical_pct)
    {
        return Err("RAM thresholds must be in [10.0, 99.9]".into());
    }
    if cfg.ram_high_pct > cfg.ram_critical_pct {
        return Err("Expected ram_high_pct <= ram_critical_pct".into());
    }
    if cfg.tick_ms < 50 || cfg.tick_ms > 10_000 {
        return Err("tick_ms must be between 50 and 10000".into());
    }
    Ok(())
}

pub fn apply_toml_text(cfg: &mut Config, content: &str) -> Result<(), String> {
    for (line_no, raw) in content.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("Invalid config syntax at line {}", line_no + 1));
        };
        let key = k.trim();
        let val = v.trim();
        match key {
            "cpu_soft" => cfg.cpu_soft = val.parse().map_err(|_| bad_val(key, val))?,
            "cpu_high" => cfg.cpu_high = val.parse().map_err(|_| bad_val(key, val))?,
            "cpu_cap" => cfg.cpu_cap = val.parse().map_err(|_| bad_val(key, val))?,
            "cpu_release" => cfg.cpu_release = val.parse().map_err(|_| bad_val(key, val))?,
            "soft_sustain_ms" => {
                cfg.soft_sustain_ms = val.parse().map_err(|_| bad_val(key, val))?
            }
            "release_cooldown_ms" => {
                cfg.release_cooldown_ms = val.parse().map_err(|_| bad_val(key, val))?
            }
            "ram_high_pct" => cfg.ram_high_pct = val.parse().map_err(|_| bad_val(key, val))?,
            "ram_critical_pct" => {
                cfg.ram_critical_pct = val.parse().map_err(|_| bad_val(key, val))?
            }
            "disk_min_free_gb" => {
                cfg.disk_min_free_gb = val.parse().map_err(|_| bad_val(key, val))?
            }
            "swap_warn_mb" => cfg.swap_warn_mb = val.parse().map_err(|_| bad_val(key, val))?,
            "tick_ms" => cfg.tick_ms = val.parse().map_err(|_| bad_val(key, val))?,
            "min_proc_cpu_pct" => {
                cfg.min_proc_cpu_pct = val.parse().map_err(|_| bad_val(key, val))?
            }
            "min_proc_rss_mb" => {
                cfg.min_proc_rss_mb = val.parse().map_err(|_| bad_val(key, val))?
            }
            "dry_run" => cfg.dry_run = val.parse().map_err(|_| bad_val(key, val))?,
            "notify" => cfg.notify = val.parse().map_err(|_| bad_val(key, val))?,
            "quiet" => cfg.quiet = val.parse().map_err(|_| bad_val(key, val))?,
            "auto_update" => cfg.auto_update = val.parse().map_err(|_| bad_val(key, val))?,
            "auto_update_interval_hours" => {
                cfg.auto_update_interval_hours = val.parse().map_err(|_| bad_val(key, val))?
            }
            "log_file" => {
                let s = val
                    .strip_prefix('"')
                    .and_then(|x| x.strip_suffix('"'))
                    .unwrap_or(val)
                    .trim();
                if !s.is_empty() {
                    cfg.log_file = Some(s.to_string());
                }
            }
            "protect_extra" => {
                for item in parse_string_list(val)? {
                    if !cfg.protect_names.iter().any(|p| p.eq_ignore_ascii_case(&item)) {
                        cfg.protect_names.push(item);
                    }
                }
            }
            "protect_names" => {
                cfg.protect_names = parse_string_list(val)?;
            }
            _ => {
                return Err(format!("Unknown config key '{}' at line {}", key, line_no + 1));
            }
        }
    }
    Ok(())
}

fn bad_val(key: &str, val: &str) -> String {
    format!("Invalid value for {}: {}", key, val)
}

fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn parse_string_list(val: &str) -> Result<Vec<String>, String> {
    let trimmed = val.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("Expected array [...] got: {}", val))?;
    let mut out = Vec::new();
    for part in inner.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let unquoted = p
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| p.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(p);
        if !unquoted.is_empty() {
            out.push(unquoted.to_string());
        }
    }
    Ok(out)
}

pub fn write_default_config(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }
    let body = r#"# resguard configuration
# CPU thresholds (% of total machine capacity)
cpu_release = 68.0
cpu_soft = 75.0
cpu_high = 95.0
cpu_cap = 97.0
soft_sustain_ms = 1000
release_cooldown_ms = 5000

# RAM, Swap & Disk emergency guard (prevents swap from filling low SSD)
ram_high_pct = 90.0
ram_critical_pct = 95.0
disk_min_free_gb = 8.0
swap_warn_mb = 2048.0

# Control loop interval (ms) & minimum consumer filter
tick_ms = 500
min_proc_cpu_pct = 8.0
min_proc_rss_mb = 256.0

# Behavior
dry_run = false
notify = true
quiet = false
# log_file = "/tmp/resguard.log"

# Extra process names to never throttle or pause
protect_extra = ["Docker", "Ollama"]
"#;
    fs::write(path, body).map_err(|e| format!("Failed to write {}: {}", path.display(), e))
}

pub fn load_config_or_default(path: Option<&Path>) -> Result<Config, String> {
    let mut cfg = Config::default();
    let owned_default;
    let target = match path {
        Some(p) => Some(p),
        None => {
            owned_default = default_config_path();
            owned_default.as_deref()
        }
    };
    if let Some(p) = target {
        if p.exists() {
            let text = fs::read_to_string(p)
                .map_err(|e| format!("Failed to read {}: {}", p.display(), e))?;
            apply_toml_text(&mut cfg, &text)?;
            validate_config(&cfg)?;
        }
    }
    Ok(cfg)
}

pub fn save_config(path: &Path, cfg: &Config) -> Result<(), String> {
    validate_config(cfg)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }

    let defaults = default_protected_names();
    let all_defaults_present = defaults
        .iter()
        .all(|d| cfg.protect_names.iter().any(|p| p.eq_ignore_ascii_case(d)));

    let protect_line = if all_defaults_present {
        let extras: Vec<String> = cfg
            .protect_names
            .iter()
            .filter(|p| !defaults.iter().any(|d| d.eq_ignore_ascii_case(p)))
            .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
            .collect();
        format!("protect_extra = [{}]", extras.join(", "))
    } else {
        let all: Vec<String> = cfg
            .protect_names
            .iter()
            .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
            .collect();
        format!("protect_names = [{}]", all.join(", "))
    };

    let log_line = match &cfg.log_file {
        Some(p) if !p.trim().is_empty() => format!("log_file = \"{}\"", p.replace('"', "\\\"")),
        _ => "# log_file = \"/tmp/resguard.log\"".to_string(),
    };

    let body = format!(
        "# resguard configuration\n\
# CPU thresholds (% of total machine capacity)\n\
cpu_release = {cpu_release:.1}\n\
cpu_soft = {cpu_soft:.1}\n\
cpu_high = {cpu_high:.1}\n\
cpu_cap = {cpu_cap:.1}\n\
soft_sustain_ms = {soft_sustain_ms}\n\
release_cooldown_ms = {release_cooldown_ms}\n\
\n\
# RAM, Swap & Disk emergency guard (prevents swap from filling low SSD)\n\
ram_high_pct = {ram_high_pct:.1}\n\
ram_critical_pct = {ram_critical_pct:.1}\n\
disk_min_free_gb = {disk_min_free_gb:.1}\n\
swap_warn_mb = {swap_warn_mb:.1}\n\
\n\
# Control loop interval (ms) & minimum consumer filter\n\
tick_ms = {tick_ms}\n\
min_proc_cpu_pct = {min_proc_cpu_pct:.1}\n\
min_proc_rss_mb = {min_proc_rss_mb:.1}\n\
\n\
# Behavior\n\
dry_run = {dry_run}\n\
notify = {notify}\n\
quiet = {quiet}\n\
auto_update = {auto_update}\n\
auto_update_interval_hours = {auto_update_interval_hours}\n\
{log_line}\n\
\n\
# Protected process names\n\
{protect_line}\n",
        cpu_release = cfg.cpu_release,
        cpu_soft = cfg.cpu_soft,
        cpu_high = cfg.cpu_high,
        cpu_cap = cfg.cpu_cap,
        soft_sustain_ms = cfg.soft_sustain_ms,
        release_cooldown_ms = cfg.release_cooldown_ms,
        ram_high_pct = cfg.ram_high_pct,
        ram_critical_pct = cfg.ram_critical_pct,
        disk_min_free_gb = cfg.disk_min_free_gb,
        swap_warn_mb = cfg.swap_warn_mb,
        tick_ms = cfg.tick_ms,
        min_proc_cpu_pct = cfg.min_proc_cpu_pct,
        min_proc_rss_mb = cfg.min_proc_rss_mb,
        dry_run = cfg.dry_run,
        notify = cfg.notify,
        quiet = cfg.quiet,
        auto_update = cfg.auto_update,
        auto_update_interval_hours = cfg.auto_update_interval_hours,
        log_line = log_line,
        protect_line = protect_line,
    );

    fs::write(path, body).map_err(|e| format!("Failed to write {}: {}", path.display(), e))
}

fn print_help() {
    println!(
        r#"resguard — universal low-overhead resource watcher & governor

USAGE:
    resguard [OPTIONS]

OPTIONS:
    -t, --tui, --dash       Launch interactive htop/btop-style terminal dashboard
    --dry-run               Observe & log actions without modifying processes
    --once                  Print one snapshot and exit
    --soft <PCT>            Soft CPU watermark -> background QoS / E-cores (default: 75)
    --high <PCT>            High CPU watermark -> duty-cycle throttling (default: 95)
    --cap <PCT>             Hard CPU ceiling -> aggressive throttling (default: 97)
    --release <PCT>         Hysteresis release threshold (default: 68)
    --ram-high <PCT>        High RAM % threshold (default: 90)
    --ram-crit <PCT>        Critical RAM % -> pause top allocator (default: 95)
    --disk-min-gb <GB>      Min free SSD GB before swap-emergency guard (default: 8)
    --swap-warn-mb <MB>     Swap MB threshold combined with low disk (default: 2048)
    --tick-ms <MS>          Sampling interval in ms (default: 500)
    --min-proc-cpu <PCT>    Ignore processes below this total-CPU % (default: 8.0)
    --protect <NAMES>       Comma-separated extra process names to protect
    --log-file <PATH>       Append plain-text timestamped logs to a file
    --color                 Force ANSI color output
    --no-color              Disable ANSI color output
    --config <PATH>         Custom config file (default: ~/.config/resguard/config.toml)
    --init-config           Write default config file and exit
    --no-notify             Disable desktop notifications
    -q, --quiet             Only log state transitions and interventions
    --auto-update           Enable periodic background self-updates (default: on)
    --no-auto-update        Disable automatic background self-updates
    --check-update          Check GitHub Releases for a newer version and exit
    --update                Download and install the latest release and exit
    --install-service       Install and start resguard as a user/system service
    --uninstall-service     Stop and remove the resguard service
    --service-status        Print installed service status
    -V, --version           Print version
    -h, --help              Print help"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_config_roundtrip() {
        let dir = env::temp_dir().join(format!("resguard_cfg_test_{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.cpu_soft = 72.0;
        cfg.cpu_high = 91.0;
        cfg.cpu_cap = 96.0;
        cfg.protect_names.push("MyCustomWorker".to_string());
        cfg.log_file = Some("/tmp/my_resguard.log".to_string());

        save_config(&path, &cfg).expect("save_config should succeed");
        let loaded = load_config_or_default(Some(&path)).expect("load_config_or_default should succeed");

        assert!((loaded.cpu_soft - 72.0).abs() < 0.01);
        assert!((loaded.cpu_high - 91.0).abs() < 0.01);
        assert!((loaded.cpu_cap - 96.0).abs() < 0.01);
        assert_eq!(loaded.log_file.as_deref(), Some("/tmp/my_resguard.log"));
        assert!(loaded.protect_names.iter().any(|n| n == "MyCustomWorker"));
        assert!(loaded.protect_names.iter().any(|n| n == "rs-dash"));

        let _ = fs::remove_dir_all(&dir);
    }
}
