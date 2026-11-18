# resguard

Universal, zero-dependency Rust resource watcher & governor that keeps your machine responsive and prevents 100% CPU lockups or low-SSD swap crashes.

## Features

- **Zero external crate dependencies** — builds offline in seconds into a ~460 KB native binary with minimal runtime overhead (< 0.2% CPU, ~2–4 MB RAM).
- **Universal** — works on any runaway user workload (compilers, browsers, video encoders, local LLMs, containers, scripts), not tied to any specific stack.
- **3-Zone CPU Governor (`75% / 95% / 97%` defaults with `68%` hysteresis):**
  - `< 68%` (after cooldown): restores normal process QoS.
  - `75% – 95%` (**Soft**): demotes heavy background consumers to `PRIO_DARWIN_BG` (Apple Silicon E-cores + background I/O) — processes keep running smoothly without starving your UI.
  - `95% – 97%+` (**High / Cap**): applies proportional `SIGSTOP`/`SIGCONT` duty-cycle pulsing across top CPU consumers (and sibling worker trees) so total CPU stays under ceiling.
- **RAM + Swap + Low-SSD Emergency Guard:**
  - Tracks RAM %, Swap usage, and free space on `/`.
  - If free SSD space drops below `disk_min_free_gb` (default `8 GB`) while swap/RAM pressure rises (or RAM hits `92%+`), pauses the largest non-protected allocator (`SIGSTOP`) and sends a notification before swap exhausts the disk.
- **Safety First:**
  - Never touches PID `0/1`, other users' processes (unless root), your ancestor shell/terminal chain, OS UI/audio services, or configured protected apps.
  - Failsafe cleanup on `SIGINT` / `SIGTERM` / `SIGHUP` immediately resumes (`SIGCONT`) any paused processes and restores QoS.

## Quick Install

**macOS / Linux:**

```bash
curl -fsSL https://resguard.repentandliveholy.org/install.sh | sh
# Or install + register background service + RS Dash GUI:
curl -fsSL https://resguard.repentandliveholy.org/install.sh | sh -s -- --service --with-gui
```

**Windows (PowerShell):**

```powershell
irm https://resguard.repentandliveholy.org/install.ps1 | iex
```

## Service & Auto-Update

```bash
# Install and start as a user/system service (launchd / systemd / Task Scheduler)
resguard --install-service
resguard --service-status
resguard --uninstall-service

# Check or apply updates from GitHub Releases (with SHA256 verification)
resguard --check-update
resguard --update
```

## Build & Install from Source

```bash
cargo build --release --workspace
# Optional zero-dependency daemon-only build (without TUI):
cargo build --release --no-default-features -p resguard
# Optional system-wide install:
sudo cp target/release/resguard /usr/local/bin/resguard
sudo cp target/release/rs-tui /usr/local/bin/rs-tui
sudo cp target/release/rs-dash /usr/local/bin/rs-dash
# Or install via cargo:
cargo install --path .
```

## Terminal TUI Dashboard (`resguard --tui` / `rs-tui`)

An interactive `htop`/`btop`-style terminal dashboard built with `ratatui` + `crossterm`:

```bash
resguard --tui    # or: resguard --dash / resguard -t
# or run standalone binary:
rs-tui
```

Keybindings: `m` (cycle `DRY-RUN` / `ACTIVE GUARD` / `OBSERVE`), `Tab` (Dashboard / Live Config), `/` (search), `f` (filter), `s` (sort), `b` (toggle BG QoS), `Space` (Pause/Resume), `p` (Protect/Unprotect), `w` (save `config.toml`), `?` (help), `q` (quit & restore).

## RS Dash (Resguard Dashboard App)

A native Rust desktop dashboard (`rs-dash`, built with `eframe`/`egui`) is included in the workspace while keeping the `resguard` CLI/daemon dependency-free.

```bash
# Run directly:
cargo run --release -p rs-dash

# Or build and open the macOS .app bundle:
./scripts/build-app.sh
open "target/release/RS Dash.app"
```

Features:
- **Live Dashboard**: CPU, Memory/Pressure, Swap, Root SSD free space, GPU %, sparklines, and interactive time-series charts with threshold overlays.
- **Safe Daemon Coexistence**: Automatically detects if a background `resguard` daemon is running and defaults to **Monitor (Dry-Run)** mode; switch to **Active Guard**, **Dry-Run**, or **Observe** anytime.
- **Process Explorer**: Search, filter, and sort live processes with one-click **Protect/Unprotect**, **BG QoS**, and **Pause/Resume** controls.
- **Events, Daemon Log Tail & Settings**: Inspect live governor interventions, tail `/tmp/resguard.log`, and edit/save thresholds to `~/.config/resguard/config.toml`.

## Quick Start

1. Test safely in `--dry-run` first:
   ```bash
   resguard --dry-run
   ```
2. Generate a user config at `~/.config/resguard/config.toml`:
   ```bash
   resguard --init-config
   ```
3. Run actively (default thresholds: soft `75%`, high `95%`, cap `97%`, SSD min `8 GB`):
   ```bash
   resguard
   ```
4. Custom thresholds & extra protected apps:
   ```bash
   resguard --soft 70 --high 90 --cap 95 --protect "Docker,Ollama"
   ```

## Run in Background on macOS (`launchd`)

```bash
sudo cp target/release/resguard /usr/local/bin/resguard
cp launchd/com.resguard.agent.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.resguard.agent.plist
```

To stop:
```bash
launchctl unload ~/Library/LaunchAgents/com.resguard.agent.plist
```

## Cross-Platform Architecture

All policy logic (`src/governor.rs`, `src/config.rs`, `src/model.rs`) is OS-agnostic behind `PlatformBackend` (`src/platform/mod.rs`):

- `src/platform/macos.rs`: Mach `host_statistics64`, `libproc`, `sysctlbyname`, `statvfs`, `PRIO_DARWIN_BG`, `SIGSTOP`/`SIGCONT`.
- `src/platform/linux.rs`: `/proc/stat`, `/proc/meminfo`, `/proc/pressure/memory` (PSI), `statvfs`, delegated `cgroups v2` (`cpu.weight`/`io.weight`) with `nice` fallback, `SIGSTOP`/`SIGCONT`.
- `src/platform/windows.rs`: Win32 `GetSystemTimes`, `GlobalMemoryStatusEx`, `GetDiskFreeSpaceExW`, `CreateToolhelp32Snapshot`, reversible `Job Objects` (`JOBOBJECT_CPU_RATE_CONTROL_HARD_CAP`) + `SetPriorityClass`, `NtSuspendProcess`/`NtResumeProcess`.
