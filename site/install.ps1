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
    Expand-Archive -Force -Path $ZipPath -DestinationPath $InstallDir

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
        $env:Path = "$env:Path;$InstallDir"
        Write-Host "==> Added $InstallDir to User PATH."
    }

    if ($env:RESGUARD_INSTALL_SERVICE -eq "1") {
        & (Join-Path $InstallDir "resguard.exe") --install-service
    }

    Write-Host ""
    Write-Host "Installed resguard, rs-tui, and rs-dash to $InstallDir"
    Write-Host "Try: resguard --tui   |   resguard --install-service   |   rs-dash"
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $TmpDir
}
