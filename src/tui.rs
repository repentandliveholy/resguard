use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Gauge, List, ListItem, Paragraph, Row, Sparkline, Table,
    TableState, Wrap,
};
use ratatui::{Frame, Terminal};

use crate::config::{self, Config};
use crate::control::Command;
use crate::governor::{EventKind, Zone};
use crate::model::{MemoryPressure, SystemMetrics};
use crate::runtime::{local_hms, Authority, Mode, ProcRow, Role, Runtime};

/// Default length of a manual pause from the TUI.
const MANUAL_PAUSE_SECS: u64 = 30;

const HISTORY_CAP: usize = 120;
const EVENTS_CAP: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiMode {
    DryRun,
    Active,
    Observe,
}

impl TuiMode {
    fn next(self) -> Self {
        match self {
            TuiMode::DryRun => TuiMode::Active,
            TuiMode::Active => TuiMode::Observe,
            TuiMode::Observe => TuiMode::DryRun,
        }
    }

    fn label(self) -> &'static str {
        match self {
            TuiMode::DryRun => "DRY-RUN",
            TuiMode::Active => "ACTIVE",
            TuiMode::Observe => "OBSERVE",
        }
    }

    fn color(self) -> Color {
        match self {
            TuiMode::DryRun => Color::Cyan,
            TuiMode::Active => Color::Green,
            TuiMode::Observe => Color::DarkGray,
        }
    }

    fn to_mode(self) -> Mode {
        match self {
            TuiMode::DryRun => Mode::DryRun,
            TuiMode::Active => Mode::Active,
            TuiMode::Observe => Mode::Observe,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiTab {
    Dashboard,
    Config,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcFilter {
    All,
    ActiveCpu,
    Governed,
    Protected,
}

impl ProcFilter {
    fn next(self) -> Self {
        match self {
            ProcFilter::All => ProcFilter::ActiveCpu,
            ProcFilter::ActiveCpu => ProcFilter::Governed,
            ProcFilter::Governed => ProcFilter::Protected,
            ProcFilter::Protected => ProcFilter::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ProcFilter::All => "All",
            ProcFilter::ActiveCpu => "Active≥1%",
            ProcFilter::Governed => "Governed",
            ProcFilter::Protected => "Protected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcSort {
    CpuDesc,
    RamDesc,
    PidAsc,
    NameAsc,
}

impl ProcSort {
    fn next(self) -> Self {
        match self {
            ProcSort::CpuDesc => ProcSort::RamDesc,
            ProcSort::RamDesc => ProcSort::PidAsc,
            ProcSort::PidAsc => ProcSort::NameAsc,
            ProcSort::NameAsc => ProcSort::CpuDesc,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ProcSort::CpuDesc => "CPU%",
            ProcSort::RamDesc => "RAM",
            ProcSort::PidAsc => "PID",
            ProcSort::NameAsc => "Name",
        }
    }
}

#[derive(Debug, Clone)]
struct EventEntry {
    ts: String,
    kind: EventKind,
    msg: String,
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok((Self { active: true }, terminal))
    }

    fn leave(&mut self) {
        if self.active {
            self.active = false;
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

struct TuiState {
    cfg: Config,
    mode: TuiMode,
    tab: TuiTab,
    show_help: bool,
    filter: ProcFilter,
    sort: ProcSort,
    search_mode: bool,
    search: String,
    table_state: TableState,
    table_area: Rect,
    cfg_idx: usize,
    status_msg: Option<(String, Color, Instant)>,

    backend_name: String,
    num_cores: usize,
    authority: Authority,
    zone: Zone,
    own_cpu: f64,
    governable_cpu: f64,
    other_cpu: f64,
    metrics: SystemMetrics,
    rows: Vec<ProcRow>,
    protected_count: usize,
    demoted_count: usize,
    paused_count: usize,
    cpu_hist: VecDeque<u64>,
    ram_hist: VecDeque<u64>,
    events: VecDeque<EventEntry>,
}

impl TuiState {
    fn new(cfg: Config, backend_name: String, num_cores: usize) -> Self {
        let mode = if cfg.dry_run {
            TuiMode::DryRun
        } else {
            // Default safe mode until daemon check runs
            TuiMode::DryRun
        };
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self {
            cfg,
            mode,
            tab: TuiTab::Dashboard,
            show_help: false,
            filter: ProcFilter::All,
            sort: ProcSort::CpuDesc,
            search_mode: false,
            search: String::new(),
            table_state,
            table_area: Rect::default(),
            cfg_idx: 0,
            status_msg: None,
            backend_name,
            num_cores,
            authority: Authority::Simulating,
            zone: Zone::Normal,
            own_cpu: 0.0,
            governable_cpu: 0.0,
            other_cpu: 0.0,
            metrics: SystemMetrics {
                cpu_total_pct: 0.0,
                cpu_user_pct: 0.0,
                cpu_sys_pct: 0.0,
                gpu_pct: None,
                load_1m: 0.0,
                ram_total_bytes: 0,
                ram_used_bytes: 0,
                ram_used_pct: 0.0,
                memory_pressure: MemoryPressure::Normal,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
                disk_free_bytes: 0,
                num_cores,
            },
            rows: Vec::new(),
            protected_count: 0,
            demoted_count: 0,
            paused_count: 0,
            cpu_hist: VecDeque::with_capacity(HISTORY_CAP),
            ram_hist: VecDeque::with_capacity(HISTORY_CAP),
            events: VecDeque::with_capacity(EVENTS_CAP),
        }
    }

    fn push_event(&mut self, kind: EventKind, msg: String) {
        self.push_event_at(local_hms(), kind, msg);
    }

    fn push_event_at(&mut self, ts: String, kind: EventKind, msg: String) {
        if self.events.len() >= EVENTS_CAP {
            self.events.pop_back();
        }
        self.events.push_front(EventEntry { ts, kind, msg });
    }

    fn set_status(&mut self, msg: String, color: Color) {
        self.status_msg = Some((msg, color, Instant::now()));
    }

    fn visible_rows(&self) -> Vec<&ProcRow> {
        let needle = self.search.trim().to_ascii_lowercase();
        let mut out: Vec<&ProcRow> = self
            .rows
            .iter()
            .filter(|r| {
                let f_ok = match self.filter {
                    ProcFilter::All => true,
                    ProcFilter::ActiveCpu => r.sample.cpu_pct >= 1.0,
                    ProcFilter::Governed => r.demoted || r.paused || r.sample.is_stopped,
                    ProcFilter::Protected => !r.eligibility.is_governable(),
                };
                if !f_ok {
                    return false;
                }
                if needle.is_empty() {
                    return true;
                }
                r.sample.name.to_ascii_lowercase().contains(&needle)
                    || r.sample.exe_path.to_ascii_lowercase().contains(&needle)
                    || r.sample.pid.to_string().contains(&needle)
            })
            .collect();

        match self.sort {
            ProcSort::CpuDesc => out.sort_by(|a, b| {
                b.sample
                    .cpu_pct
                    .partial_cmp(&a.sample.cpu_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.sample.rss_bytes.cmp(&a.sample.rss_bytes))
            }),
            ProcSort::RamDesc => out.sort_by(|a, b| b.sample.rss_bytes.cmp(&a.sample.rss_bytes)),
            ProcSort::PidAsc => out.sort_by_key(|r| r.sample.pid),
            ProcSort::NameAsc => out.sort_by(|a, b| {
                a.sample
                    .name
                    .to_ascii_lowercase()
                    .cmp(&b.sample.name.to_ascii_lowercase())
            }),
        }
        out
    }

    fn selected_process(&self) -> Option<(u32, String, bool, bool, bool)> {
        let vis = self.visible_rows();
        let idx = self.table_state.selected()?;
        vis.get(idx).map(|r| {
            (
                r.sample.pid,
                r.sample.name.clone(),
                !r.eligibility.is_governable(),
                r.demoted,
                r.sample.is_stopped,
            )
        })
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_rows().len();
        if len == 0 {
            self.table_state.select(None);
        } else {
            let cur = self.table_state.selected().unwrap_or(0);
            self.table_state.select(Some(cur.min(len - 1)));
        }
    }
}

pub fn run_tui(cfg: Config) -> Result<(), String> {
    // Start in dry-run; Active is one key press away ([m]). If the daemon is governing,
    // the TUI follows it and forwards manual actions to it.
    let mut rt = Runtime::new(cfg.clone(), Mode::DryRun, Role::Tui)?;

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default_hook(info);
    }));

    let (mut term_guard, mut terminal) =
        TerminalGuard::enter().map_err(|e| format!("Failed to initialize terminal: {}", e))?;

    let mut state = TuiState::new(cfg, rt.backend_name().to_string(), rt.num_cores());
    state.push_event(
        EventKind::System,
        "Started in DRY-RUN (press [m] for ACTIVE). Nothing is changed in dry-run.".to_string(),
    );

    let mut last_sample = Instant::now() - Duration::from_millis(state.cfg.tick_ms);
    let mut running = true;
    std::thread::sleep(Duration::from_millis(120));

    let result = (|| -> Result<(), String> {
        while running {
            let tick_dur = Duration::from_millis(state.cfg.tick_ms);
            if last_sample.elapsed() >= tick_dur {
                last_sample = Instant::now();
                let snap = rt.tick();
                for ev in snap.events {
                    state.push_event_at(ev.ts, ev.kind, ev.msg);
                }
                state.authority = snap.authority;
                state.zone = snap.zone;
                state.own_cpu = snap.own_cpu;
                state.governable_cpu = snap.governable_cpu;
                state.other_cpu = snap.other_cpu;
                state.demoted_count = snap.demoted.len();
                state.paused_count = snap.paused.len();
                state.protected_count = snap
                    .procs
                    .iter()
                    .filter(|r| !r.eligibility.is_governable())
                    .count();
                if let Some(metrics) = snap.metrics {
                    if state.cpu_hist.len() >= HISTORY_CAP {
                        state.cpu_hist.pop_front();
                    }
                    state
                        .cpu_hist
                        .push_back(metrics.cpu_total_pct.round().clamp(0.0, 100.0) as u64);
                    if state.ram_hist.len() >= HISTORY_CAP {
                        state.ram_hist.pop_front();
                    }
                    state
                        .ram_hist
                        .push_back(metrics.ram_used_pct.round().clamp(0.0, 100.0) as u64);
                    state.metrics = metrics;
                }
                state.rows = snap.procs;
                state.clamp_selection();
            }

            if let Some((_, _, t)) = &state.status_msg {
                if t.elapsed() >= Duration::from_secs(4) {
                    state.status_msg = None;
                }
            }

            terminal
                .draw(|f| draw_ui(f, &mut state))
                .map_err(|e| format!("Terminal draw error: {}", e))?;

            let wait = tick_dur
                .saturating_sub(last_sample.elapsed())
                .min(Duration::from_millis(60));
            if event::poll(wait).map_err(|e| format!("Event poll error: {}", e))? {
                match event::read().map_err(|e| format!("Event read error: {}", e))? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        running = handle_key(key.code, key.modifiers, &mut state, &mut rt);
                    }
                    Event::Mouse(mouse) => {
                        handle_mouse(mouse.kind, mouse.row, &mut state);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    // Undo everything this instance changed, on every exit path.
    rt.shutdown();
    term_guard.leave();
    result
}

fn handle_key(
    code: KeyCode,
    mods: KeyModifiers,
    state: &mut TuiState,
    rt: &mut Runtime,
) -> bool {
    if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        return false;
    }

    if state.show_help {
        match code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::Char('q') => {
                state.show_help = false;
            }
            _ => {}
        }
        return true;
    }

    if state.search_mode {
        match code {
            KeyCode::Esc => {
                state.search_mode = false;
                state.search.clear();
                state.clamp_selection();
            }
            KeyCode::Enter => {
                state.search_mode = false;
            }
            KeyCode::Backspace => {
                state.search.pop();
                state.clamp_selection();
            }
            KeyCode::Char(c) => {
                state.search.push(c);
                state.clamp_selection();
            }
            _ => {}
        }
        return true;
    }

    match code {
        KeyCode::Char('q') | KeyCode::Char('Q') => return false,
        KeyCode::Char('?') | KeyCode::Char('h') => {
            state.show_help = true;
        }
        KeyCode::Tab => {
            state.tab = match state.tab {
                TuiTab::Dashboard => TuiTab::Config,
                TuiTab::Config => TuiTab::Dashboard,
            };
        }
        KeyCode::Char('1') => state.tab = TuiTab::Dashboard,
        KeyCode::Char('2') => state.tab = TuiTab::Config,
        KeyCode::Char('m') | KeyCode::Char('M') => {
            let next = state.mode.next();
            rt.set_mode(next.to_mode());
            state.mode = next;
            state.set_status(format!("Mode -> {}", next.label()), next.color());
        }
        _ => match state.tab {
            TuiTab::Dashboard => handle_dashboard_key(code, state, rt),
            TuiTab::Config => handle_config_key(code, state, rt),
        },
    }
    true
}

fn run_command(state: &mut TuiState, rt: &mut Runtime, cmd: Command) {
    match rt.command(cmd) {
        Ok(msg) => state.set_status(msg, Color::Green),
        Err(e) => state.set_status(e, Color::Red),
    }
}

fn handle_dashboard_key(code: KeyCode, state: &mut TuiState, rt: &mut Runtime) {
    match code {
        KeyCode::Char('/') => {
            state.search_mode = true;
        }
        KeyCode::Esc => {
            if !state.search.is_empty() {
                state.search.clear();
                state.clamp_selection();
            }
        }
        KeyCode::Char('f') | KeyCode::Char('F') => {
            state.filter = state.filter.next();
            state.clamp_selection();
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            state.sort = state.sort.next();
            state.clamp_selection();
        }
        KeyCode::Down | KeyCode::Char('j') => move_selection(state, 1),
        KeyCode::Up | KeyCode::Char('k') => move_selection(state, -1),
        KeyCode::PageDown => move_selection(state, 10),
        KeyCode::PageUp => move_selection(state, -10),
        KeyCode::Home | KeyCode::Char('g') => {
            if !state.visible_rows().is_empty() {
                state.table_state.select(Some(0));
            }
        }
        KeyCode::End | KeyCode::Char('G') => {
            let len = state.visible_rows().len();
            if len > 0 {
                state.table_state.select(Some(len - 1));
            }
        }
        KeyCode::Char('b') | KeyCode::Char('B') => {
            if let Some((pid, name, is_prot, is_demoted, _)) = state.selected_process() {
                if is_prot && !is_demoted {
                    state.set_status(format!("Refused: {} ({}) is protected", name, pid), Color::Red);
                } else {
                    run_command(state, rt, Command::Background { pid, on: !is_demoted });
                }
            }
        }
        KeyCode::Char(' ') | KeyCode::Char('x') => {
            if let Some((pid, name, is_prot, _, is_stopped)) = state.selected_process() {
                if is_stopped {
                    run_command(state, rt, Command::Resume { pid });
                } else if is_prot {
                    state.set_status(format!("Refused: {} ({}) is protected", name, pid), Color::Red);
                } else {
                    run_command(state, rt, Command::Pause { pid, secs: MANUAL_PAUSE_SECS });
                }
            }
        }
        KeyCode::Char('u') | KeyCode::Char('U') => run_command(state, rt, Command::RestoreAll),
        KeyCode::Char('p') | KeyCode::Char('P') => {
            if let Some((_, name, _, _, _)) = state.selected_process() {
                if let Some(pos) = state
                    .cfg
                    .protect_names
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case(&name))
                {
                    state.cfg.protect_names.remove(pos);
                    let msg = format!("Unprotected '{}'", name);
                    state.push_event(EventKind::Manual, msg.clone());
                    state.set_status(msg, Color::Yellow);
                } else {
                    state.cfg.protect_names.push(name.clone());
                    let msg = format!("Protected '{}'", name);
                    state.push_event(EventKind::Manual, msg.clone());
                    state.set_status(msg, Color::Cyan);
                }
                rt.set_config(state.cfg.clone());
            }
        }
        KeyCode::Char('c') => {
            state.events.clear();
        }
        _ => {}
    }
}

fn handle_config_key(code: KeyCode, state: &mut TuiState, rt: &mut Runtime) {
    const NUM_FIELDS: usize = 8;
    match code {
        KeyCode::Down | KeyCode::Char('j') => {
            state.cfg_idx = (state.cfg_idx + 1).min(NUM_FIELDS - 1);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.cfg_idx = state.cfg_idx.saturating_sub(1);
        }
        KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('l') => {
            adjust_cfg_field(state, 1.0);
            rt.set_config(state.cfg.clone());
        }
        KeyCode::Left | KeyCode::Char('-') | KeyCode::Char('h') => {
            adjust_cfg_field(state, -1.0);
            rt.set_config(state.cfg.clone());
        }
        KeyCode::Char('w') | KeyCode::Char('W') => {
            if let Some(path) = config::default_config_path() {
                match config::save_config(&path, &state.cfg) {
                    Ok(()) => {
                        let msg = format!("Saved config to {}", path.display());
                        state.push_event(EventKind::System, msg.clone());
                        state.set_status(msg, Color::Green);
                    }
                    Err(e) => state.set_status(e, Color::Red),
                }
            } else {
                state.set_status("Cannot resolve config path".to_string(), Color::Red);
            }
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            state.cfg = Config::default();
            rt.set_config(state.cfg.clone());
            state.set_status("Reset config to defaults".to_string(), Color::Yellow);
        }
        _ => {}
    }
}

fn adjust_cfg_field(state: &mut TuiState, dir: f64) {
    let mut next = state.cfg.clone();
    match state.cfg_idx {
        0 => next.cpu_release = (next.cpu_release + dir * 1.0).clamp(10.0, 95.0),
        1 => next.cpu_soft = (next.cpu_soft + dir * 1.0).clamp(15.0, 99.0),
        2 => next.cpu_high = (next.cpu_high + dir * 1.0).clamp(20.0, 100.0),
        3 => {
            next.min_governable_cpu_pct = (next.min_governable_cpu_pct + dir * 1.0).clamp(0.0, 90.0)
        }
        4 => next.ram_high_pct = (next.ram_high_pct + dir * 1.0).clamp(20.0, 99.0),
        5 => next.ram_critical_pct = (next.ram_critical_pct + dir * 1.0).clamp(25.0, 99.5),
        6 => next.disk_min_free_gb = (next.disk_min_free_gb + dir * 0.5).clamp(1.0, 200.0),
        7 => {
            let delta = (dir as i64) * 100;
            next.tick_ms = ((next.tick_ms as i64) + delta).clamp(100, 5000) as u64;
        }
        _ => {}
    }
    match config::validate_config(&next) {
        Ok(()) => state.cfg = next,
        Err(e) => state.set_status(e, Color::Red),
    }
}

fn move_selection(state: &mut TuiState, delta: isize) {
    let len = state.visible_rows().len();
    if len == 0 {
        state.table_state.select(None);
        return;
    }
    let cur = state.table_state.selected().unwrap_or(0) as isize;
    let next = (cur + delta).clamp(0, (len - 1) as isize) as usize;
    state.table_state.select(Some(next));
}

fn handle_mouse(kind: MouseEventKind, row: u16, state: &mut TuiState) {
    if state.tab != TuiTab::Dashboard {
        return;
    }
    match kind {
        MouseEventKind::ScrollDown => move_selection(state, 3),
        MouseEventKind::ScrollUp => move_selection(state, -3),
        MouseEventKind::Down(_) => {
            // Table has 1 top border + 1 header row = 2 rows before data rows
            let data_top = state.table_area.y.saturating_add(2);
            let data_bot = state.table_area.y.saturating_add(state.table_area.height.saturating_sub(1));
            if row >= data_top && row < data_bot {
                let offset = state.table_state.offset();
                let target = offset + (row - data_top) as usize;
                let len = state.visible_rows().len();
                if target < len {
                    state.table_state.select(Some(target));
                }
            }
        }
        _ => {}
    }
}

fn draw_ui(f: &mut Frame<'_>, state: &mut TuiState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(10),   // Main content
            Constraint::Length(2), // Footer keybinds / toast
        ])
        .split(f.area());

    draw_header(f, root[0], state);

    match state.tab {
        TuiTab::Dashboard => draw_dashboard(f, root[1], state),
        TuiTab::Config => draw_config_tab(f, root[1], state),
    }

    draw_footer(f, root[2], state);

    if state.show_help {
        draw_help_overlay(f, f.area());
    }
}

fn draw_header(f: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let daemon_span = match &state.authority {
        Authority::Governing => Span::styled(
            " GOVERNING ",
            Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Authority::Deferred { pid, role } => Span::styled(
            format!(" FOLLOWING {}:{} ", role.to_uppercase(), pid),
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Authority::Simulating => Span::styled(" NO CHANGES ", Style::default().fg(Color::Gray).bg(Color::DarkGray)),
        Authority::Observing => Span::styled(" OBSERVING ", Style::default().fg(Color::Gray).bg(Color::DarkGray)),
    };

    let zone_col = zone_color(state.zone);
    let line = Line::from(vec![
        Span::styled(
            " RS TUI ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("resguard v{} ({}c)  ", env!("CARGO_PKG_VERSION"), state.num_cores),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", state.zone),
            Style::default()
                .fg(Color::Black)
                .bg(zone_col)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!(" {} ", state.mode.label()),
            Style::default()
                .fg(Color::Black)
                .bg(state.mode.color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        daemon_span,
        Span::raw("   "),
        Span::styled(
            if state.tab == TuiTab::Dashboard { "[1:Dashboard]" } else { " 1:Dashboard " },
            Style::default().fg(if state.tab == TuiTab::Dashboard { Color::Cyan } else { Color::DarkGray }),
        ),
        Span::raw(" "),
        Span::styled(
            if state.tab == TuiTab::Config { "[2:Config]" } else { " 2:Config " },
            Style::default().fg(if state.tab == TuiTab::Config { Color::Cyan } else { Color::DarkGray }),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    f.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_dashboard(f: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // Telemetry KPI + sparklines
            Constraint::Min(10),   // Processes + Events
        ])
        .split(area);

    // Top 3 telemetry boxes
    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(38),
            Constraint::Percentage(38),
            Constraint::Percentage(24),
        ])
        .split(rows[0]);

    draw_cpu_panel(f, top_cols[0], state);
    draw_mem_panel(f, top_cols[1], state);
    draw_guard_panel(f, top_cols[2], state);

    // Bottom split: Processes (left/top) + Events (right/bottom)
    let bot_cols = if area.width >= 110 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(63), Constraint::Percentage(37)])
            .split(rows[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(rows[1])
    };

    draw_processes_panel(f, bot_cols[0], state);
    draw_events_panel(f, bot_cols[1], state);
}

fn draw_cpu_panel(f: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let m = &state.metrics;
    let col = pct_color(m.cpu_total_pct, state.cfg.cpu_soft, state.cfg.cpu_high);
    let block = Block::default()
        .title(Span::styled(
            format!(
                " CPU {:.1}% (u:{:.0}% s:{:.0}%) load {:.1} ",
                m.cpu_total_pct, m.cpu_user_pct, m.cpu_sys_pct, m.load_1m
            ),
            Style::default().fg(col).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)])
        .split(inner);

    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(col).bg(Color::Black))
        .ratio((m.cpu_total_pct / 100.0).clamp(0.0, 1.0))
        .label(format!("{:.1}%", m.cpu_total_pct));
    f.render_widget(gauge, chunks[0]);

    let data: Vec<u64> = state.cpu_hist.iter().copied().collect();
    let spark = Sparkline::default()
        .data(&data)
        .max(100)
        .style(Style::default().fg(col));
    f.render_widget(spark, chunks[1]);

    let th_line = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(
                "yours {:.0}% (governable {:.0}%) · system/other {:.0}%  busy≥{:.0}%",
                state.own_cpu,
                state.governable_cpu,
                state.other_cpu,
                state.cfg.cpu_soft
            ),
            Style::default().fg(Color::Gray),
        ),
    ]));
    f.render_widget(th_line, chunks[2]);
}

fn draw_mem_panel(f: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let m = &state.metrics;
    let ram_col = pct_color(m.ram_used_pct, state.cfg.ram_high_pct, state.cfg.ram_critical_pct);
    let ssd_gb = m.disk_free_gb();
    let ssd_col = if ssd_gb < state.cfg.disk_min_free_gb {
        Color::Red
    } else if ssd_gb < state.cfg.disk_min_free_gb * 2.0 {
        Color::Yellow
    } else {
        Color::Green
    };

    let block = Block::default()
        .title(Span::styled(
            format!(
                " RAM {:.1}% ({:.1}/{:.1}G {}) ",
                m.ram_used_pct,
                m.ram_used_gb(),
                m.ram_total_gb(),
                m.memory_pressure
            ),
            Style::default().fg(ram_col).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(inner);

    let ram_gauge = Gauge::default()
        .gauge_style(Style::default().fg(ram_col).bg(Color::Black))
        .ratio((m.ram_used_pct / 100.0).clamp(0.0, 1.0))
        .label(format!("RAM {:.1}%", m.ram_used_pct));
    f.render_widget(ram_gauge, chunks[0]);

    let swap_ratio = if m.swap_total_bytes > 0 {
        (m.swap_used_bytes as f64 / m.swap_total_bytes as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let swap_col = if m.swap_used_mb() > state.cfg.swap_warn_mb {
        Color::Yellow
    } else {
        Color::Cyan
    };
    let swap_gauge = Gauge::default()
        .gauge_style(Style::default().fg(swap_col).bg(Color::Black))
        .ratio(swap_ratio)
        .label(format!("Swap {:.0}/{:.0}M", m.swap_used_mb(), m.swap_total_mb()));
    f.render_widget(swap_gauge, chunks[1]);

    let data: Vec<u64> = state.ram_hist.iter().copied().collect();
    let spark = Sparkline::default()
        .data(&data)
        .max(100)
        .style(Style::default().fg(Color::Blue));
    f.render_widget(spark, chunks[2]);

    let ssd_line = Paragraph::new(Line::from(vec![
        Span::styled("Root SSD Free: ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{:.1} GB", ssd_gb),
            Style::default().fg(ssd_col).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" (min {:.1}G)", state.cfg.disk_min_free_gb),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    f.render_widget(ssd_line, chunks[3]);
}

fn draw_guard_panel(f: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let block = Block::default()
        .title(Span::styled(
            " GPU & Guard ",
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let gpu_line = match state.metrics.gpu_pct {
        Some(g) => Line::from(vec![
            Span::styled("GPU:       ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.0}%", g),
                Style::default()
                    .fg(pct_color(g, state.cfg.cpu_soft, state.cfg.cpu_high))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        None => Line::from(vec![
            Span::styled("GPU:       ", Style::default().fg(Color::Gray)),
            Span::styled("N/A", Style::default().fg(Color::DarkGray)),
        ]),
    };

    let lines = vec![
        gpu_line,
        Line::from(vec![
            Span::styled("Slowed:    ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{}", state.demoted_count),
                Style::default().fg(if state.demoted_count > 0 { Color::Yellow } else { Color::Green }),
            ),
        ]),
        Line::from(vec![
            Span::styled("Paused:    ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{}", state.paused_count),
                Style::default().fg(if state.paused_count > 0 { Color::Red } else { Color::Green }),
            ),
        ]),
        Line::from(vec![
            Span::styled("Protected: ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{}", state.protected_count), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled("Tick:      ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{} ms", state.cfg.tick_ms), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Backend:   ", Style::default().fg(Color::Gray)),
            Span::styled(
                state.backend_name.split(' ').next().unwrap_or(&state.backend_name),
                Style::default().fg(Color::Gray),
            ),
        ]),
    ];

    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_processes_panel(f: &mut Frame<'_>, area: Rect, state: &mut TuiState) {
    let search_part = if state.search_mode {
        format!(" /{}█ ", state.search)
    } else if !state.search.is_empty() {
        format!(" filter='{}' ", state.search)
    } else {
        String::new()
    };

    let title = format!(
        " Processes [{filter} | sort:{sort}]{search} ",
        filter = state.filter.label(),
        sort = state.sort.label(),
        search = search_part,
    );

    let visible_rows_data: Vec<(u32, String, String, Color, f64, f64, String)> = {
        let vis = state.visible_rows();
        vis.into_iter()
            .map(|r| {
                let (st_lbl, st_col) = if r.paused {
                    ("PAUSED", Color::Red)
                } else if r.sample.is_stopped {
                    ("STOPPED", Color::Red)
                } else if r.demoted {
                    ("SLOWED", Color::Yellow)
                } else if !r.eligibility.is_governable() {
                    ("PROTECTED", Color::Cyan)
                } else {
                    ("normal", Color::DarkGray)
                };
                (
                    r.sample.pid,
                    r.sample.name.clone(),
                    st_lbl.to_string(),
                    st_col,
                    r.sample.cpu_pct,
                    r.sample.rss_mb(),
                    r.sample.exe_path.clone(),
                )
            })
            .collect()
    };

    state.table_area = area;

    let rows: Vec<Row> = visible_rows_data
        .iter()
        .map(|(pid, name, st_lbl, st_col, cpu, ram_mb, path)| {
            let cpu_col = pct_color(
                *cpu,
                state.cfg.min_proc_cpu_pct,
                state.cfg.min_proc_cpu_pct * 3.0,
            );
            Row::new(vec![
                Cell::from(format!("{:>6}", pid)).style(Style::default().fg(Color::DarkGray)),
                Cell::from(name.as_str()).style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Cell::from(st_lbl.as_str()).style(Style::default().fg(*st_col)),
                Cell::from(format!("{:5.1}%", cpu)).style(Style::default().fg(cpu_col)),
                Cell::from(format!("{:6.0}M", ram_mb)).style(Style::default().fg(Color::Gray)),
                Cell::from(path.as_str()).style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let header = Row::new(vec!["   PID", "PROCESS", "STATE", "  CPU%", "   RAM", "PATH"])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(0);

    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(Span::styled(
                title,
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if state.search_mode {
                Color::Cyan
            } else {
                Color::DarkGray
            })),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(32, 48, 72))
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");

    f.render_stateful_widget(table, area, &mut state.table_state);
}

fn draw_events_panel(f: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let block = Block::default()
        .title(Span::styled(
            " Governor Events ",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    if state.events.is_empty() {
        let p = Paragraph::new("No governor interventions yet.\nSystem is operating normally.")
            .style(Style::default().fg(Color::DarkGray))
            .block(block)
            .wrap(Wrap { trim: true });
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = state
        .events
        .iter()
        .take(50)
        .map(|ev| {
            let col = match ev.kind {
                EventKind::Alert | EventKind::Error => Color::Red,
                EventKind::Action => Color::Yellow,
                EventKind::Restore => Color::Green,
                EventKind::DryRun => Color::Cyan,
                EventKind::Manual => Color::Magenta,
                EventKind::System => Color::Gray,
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", ev.ts), Style::default().fg(Color::DarkGray)),
                Span::styled(&ev.msg, Style::default().fg(col)),
            ]))
        })
        .collect();

    f.render_widget(List::new(items).block(block), area);
}

fn draw_config_tab(f: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    let items_data = [
        ("CPU Release Hysteresis (%)", format!("{:.1}%", state.cfg.cpu_release)),
        ("CPU Busy: lower batch priority (%)", format!("{:.1}%", state.cfg.cpu_soft)),
        ("CPU Saturated (%)", format!("{:.1}%", state.cfg.cpu_high)),
        ("Min governable CPU to act (%)", format!("{:.1}%", state.cfg.min_governable_cpu_pct)),
        ("RAM High Watermark (%)", format!("{:.1}%", state.cfg.ram_high_pct)),
        ("RAM Critical Alert (%)", format!("{:.1}%", state.cfg.ram_critical_pct)),
        ("Root SSD Min Free (GB)", format!("{:.1} GB", state.cfg.disk_min_free_gb)),
        ("Control Loop Tick (ms)", format!("{} ms", state.cfg.tick_ms)),
    ];

    let rows: Vec<Row> = items_data
        .iter()
        .enumerate()
        .map(|(idx, (k, v))| {
            let selected = idx == state.cfg_idx;
            let style = if selected {
                Style::default()
                    .bg(Color::Rgb(32, 48, 72))
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            Row::new(vec![
                Cell::from(if selected { format!("▶ {}", k) } else { format!("  {}", k) }),
                Cell::from(v.as_str()),
            ])
            .style(style)
        })
        .collect();

    let cfg_table = Table::new(rows, [Constraint::Min(26), Constraint::Length(14)]).block(
        Block::default()
            .title(" Live Thresholds ([↑/↓] select, [←/→ or -/+] edit, [w] save config.toml, [r] reset) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(cfg_table, cols[0]);

    let prot_text = state.cfg.protect_names.join(", ");
    let prot_para = Paragraph::new(vec![
        Line::from(Span::styled(
            format!("Protected process names ({}):", state.cfg.protect_names.len()),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(prot_text, Style::default().fg(Color::Gray))),
        Line::from(""),
        Line::from(Span::styled(
            "Tip: Select any process in [1:Dashboard] and press [p] to toggle protection.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(
        Block::default()
            .title(" Protected Apps & Processes ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    )
    .wrap(Wrap { trim: true });
    f.render_widget(prot_para, cols[1]);
}

fn draw_footer(f: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let keybinds = if state.tab == TuiTab::Dashboard {
        "[q]Quit  [m]Mode  [Tab]Config  [/]Search  [f]Filter  [s]Sort  [b]Slow/Restore  [Space]Pause 30s/Resume  [u]Undo all  [p]Protect  [?]Help"
    } else {
        "[q]Quit  [m]Mode  [Tab]Dashboard  [↑/↓]Select  [-/+]Adjust  [w]Save config.toml  [r]Reset  [?]Help"
    };

    let mut lines = vec![Line::from(Span::styled(keybinds, Style::default().fg(Color::Gray)))];
    if let Some((ref msg, col, _)) = state.status_msg {
        lines.push(Line::from(Span::styled(
            msg.as_str(),
            Style::default().fg(col).add_modifier(Modifier::BOLD),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_help_overlay(f: &mut Frame<'_>, area: Rect) {
    let w = area.width.min(68);
    let h = area.height.min(18);
    let popup = Rect::new(
        area.x + (area.width.saturating_sub(w)) / 2,
        area.y + (area.height.saturating_sub(h)) / 2,
        w,
        h,
    );
    f.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            "resguard TUI Keybindings",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  q / Ctrl+C       Quit cleanly (restores any governed processes)"),
        Line::from("  m                Cycle mode: DRY-RUN -> ACTIVE -> OBSERVE"),
        Line::from("  Tab / 1 / 2      Switch between Dashboard and Live Config tabs"),
        Line::from("  ↑/↓ or j/k       Move process / config selection"),
        Line::from("  /                Search processes by name, PID, or path (Esc clears)"),
        Line::from("  f                Cycle process filter (All / Active / Governed / Protected)"),
        Line::from("  s                Cycle sort order (CPU% / RAM / PID / Name)"),
        Line::from("  b                Slow down (lower priority) / restore selected process"),
        Line::from("  Space / x        Pause selected process for 30 s (auto-resume) / resume it"),
        Line::from("  p                Add/remove selected process name from protected list"),
        Line::from("  u                Undo every change resguard has made"),
        Line::from("  c                Clear event log"),
        Line::from(""),
        Line::from(Span::styled("Press Esc or ? to close", Style::default().fg(Color::DarkGray))),
    ];
    let para = Paragraph::new(lines).block(
        Block::default()
            .title(" Help ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Black)),
    );
    f.render_widget(para, popup);
}

fn zone_color(zone: Zone) -> Color {
    match zone {
        Zone::Normal => Color::Green,
        Zone::Soft => Color::Yellow,
        Zone::High => Color::Magenta,
        Zone::MemoryEmergency => Color::LightRed,
    }
}

fn pct_color(val: f64, warn: f64, crit: f64) -> Color {
    if val >= crit {
        Color::Red
    } else if val >= warn {
        Color::Yellow
    } else {
        Color::Green
    }
}
