# resguard

Keeps your desktop responsive under heavy load by **de-prioritizing heavy batch work** (compilers, test runners, build tools, scripts), and warns you before memory, swap or a nearly-full disk get dangerous. Zero-dependency Rust daemon, plus a terminal UI and a desktop dashboard.

**Design rule: slowing something down is acceptable; freezing it never is.**

## What it does

- **Sustained CPU load** (default ≥ 75% for 1 s): lowers the priority of the heaviest *governable* processes — macOS background QoS (E-cores), Linux cgroup weight / `nice`, Windows below-normal priority. They keep running, just behind your apps. Restored once the machine is calm (with a minimum dwell to avoid flapping).
- **Load it can't help with:** if most CPU comes from the kernel, system services or other users, resguard says so and does nothing, instead of squeezing whatever it can see.
- **Memory / swap / low disk:** alerts (log + desktop notification naming the biggest allocator). Pausing is **opt-in** (`emergency_pause = true`): one process at a time, never an app, auto-resumed after `emergency_pause_secs` (default 20 s).

## Safety guarantees

- **Never pauses anything for CPU load.** (v0.1 SIGSTOP-pulsed processes to hold CPU under a hard cap; that could freeze apps and was removed in v0.2.)
- **Never touches** GUI apps (anything inside a macOS `.app` bundle), system binaries, terminals and shells, its own terminal chain, other users' processes, or names on the protect list.
- **Every change is recorded** in a ledger and undone on exit, on restart after a crash, or on demand: `resguard --restore`.
- **One governor at a time.** The daemon, `rs-tui` and RS Dash share one lock; dashboards follow the running daemon and forward your actions to it.
- Manual pauses from the TUI/GUI are always time-limited (30 s) and refused for protected processes.

## Quick Install

**macOS / Linux:**

```bash
curl -fsSL https://resguard.repentandliveholy.org/install.sh | sh
# Or install + register background service + RS Dash GUI:
curl -fsSL https://resguard.repentandliveholy.org/install.sh | sh -s -- --service --with-gui
```

**Windows:** download the installer (per-user, no admin, self-updating):
https://github.com/repentandliveholy/resguard/releases/latest/download/resguard-setup-windows-x86_64.exe

or use PowerShell:

```powershell
irm https://resguard.repentandliveholy.org/install.ps1 | iex
```

## Service & Auto-Update

```bash
# Install and start as a user/system service (launchd / systemd / Task Scheduler)
resguard --install-service
resguard --service-status
resguard --uninstall-service
resguard --restore            # undo every change resguard has made

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

Keybindings: `m` (cycle `DRY-RUN` / `ACTIVE` / `OBSERVE`), `Tab` (Dashboard / Live Config), `/` (search), `f` (filter), `s` (sort), `b` (slow down / restore), `Space` (pause 30 s / resume), `u` (undo all changes), `p` (protect/unprotect), `w` (save `config.toml`), `?` (help), `q` (quit & restore).

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
- **Status in one sentence:** who is in charge (the background service, RS Dash, or nobody) and what is currently slowed or paused, with *Undo all changes*.
- **Where the CPU load comes from:** batch work resguard may slow down vs. your apps vs. system/kernel/other users.
- **Processes:** search, filter and sort; slow down / restore / pause 30 s / resume. Each protected process shows why it is protected.
- **Activity & service log**, and **Settings** saved to `~/.config/resguard/config.toml` — the running service reloads it automatically.
- Starts in **Dry-run**; when the background service is running, RS Dash follows it instead of governing itself.

## Quick Start

1. Test safely in `--dry-run` first:
   ```bash
   resguard --dry-run
   ```
2. Generate a user config at `~/.config/resguard/config.toml`:
   ```bash
   resguard --init-config
   ```
3. Run actively (defaults: busy `75%`, saturated `95%`, SSD min `8 GB`):
   ```bash
   resguard
   ```
4. Custom thresholds & extra protected apps:
   ```bash
   resguard --soft 70 --high 90 --protect "Docker,Ollama"
   ```

## Run in Background on macOS (`launchd`)

```bash
resguard --install-service     # LaunchAgent running `resguard --service`, logs to /tmp/resguard.log
resguard --uninstall-service
```

## Cross-Platform Architecture

Policy (`src/governor.rs`) and the shared governor loop used by the daemon, TUI and GUI (`src/runtime.rs`, with lock/ledger/status/inbox files in `src/control.rs`) are OS-agnostic behind `PlatformBackend` (`src/platform/mod.rs`):

- `src/platform/macos.rs`: Mach `host_statistics64`, `libproc`, `sysctlbyname`, `statvfs`, `PRIO_DARWIN_BG`, `SIGSTOP`/`SIGCONT`.
- `src/platform/linux.rs`: `/proc/stat`, `/proc/meminfo`, `/proc/pressure/memory` (PSI), `statvfs`, delegated `cgroups v2` (`cpu.weight`/`io.weight`) with `nice` fallback, `SIGSTOP`/`SIGCONT`.
- `src/platform/windows.rs`: Win32 `GetSystemTimes`, `GlobalMemoryStatusEx`, `GetDiskFreeSpaceExW`, `CreateToolhelp32Snapshot`, reversible `Job Objects` (`JOBOBJECT_CPU_RATE_CONTROL_HARD_CAP`) + `SetPriorityClass`, `NtSuspendProcess`/`NtResumeProcess`.
