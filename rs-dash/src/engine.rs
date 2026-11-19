//! Background thread that drives the shared [`Runtime`] and publishes snapshots to the UI.
//!
//! RS Dash has no governor of its own: it uses exactly the same runtime as the daemon and
//! the TUI. When the background daemon is governing, RS Dash follows it and forwards
//! actions to it; it only governs itself in Active mode when nothing else does.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use resguard::config::Config;
use resguard::control::Command;
use resguard::governor::EventKind;
use resguard::runtime::{local_hms, Event, Mode, Role, Runtime, Snapshot};

pub const HISTORY_CAP: usize = 240;
pub const EVENTS_CAP: usize = 400;

#[derive(Debug, Clone)]
pub struct HistoryPoint {
    pub ts: String,
    pub cpu: f64,
    pub ram: f64,
    pub governable: f64,
}

/// Everything the UI renders. Replaced wholesale on every tick.
pub struct Shared {
    pub generation: u64,
    pub backend_name: String,
    pub num_cores: usize,
    pub snapshot: Option<Snapshot>,
    pub config: Config,
    pub history: VecDeque<HistoryPoint>,
    pub events: VecDeque<Event>,
    /// Result of the last user action, shown as a toast.
    pub toast: Option<(String, bool, Instant)>,
    pub fatal: Option<String>,
}

pub enum EngineCmd {
    SetMode(Mode),
    SetConfig(Config),
    Run(Command),
    Shutdown,
}

pub struct EngineHandle {
    pub shared: Arc<Mutex<Shared>>,
    tx: Sender<EngineCmd>,
    worker: Option<JoinHandle<()>>,
}

impl EngineHandle {
    pub fn start(ctx: egui::Context, cfg: Config, mode: Mode) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            generation: 0,
            backend_name: String::new(),
            num_cores: 1,
            snapshot: None,
            config: cfg.clone(),
            history: VecDeque::with_capacity(HISTORY_CAP),
            events: VecDeque::with_capacity(EVENTS_CAP),
            toast: None,
            fatal: None,
        }));
        let (tx, rx) = mpsc::channel();
        let shared_bg = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("rs-dash-engine".into())
            .spawn(move || run(ctx, shared_bg, rx, cfg, mode))
            .expect("spawn engine thread");
        Self {
            shared,
            tx,
            worker: Some(worker),
        }
    }

    pub fn send(&self, cmd: EngineCmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(EngineCmd::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    ctx: egui::Context,
    shared: Arc<Mutex<Shared>>,
    rx: Receiver<EngineCmd>,
    cfg: Config,
    mode: Mode,
) {
    let mut rt = match Runtime::new(cfg, mode, Role::Gui) {
        Ok(rt) => rt,
        Err(e) => {
            shared.lock().unwrap().fatal = Some(format!("Could not start: {}", e));
            ctx.request_repaint();
            return;
        }
    };
    {
        let mut s = shared.lock().unwrap();
        s.backend_name = rt.backend_name().to_string();
        s.num_cores = rt.num_cores();
    }
    if let Some(path) = resguard::config::default_config_path() {
        rt.watch_config(path.clone(), move || resguard::config::load_config_or_default(Some(&path)));
    }

    // Let the first per-process CPU delta accumulate.
    if !wait_or_cmd(Duration::from_millis(300), &rx, &mut rt, &shared) {
        rt.shutdown();
        return;
    }

    loop {
        let started = Instant::now();
        let snap = rt.tick();
        publish(&shared, &rt, snap);
        ctx.request_repaint();

        let tick = Duration::from_millis(rt.config().tick_ms.max(250));
        if !wait_or_cmd(tick.saturating_sub(started.elapsed()), &rx, &mut rt, &shared) {
            break;
        }
    }
    rt.shutdown();
}

fn publish(shared: &Arc<Mutex<Shared>>, rt: &Runtime, mut snap: Snapshot) {
    let mut s = shared.lock().unwrap();
    for ev in snap.events.drain(..) {
        if s.events.len() >= EVENTS_CAP {
            s.events.pop_back();
        }
        s.events.push_front(ev);
    }
    if let Some(m) = &snap.metrics {
        if s.history.len() >= HISTORY_CAP {
            s.history.pop_front();
        }
        s.history.push_back(HistoryPoint {
            ts: local_hms(),
            cpu: m.cpu_total_pct,
            ram: m.ram_used_pct,
            governable: snap.governable_cpu,
        });
    }
    s.config = rt.config().clone();
    s.snapshot = Some(snap);
    s.generation += 1;
}

/// Sleep up to `dur`, handling UI commands as they arrive. Returns false on shutdown.
fn wait_or_cmd(
    dur: Duration,
    rx: &Receiver<EngineCmd>,
    rt: &mut Runtime,
    shared: &Arc<Mutex<Shared>>,
) -> bool {
    let deadline = Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        match rx.recv_timeout(left) {
            Ok(EngineCmd::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            Ok(EngineCmd::SetMode(m)) => rt.set_mode(m),
            Ok(EngineCmd::SetConfig(c)) => rt.set_config(c),
            Ok(EngineCmd::Run(cmd)) => {
                let res = rt.command(cmd);
                let mut s = shared.lock().unwrap();
                s.toast = Some(match res {
                    Ok(m) => (m, true, Instant::now()),
                    Err(e) => (e, false, Instant::now()),
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return true,
        }
    }
}

pub fn event_is_noteworthy(kind: EventKind) -> bool {
    !matches!(kind, EventKind::System)
}

pub fn read_tail_lines(path: &Path, max_bytes: u64, max_lines: usize) -> Result<Vec<String>, String> {
    let mut f = File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).map_err(|e| format!("seek failed: {}", e))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).map_err(|e| format!("read failed: {}", e))?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.len() > max_lines {
        let drop_n = lines.len() - max_lines;
        lines.drain(0..drop_n);
    }
    Ok(lines)
}
