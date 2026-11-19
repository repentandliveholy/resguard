//! Well-known file locations shared by the daemon, TUI and GUI.

use std::env;
use std::path::PathBuf;

/// Per-user directory for resguard runtime state (lock, ledger, status, inbox).
///
/// - macOS:   ~/Library/Application Support/resguard
/// - Linux:   $XDG_STATE_HOME/resguard or ~/.local/state/resguard
/// - Windows: %LOCALAPPDATA%\resguard
pub fn state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("RESGUARD_STATE_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join("Library/Application Support/resguard");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("resguard");
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(xdg) = env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(xdg).join("resguard");
        }
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(".local/state/resguard");
        }
    }
    env::temp_dir().join("resguard")
}

/// Default log file written by the background service.
pub fn default_service_log() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        state_dir().join("resguard.log")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/tmp/resguard.log")
    }
}
