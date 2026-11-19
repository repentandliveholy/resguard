<#
.SYNOPSIS
    Builds resguard-setup-windows-x86_64.exe with Inno Setup (used by CI and release).

.DESCRIPTION
    - Locates ISCC.exe (PATH, then Program Files (x86)\Inno Setup 6/7); installs
      Inno Setup via Chocolatey if it is missing.
    - Reads the version from the workspace Cargo.toml unless -Version is given.
    - Best effort: renders the RS Dash icon (rs-dash --export-icon-tga) and wraps it
      into an .ico for the wizard and shortcuts. Any failure here only skips the icon.
    - Compiles installer/windows/resguard.iss.

.EXAMPLE
    ./installer/windows/build-installer.ps1 -SourceDir dist/stage -OutputDir dist
#>
param(
    # Folder containing resguard.exe, rs-tui.exe, rs-dash.exe and README.md.
    [Parameter(Mandatory = $true)] [string]$SourceDir,
    # Where resguard-setup-windows-x86_64.exe is written.
    [string]$OutputDir = "dist",
    # Defaults to the first `version = "..."` line of Cargo.toml.
    [string]$Version,
    # Inno Setup compression spec override (e.g. zip/1 for fast CI validation).
    [string]$Compression,
    [switch]$NoIcon
)

$ErrorActionPreference = "Stop"
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$IssPath = Join-Path $PSScriptRoot "resguard.iss"

function Find-Iscc {
    $cmd = Get-Command "iscc.exe" -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    foreach ($base in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
        if (-not $base) { continue }
        foreach ($dir in @("Inno Setup 6", "Inno Setup 7")) {
            $candidate = Join-Path $base "$dir\ISCC.exe"
            if (Test-Path $candidate) { return $candidate }
        }
    }
    return $null
}

function Convert-TgaToIco([string]$TgaPath, [string]$IcoPath) {
    # rs-dash writes an uncompressed 32-bit BGRA TGA. Load it into a 32bpp ARGB
    # bitmap (same BGRA byte order in memory), downscale, and store PNG-compressed
    # entries in an .ico container (supported since Windows Vista).
    Add-Type -AssemblyName System.Drawing
    $bytes = [IO.File]::ReadAllBytes($TgaPath)
    if ($bytes[2] -ne 2 -or $bytes[16] -ne 32) { throw "Unsupported TGA format" }
    $idLen = [int]$bytes[0]
    $w = [int][BitConverter]::ToUInt16($bytes, 12)
    $h = [int][BitConverter]::ToUInt16($bytes, 14)
    $topDown = ($bytes[17] -band 0x20) -ne 0
    $stride = $w * 4
    $offset = 18 + $idLen

    $src = New-Object System.Drawing.Bitmap($w, $h, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $rect = New-Object System.Drawing.Rectangle(0, 0, $w, $h)
    $data = $src.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::WriteOnly, $src.PixelFormat)
    try {
        for ($y = 0; $y -lt $h; $y++) {
            $row = if ($topDown) { $y } else { $h - 1 - $y }
            $dst = [IntPtr]::Add($data.Scan0, $y * $data.Stride)
            [Runtime.InteropServices.Marshal]::Copy($bytes, $offset + $row * $stride, $dst, $stride)
        }
    } finally {
        $src.UnlockBits($data)
    }

    $images = @()
    foreach ($size in @(256, 64, 48, 32, 16)) {
        $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
        $g.DrawImage($src, 0, 0, $size, $size)
        $g.Dispose()
        $ms = New-Object IO.MemoryStream
        $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
        $bmp.Dispose()
        $images += , @($size, $ms.ToArray())
    }
    $src.Dispose()

    $fs = [IO.File]::Create($IcoPath)
    $bw = New-Object IO.BinaryWriter($fs)
    try {
        $bw.Write([UInt16]0)               # reserved
        $bw.Write([UInt16]1)               # type: icon
        $bw.Write([UInt16]$images.Count)
        $dataOffset = 6 + 16 * $images.Count
        foreach ($img in $images) {
            $size = $img[0]; $png = $img[1]
            $dim = if ($size -ge 256) { 0 } else { $size }
            $bw.Write([Byte]$dim)          # width (0 = 256)
            $bw.Write([Byte]$dim)          # height
            $bw.Write([Byte]0)             # palette colors
            $bw.Write([Byte]0)             # reserved
            $bw.Write([UInt16]1)           # color planes
            $bw.Write([UInt16]32)          # bits per pixel
            $bw.Write([UInt32]$png.Length)
            $bw.Write([UInt32]$dataOffset)
            $dataOffset += $png.Length
        }
        foreach ($img in $images) { $bw.Write([byte[]]$img[1]) }
    } finally {
        $bw.Dispose()
        $fs.Dispose()
    }
}

# --- Inputs ------------------------------------------------------------------
$SourceDir = (Resolve-Path $SourceDir).Path
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$OutputDir = (Resolve-Path $OutputDir).Path

foreach ($f in @("resguard.exe", "rs-tui.exe", "rs-dash.exe", "README.md")) {
    if (-not (Test-Path (Join-Path $SourceDir $f))) { throw "Missing $f in $SourceDir" }
}

if (-not $Version) {
    $m = Select-String -Path (Join-Path $RepoRoot "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $m) { throw "Could not read version from Cargo.toml" }
    $Version = $m.Matches[0].Groups[1].Value
}
Write-Host "==> resguard version: $Version"

# --- Inno Setup --------------------------------------------------------------
$Iscc = Find-Iscc
if (-not $Iscc) {
    Write-Host "==> Inno Setup not found; installing via Chocolatey..."
    choco install innosetup -y --no-progress
    if ($LASTEXITCODE -ne 0) { throw "choco install innosetup failed ($LASTEXITCODE)" }
    $Iscc = Find-Iscc
}
if (-not $Iscc) { throw "ISCC.exe not found after installing Inno Setup" }
Write-Host "==> Using $Iscc"

# --- Optional icon -----------------------------------------------------------
$IsccArgs = @("/Qp", "/DAppVersion=$Version", "/DSourceDir=$SourceDir", "/DOutputDir=$OutputDir")
if ($Compression) { $IsccArgs += "/DCompression=$Compression" }

if (-not $NoIcon) {
    try {
        $IconDir = Join-Path ([IO.Path]::GetTempPath()) ("resguard-icon-" + [Guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $IconDir | Out-Null
        $Tga = Join-Path $IconDir "icon.tga"
        $Ico = Join-Path $IconDir "rs-dash.ico"
        # Start-Process -Wait also works if rs-dash is built as a GUI-subsystem exe.
        $p = Start-Process -FilePath (Join-Path $SourceDir "rs-dash.exe") `
            -ArgumentList @("--export-icon-tga", "`"$Tga`"") -Wait -PassThru -NoNewWindow
        if ($p.ExitCode -ne 0 -or -not (Test-Path $Tga)) { throw "rs-dash --export-icon-tga failed ($($p.ExitCode))" }
        Convert-TgaToIco $Tga $Ico
        $IsccArgs += "/DSetupIcon=$Ico"
        Write-Host "==> Generated installer icon: $Ico"
    } catch {
        Write-Warning "Skipping installer icon: $($_.Exception.Message)"
    }
}

# --- Compile -----------------------------------------------------------------
Write-Host "==> $Iscc $($IsccArgs -join ' ') $IssPath"
& $Iscc @IsccArgs $IssPath
if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit code $LASTEXITCODE" }

$Setup = Join-Path $OutputDir "resguard-setup-windows-x86_64.exe"
if (-not (Test-Path $Setup)) { throw "Expected installer not found: $Setup" }
Write-Host "==> Built $Setup ($([math]::Round((Get-Item $Setup).Length / 1MB, 1)) MB)"
