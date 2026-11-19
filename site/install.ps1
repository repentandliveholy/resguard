$ErrorActionPreference = "Stop"

$Repo = "repentandliveholy/resguard"
$SiteReleases = "https://resguard.repentandliveholy.org/releases"
$Version = if ($env:RESGUARD_VERSION) { $env:RESGUARD_VERSION } else { "latest" }
$InstallDir = if ($env:RESGUARD_INSTALL_DIR) { $env:RESGUARD_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "resguard\bin" }

$Asset = "resguard-windows-x86_64.zip"
$GhBase = if ($Version -eq "latest") { "https://github.com/$Repo/releases/latest/download" } else { "https://github.com/$Repo/releases/download/$Version" }

$TmpDir = Join-Path $env:TEMP ("resguard_install_" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $ZipPath = Join-Path $TmpDir $Asset
    Write-Host "==> Downloading $Asset..."
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$GhBase/$Asset" -OutFile $ZipPath
    } catch {
        Invoke-WebRequest -UseBasicParsing -Uri "$SiteReleases/$Asset" -OutFile $ZipPath
    }

    try {
        $SumsPath = Join-Path $TmpDir "SHA256SUMS.txt"
        Invoke-WebRequest -UseBasicParsing -Uri "$GhBase/SHA256SUMS.txt" -OutFile $SumsPath
        $Line = Get-Content $SumsPath | Where-Object { $_ -match "\b$([regex]::Escape($Asset))$" } | Select-Object -First 1
        if ($Line) {
            $Expected = ($Line -split "\s+")[0].Trim()
            $Actual = (Get-FileHash -Algorithm SHA256 $ZipPath).Hash
            if ($Actual -ine $Expected) {
                throw "SHA256 mismatch for ${Asset}: expected $Expected, got $Actual"
            }
            Write-Host "==> SHA256 verified."
        }
    } catch {
        if ($_.Exception.Message -like "SHA256 mismatch*") { throw }
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    # Upgrading over a running install: stop the logon task and any resguard /
    # rs-dash / rs-tui started from $InstallDir so their files are not locked.
    # A force-stopped governor's changes are undone from its ledger on next start.
    $HadService = $false
    try {
        & schtasks.exe /Query /TN resguard 2>&1 | Out-Null
        $HadService = ($LASTEXITCODE -eq 0)
        if ($HadService) { & schtasks.exe /End /TN resguard 2>&1 | Out-Null }
    } catch { }
    $InstallDirFull = [System.IO.Path]::GetFullPath($InstallDir).TrimEnd('\') + '\'
    $Running = @(Get-Process -Name "resguard", "rs-dash", "rs-tui" -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($InstallDirFull, [System.StringComparison]::OrdinalIgnoreCase) })
    if ($Running.Count -gt 0) {
        Write-Host "==> Stopping running resguard processes..."
        $Running | Stop-Process -Force -ErrorAction SilentlyContinue
        $Running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    }
    # Leftovers from the self-updater (renamed running executables).
    Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $InstallDir "*.old")

    Expand-Archive -Force -Path $ZipPath -DestinationPath $InstallDir

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        $NewPath = if ([string]::IsNullOrEmpty($UserPath)) { $InstallDir } else { "$($UserPath.TrimEnd(';'));$InstallDir" }
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        $env:Path = "$env:Path;$InstallDir"
        Write-Host "==> Added $InstallDir to User PATH."
    }

    # Start Menu shortcut for RS Dash (same location the .exe installer uses).
    try {
        $Programs = [Environment]::GetFolderPath("Programs")
        $Shell = New-Object -ComObject WScript.Shell
        $Shortcut = $Shell.CreateShortcut((Join-Path $Programs "RS Dash.lnk"))
        $Shortcut.TargetPath = Join-Path $InstallDir "rs-dash.exe"
        $Shortcut.WorkingDirectory = $InstallDir
        $Shortcut.Description = "RS Dash - resguard dashboard"
        $Shortcut.Save()
        Write-Host "==> Created Start Menu shortcut: RS Dash"
    } catch {
        Write-Host "==> Could not create Start Menu shortcut: $($_.Exception.Message)"
    }

    # (Re)register the logon task when asked to, or when one was already installed
    # (re-registering points it at the new binary and starts it again).
    if ($env:RESGUARD_INSTALL_SERVICE -eq "1" -or $HadService) {
        & (Join-Path $InstallDir "resguard.exe") --install-service
    }

    Write-Host ""
    Write-Host "Installed resguard, rs-tui, and rs-dash to $InstallDir"
    Write-Host "Try: resguard --tui   |   resguard --install-service   |   rs-dash"
    Write-Host ""
    Write-Host "Prefer a regular Windows installer (per-user, no admin, self-updating)?"
    Write-Host "  https://github.com/$Repo/releases/latest/download/resguard-setup-windows-x86_64.exe"
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $TmpDir
}
