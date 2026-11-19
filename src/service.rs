use std::env;
#[cfg(not(windows))]
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// The `resguard` daemon binary: the current executable, or its sibling when invoked
/// from `rs-tui` / `rs-dash`.
fn daemon_exe() -> Result<PathBuf, String> {
    let exe = env::current_exe()
        .map_err(|e| format!("Failed to resolve current executable path: {}", e))?;
    let daemon_name = if cfg!(windows) { "resguard.exe" } else { "resguard" };
    if exe.file_name().and_then(|n| n.to_str()) == Some(daemon_name) {
        return Ok(exe);
    }
    let sibling = exe
        .parent()
        .map(|d| d.join(daemon_name))
        .filter(|p| p.exists())
        .ok_or_else(|| format!("Could not find {} next to {}", daemon_name, exe.display()))?;
    Ok(sibling)
}

pub fn install_service() -> Result<String, String> {
    let exe = daemon_exe()?;
    let exe_str = exe.to_string_lossy().to_string();

    #[cfg(target_os = "macos")]
    {
        install_macos(&exe_str)
    }
    #[cfg(target_os = "linux")]
    {
        install_linux(&exe_str)
    }
    #[cfg(target_os = "windows")]
    {
        install_windows(&exe_str)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = exe_str;
        Err("Service installation is not supported on this OS".to_string())
    }
}

pub fn uninstall_service() -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        uninstall_macos()
    }
    #[cfg(target_os = "linux")]
    {
        uninstall_linux()
    }
    #[cfg(target_os = "windows")]
    {
        uninstall_windows()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err("Service management is not supported on this OS".to_string())
    }
}

pub fn service_status() -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        status_macos()
    }
    #[cfg(target_os = "linux")]
    {
        status_linux()
    }
    #[cfg(target_os = "windows")]
    {
        status_windows()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err("Service management is not supported on this OS".to_string())
    }
}

#[cfg(target_os = "macos")]
fn macos_plist_path() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents/com.resguard.agent.plist"))
}

#[cfg(target_os = "macos")]
fn macos_uid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(target_os = "macos")]
fn install_macos(exe: &str) -> Result<String, String> {
    let plist_path = macos_plist_path()?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.resguard.agent</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>--service</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>/tmp/resguard.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/resguard.err.log</string>
</dict>
</plist>
"#,
        xml_escape(exe)
    );

    fs::write(&plist_path, plist)
        .map_err(|e| format!("Failed to write {}: {}", plist_path.display(), e))?;

    let domain = format!("gui/{}", macos_uid());
    let plist_str = plist_path.to_string_lossy().to_string();

    // Unload existing service if loaded
    let _ = Command::new("launchctl")
        .args(["bootout", &domain, &plist_str])
        .output();
    let _ = Command::new("launchctl")
        .args(["unload", &plist_str])
        .output();

    let out = Command::new("launchctl")
        .args(["bootstrap", &domain, &plist_str])
        .output()
        .map_err(|e| format!("Failed to run launchctl: {}", e))?;

    if !out.status.success() {
        let fallback = Command::new("launchctl")
            .args(["load", "-w", &plist_str])
            .output()
            .map_err(|e| format!("Failed to run launchctl load: {}", e))?;
        if !fallback.status.success() {
            let err = String::from_utf8_lossy(&fallback.stderr);
            return Err(format!("launchctl load failed: {}", err.trim()));
        }
    }

    Ok(format!(
        "Installed and started macOS LaunchAgent at {}",
        plist_path.display()
    ))
}

#[cfg(target_os = "macos")]
fn uninstall_macos() -> Result<String, String> {
    let plist_path = macos_plist_path()?;
    let domain = format!("gui/{}", macos_uid());
    let plist_str = plist_path.to_string_lossy().to_string();

    let _ = Command::new("launchctl")
        .args(["bootout", &domain, &plist_str])
        .output();
    let _ = Command::new("launchctl")
        .args(["unload", "-w", &plist_str])
        .output();

    if plist_path.exists() {
        fs::remove_file(&plist_path)
            .map_err(|e| format!("Failed to remove {}: {}", plist_path.display(), e))?;
    }
    Ok(format!("Uninstalled macOS LaunchAgent ({})", plist_path.display()))
}

#[cfg(target_os = "macos")]
fn status_macos() -> Result<String, String> {
    let plist_path = macos_plist_path()?;
    let installed = plist_path.exists();
    let target = format!("gui/{}/com.resguard.agent", macos_uid());
    let out = Command::new("launchctl").args(["print", &target]).output();
    let running = out.as_ref().map(|o| o.status.success()).unwrap_or(false);
    Ok(format!(
        "macOS LaunchAgent: installed={} ({}), loaded={}",
        installed,
        plist_path.display(),
        running
    ))
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(target_os = "linux")]
fn linux_is_root() -> bool {
    extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() == 0 }
}

#[cfg(target_os = "linux")]
fn linux_unit_path() -> Result<(PathBuf, bool), String> {
    if linux_is_root() {
        Ok((PathBuf::from("/etc/systemd/system/resguard.service"), true))
    } else {
        let home = env::var_os("HOME").ok_or("HOME is not set")?;
        Ok((
            PathBuf::from(home).join(".config/systemd/user/resguard.service"),
            false,
        ))
    }
}

#[cfg(target_os = "linux")]
fn install_linux(exe: &str) -> Result<String, String> {
    let (unit_path, system) = linux_unit_path()?;
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }

    let wanted_by = if system {
        "multi-user.target"
    } else {
        "default.target"
    };
    let unit = format!(
        "[Unit]\n\
Description=resguard universal resource watcher & governor\n\
After=default.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={} --service\n\
Restart=always\n\
RestartSec=3\n\
\n\
[Install]\n\
WantedBy={}\n",
        exe, wanted_by
    );

    fs::write(&unit_path, unit)
        .map_err(|e| format!("Failed to write {}: {}", unit_path.display(), e))?;

    let mut base_args: Vec<&str> = Vec::new();
    if !system {
        base_args.push("--user");
    }

    let mut reload_args = base_args.clone();
    reload_args.push("daemon-reload");
    let _ = Command::new("systemctl").args(&reload_args).output();

    let mut enable_args = base_args;
    enable_args.extend(["enable", "--now", "resguard.service"]);
    let out = Command::new("systemctl")
        .args(&enable_args)
        .output()
        .map_err(|e| format!("Failed to run systemctl: {}", e))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("systemctl enable failed: {}", err.trim()));
    }

    Ok(format!(
        "Installed and started systemd {} service at {}",
        if system { "system" } else { "user" },
        unit_path.display()
    ))
}

#[cfg(target_os = "linux")]
fn uninstall_linux() -> Result<String, String> {
    let (unit_path, system) = linux_unit_path()?;
    let mut args = Vec::new();
    if !system {
        args.push("--user");
    }
    args.extend(["disable", "--now", "resguard.service"]);
    let _ = Command::new("systemctl").args(&args).output();

    if unit_path.exists() {
        fs::remove_file(&unit_path)
            .map_err(|e| format!("Failed to remove {}: {}", unit_path.display(), e))?;
    }

    let mut reload = Vec::new();
    if !system {
        reload.push("--user");
    }
    reload.push("daemon-reload");
    let _ = Command::new("systemctl").args(&reload).output();

    Ok(format!("Uninstalled systemd service ({})", unit_path.display()))
}

#[cfg(target_os = "linux")]
fn status_linux() -> Result<String, String> {
    let (unit_path, system) = linux_unit_path()?;
    let mut args = Vec::new();
    if !system {
        args.push("--user");
    }
    args.extend(["is-active", "resguard.service"]);
    let out = Command::new("systemctl").args(&args).output();
    let active = out
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    Ok(format!(
        "systemd {} service: installed={} ({}), state={}",
        if system { "system" } else { "user" },
        unit_path.exists(),
        unit_path.display(),
        active
    ))
}

#[cfg(target_os = "windows")]
fn install_windows(exe: &str) -> Result<String, String> {
    // Per-user task at logon, normal privileges. `--service` detaches the console window
    // and logs to %LOCALAPPDATA%\resguard\resguard.log.
    let tr = format!("\"{}\" --service", exe);
    let out = Command::new("schtasks")
        .args(["/Create", "/TN", "resguard", "/TR", &tr, "/SC", "ONLOGON", "/F"])
        .output()
        .map_err(|e| format!("Failed to run schtasks: {}", e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("schtasks /Create failed: {}", err.trim()));
    }

    let _ = Command::new("schtasks")
        .args(["/Run", "/TN", "resguard"])
        .output();

    Ok("Installed and started Windows Task Scheduler job 'resguard' (on logon)".to_string())
}

#[cfg(target_os = "windows")]
fn uninstall_windows() -> Result<String, String> {
    let exists = Command::new("schtasks")
        .args(["/Query", "/TN", "resguard"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        return Ok("Windows Task 'resguard' is not installed".to_string());
    }
    let _ = Command::new("schtasks")
        .args(["/End", "/TN", "resguard"])
        .output();
    let out = Command::new("schtasks")
        .args(["/Delete", "/TN", "resguard", "/F"])
        .output()
        .map_err(|e| format!("Failed to run schtasks: {}", e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("schtasks /Delete failed: {}", err.trim()));
    }
    Ok("Uninstalled Windows Task Scheduler job 'resguard'".to_string())
}

#[cfg(target_os = "windows")]
fn status_windows() -> Result<String, String> {
    let out = Command::new("schtasks")
        .args(["/Query", "/TN", "resguard"])
        .output();
    let exists = out.as_ref().map(|o| o.status.success()).unwrap_or(false);
    Ok(format!("Windows Task 'resguard': installed={}", exists))
}
