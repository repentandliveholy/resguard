use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const REPO_SLUG: &str = "repentandliveholy/resguard";
pub const SITE_RELEASES_BASE: &str = "https://resguard.repentandliveholy.org/releases";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    UpToDate(String),
    Updated { from: String, to: String },
}

pub fn platform_asset_name() -> Result<&'static str, String> {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    match (os, arch) {
        ("macos", "aarch64") => Ok("resguard-macos-arm64.tar.gz"),
        ("macos", "x86_64") => Ok("resguard-macos-x86_64.tar.gz"),
        ("linux", "x86_64") => Ok("resguard-linux-x86_64.tar.gz"),
        ("linux", "aarch64") => Ok("resguard-linux-arm64.tar.gz"),
        ("windows", "x86_64") => Ok("resguard-windows-x86_64.zip"),
        _ => Err(format!(
            "No prebuilt release binary for {}/{}",
            os, arch
        )),
    }
}

pub fn check_latest_version() -> Result<UpdateCheck, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let api_url = format!("https://api.github.com/repos/{}/releases/latest", REPO_SLUG);
    let body = fetch_text(&api_url).or_else(|_| {
        fetch_text(&format!("{}/VERSION", SITE_RELEASES_BASE))
            .map(|v| format!("{{\"tag_name\":\"{}\"}}", v.trim()))
    })?;

    let latest_raw = parse_latest_tag_from_json(&body)
        .ok_or_else(|| "Could not parse latest release version from response".to_string())?;
    let latest = latest_raw.trim().trim_start_matches('v').to_string();
    let update_available = version_is_newer(&current, &latest);

    Ok(UpdateCheck {
        current_version: current,
        latest_version: latest,
        update_available,
    })
}

pub fn perform_self_update(force: bool) -> Result<UpdateOutcome, String> {
    let check = check_latest_version()?;
    if !force && !check.update_available {
        return Ok(UpdateOutcome::UpToDate(check.current_version));
    }

    let asset = platform_asset_name()?;
    let tmp_dir = env::temp_dir().join(format!("resguard_update_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir)
        .map_err(|e| format!("Failed to create temp dir {}: {}", tmp_dir.display(), e))?;

    let res = perform_self_update_inner(&tmp_dir, asset, &check);
    let _ = fs::remove_dir_all(&tmp_dir);
    res
}

fn perform_self_update_inner(
    tmp_dir: &Path,
    asset: &str,
    check: &UpdateCheck,
) -> Result<UpdateOutcome, String> {
    let archive_path = tmp_dir.join(asset);
    let sums_path = tmp_dir.join("SHA256SUMS.txt");

    let gh_asset_url = format!(
        "https://github.com/repos_unused/{}/releases/latest/download/{}",
        REPO_SLUG, asset
    );
    let _ = gh_asset_url;

    let asset_urls = [
        format!(
            "https://github.com/{}/releases/latest/download/{}",
            REPO_SLUG, asset
        ),
        format!("{}/{}", SITE_RELEASES_BASE, asset),
    ];

    let mut downloaded = false;
    let mut last_err = String::new();
    for url in &asset_urls {
        match download_file(url, &archive_path) {
            Ok(()) => {
                downloaded = true;
                break;
            }
            Err(e) => last_err = e,
        }
    }
    if !downloaded {
        return Err(format!("Failed to download {}: {}", asset, last_err));
    }

    // Verify SHA256 if SHA256SUMS.txt is published
    let sums_urls = [
        format!(
            "https://github.com/{}/releases/latest/download/SHA256SUMS.txt",
            REPO_SLUG
        ),
        format!("{}/SHA256SUMS.txt", SITE_RELEASES_BASE),
    ];
    for url in &sums_urls {
        if download_file(url, &sums_path).is_ok() {
            if let Ok(sums_txt) = fs::read_to_string(&sums_path) {
                if let Some(expected_hash) = find_sha256_for_asset(&sums_txt, asset) {
                    let actual_hash = compute_sha256(&archive_path)?;
                    if !actual_hash.eq_ignore_ascii_case(&expected_hash) {
                        return Err(format!(
                            "SHA256 mismatch for {} (expected {}, got {})",
                            asset, expected_hash, actual_hash
                        ));
                    }
                }
            }
            break;
        }
    }

    let extract_dir = tmp_dir.join("extract");
    fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("Failed to create extract dir: {}", e))?;
    extract_archive(&archive_path, &extract_dir)?;

    let bin_name = if cfg!(windows) { "resguard.exe" } else { "resguard" };
    let new_bin = find_file_recursive(&extract_dir, bin_name)
        .ok_or_else(|| format!("Downloaded archive did not contain {}", bin_name))?;

    let current_exe = env::current_exe()
        .map_err(|e| format!("Failed to resolve current executable path: {}", e))?;
    replace_executable(&new_bin, &current_exe)?;

    // Also update sibling rs-tui if it sits in the same directory
    let tui_name = if cfg!(windows) { "rs-tui.exe" } else { "rs-tui" };
    if let Some(parent) = current_exe.parent() {
        let sibling_tui = parent.join(tui_name);
        if sibling_tui.exists() {
            if let Some(new_tui) = find_file_recursive(&extract_dir, tui_name) {
                let _ = replace_executable(&new_tui, &sibling_tui);
            }
        }
    }

    Ok(UpdateOutcome::Updated {
        from: check.current_version.clone(),
        to: check.latest_version.clone(),
    })
}

fn curl_bin() -> &'static str {
    if cfg!(windows) {
        "curl.exe"
    } else {
        "curl"
    }
}

fn fetch_text(url: &str) -> Result<String, String> {
    let out = Command::new(curl_bin())
        .args([
            "-fsSL",
            "--connect-timeout",
            "8",
            "--max-time",
            "15",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: resguard-updater",
            url,
        ])
        .output()
        .map_err(|e| format!("Failed to invoke curl for {}: {}", url, e))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("HTTP fetch failed for {}: {}", url, err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn download_file(url: &str, dest: &Path) -> Result<(), String> {
    let dest_str = dest.to_string_lossy().to_string();
    let out = Command::new(curl_bin())
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            "120",
            "-H",
            "User-Agent: resguard-updater",
            "-o",
            &dest_str,
            url,
        ])
        .output()
        .map_err(|e| format!("Failed to invoke curl for {}: {}", url, e))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("Download failed for {}: {}", url, err.trim()));
    }
    Ok(())
}

fn compute_sha256(path: &Path) -> Result<String, String> {
    let p = path.to_string_lossy().to_string();
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("shasum")
            .args(["-a", "256", &p])
            .output()
            .map_err(|e| format!("shasum failed: {}", e))?;
        if !out.status.success() {
            return Err("shasum returned non-zero".to_string());
        }
        let txt = String::from_utf8_lossy(&out.stdout);
        return txt
            .split_whitespace()
            .next()
            .map(|s| s.to_string())
            .ok_or_else(|| "Unexpected shasum output".to_string());
    }
    #[cfg(target_os = "linux")]
    {
        let out = Command::new("sha256sum")
            .arg(&p)
            .output()
            .map_err(|e| format!("sha256sum failed: {}", e))?;
        if !out.status.success() {
            return Err("sha256sum returned non-zero".to_string());
        }
        let txt = String::from_utf8_lossy(&out.stdout);
        return txt
            .split_whitespace()
            .next()
            .map(|s| s.to_string())
            .ok_or_else(|| "Unexpected sha256sum output".to_string());
    }
    #[cfg(target_os = "windows")]
    {
        let script = format!("(Get-FileHash -Algorithm SHA256 '{}').Hash", p.replace('\'', "''"));
        let out = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .map_err(|e| format!("powershell Get-FileHash failed: {}", e))?;
        if !out.status.success() {
            return Err("Get-FileHash returned non-zero".to_string());
        }
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = p;
        Err("SHA256 verification unsupported on this OS".to_string())
    }
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    let arch_str = archive.to_string_lossy().to_string();
    let dest_str = dest.to_string_lossy().to_string();

    if arch_str.ends_with(".zip") {
        let script = format!(
            "Expand-Archive -Force -Path '{}' -DestinationPath '{}'",
            arch_str.replace('\'', "''"),
            dest_str.replace('\'', "''")
        );
        let out = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .map_err(|e| format!("Expand-Archive failed: {}", e))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("Failed to unzip archive: {}", err.trim()));
        }
    } else {
        let out = Command::new("tar")
            .args(["-xzf", &arch_str, "-C", &dest_str])
            .output()
            .map_err(|e| format!("tar failed: {}", e))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("Failed to extract archive: {}", err.trim()));
        }
    }
    Ok(())
}

fn find_file_recursive(dir: &Path, target_name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_recursive(&path, target_name) {
                return Some(found);
            }
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == target_name)
            .unwrap_or(false)
        {
            return Some(path);
        }
    }
    None
}

fn replace_executable(src: &Path, dst: &Path) -> Result<(), String> {
    let parent = dst
        .parent()
        .ok_or_else(|| format!("Invalid destination path: {}", dst.display()))?;
    let staging = parent.join(format!(
        ".{}.update.{}",
        dst.file_name().and_then(|n| n.to_str()).unwrap_or("resguard"),
        std::process::id()
    ));

    fs::copy(src, &staging).map_err(|e| {
        format!(
            "Failed to stage binary at {} (check permissions): {}",
            staging.display(),
            e
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&staging, fs::Permissions::from_mode(0o755));
    }

    #[cfg(windows)]
    {
        let backup = parent.join(format!(
            "{}.old",
            dst.file_name().and_then(|n| n.to_str()).unwrap_or("resguard.exe")
        ));
        let _ = fs::remove_file(&backup);
        if dst.exists() {
            fs::rename(dst, &backup).map_err(|e| format!("Failed to move running exe: {}", e))?;
        }
    }

    if let Err(e) = fs::rename(&staging, dst) {
        let _ = fs::remove_file(&staging);
        return Err(format!("Failed to replace {}: {}", dst.display(), e));
    }
    Ok(())
}

pub fn parse_latest_tag_from_json(json: &str) -> Option<String> {
    let key = "\"tag_name\"";
    let idx = json.find(key)?;
    let after_key = &json[idx + key.len()..];
    let colon = after_key.find(':')?;
    let after_colon = after_key[colon + 1..].trim_start();
    let stripped = after_colon.strip_prefix('"')?;
    let end = stripped.find('"')?;
    Some(stripped[..end].to_string())
}

pub fn find_sha256_for_asset(sums_txt: &str, asset: &str) -> Option<String> {
    for line in sums_txt.lines() {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        if file == asset || file.ends_with(&format!("/{}", asset)) {
            if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(hash.to_string());
            }
        }
    }
    None
}

pub fn version_is_newer(current: &str, latest: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let clean = s.trim().trim_start_matches('v').split('-').next().unwrap_or("0");
        let mut nums = clean.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        (
            nums.next().unwrap_or(0),
            nums.next().unwrap_or(0),
            nums.next().unwrap_or(0),
        )
    };
    parse(latest) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tag_name_from_github_json() {
        let sample = r#"{"url":"https://api.github.com/repos/x/y","tag_name": "v0.2.1", "draft": false}"#;
        assert_eq!(parse_latest_tag_from_json(sample).as_deref(), Some("v0.2.1"));
    }

    #[test]
    fn compares_semver_cleanly() {
        assert!(version_is_newer("0.1.0", "0.1.1"));
        assert!(version_is_newer("0.1.0", "0.2.0"));
        assert!(version_is_newer("0.1.9", "1.0.0"));
        assert!(!version_is_newer("0.1.0", "0.1.0"));
        assert!(!version_is_newer("0.2.0", "0.1.9"));
    }

    #[test]
    fn parses_sha256sums_lines() {
        let sums = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  resguard-macos-arm64.tar.gz\n\
                    fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210 *dist/resguard-linux-x86_64.tar.gz\n";
        assert_eq!(
            find_sha256_for_asset(sums, "resguard-macos-arm64.tar.gz").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            find_sha256_for_asset(sums, "resguard-linux-x86_64.tar.gz").as_deref(),
            Some("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210")
        );
        assert!(find_sha256_for_asset(sums, "missing.zip").is_none());
    }
}
