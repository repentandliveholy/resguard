//! Coordination files under [`paths::state_dir`]:
//!
//! - `governor.lock` — only one resguard instance (daemon, TUI or GUI) may change processes.
//! - `ledger`        — every live change, rewritten on each change. If the governor dies
//!                     without cleaning up, the next instance (or `resguard --restore`)
//!                     undoes what is listed here.
//! - `status`        — what the governor is doing, for dashboards that defer to it.
//! - `inbox/`        — commands from dashboards to the governor.
//!
//! All files are small line-oriented text written atomically (temp file + rename).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::governor::{Applied, EventKind};
use crate::paths;

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn clean_field(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

pub fn write_atomic(path: &Path, body: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {}", parent.display(), e))?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("{}: {}", tmp.display(), e))?;
        f.write_all(body.as_bytes())
            .map_err(|e| format!("{}: {}", tmp.display(), e))?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {}", path.display(), e)
    })
}

// ---------------------------------------------------------------- lock

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    pub pid: u32,
    pub role: String,
}

pub struct ControlDir {
    pub dir: PathBuf,
}

impl Default for ControlDir {
    fn default() -> Self {
        Self::new(paths::state_dir())
    }
}

impl ControlDir {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("governor.lock")
    }
    fn ledger_path(&self) -> PathBuf {
        self.dir.join("ledger")
    }
    fn status_path(&self) -> PathBuf {
        self.dir.join("status")
    }
    fn inbox_dir(&self) -> PathBuf {
        self.dir.join("inbox")
    }

    pub fn read_lock(&self) -> Option<LockInfo> {
        let text = fs::read_to_string(self.lock_path()).ok()?;
        let mut lines = text.lines();
        let pid = lines.next()?.trim().parse().ok()?;
        let role = lines.next().unwrap_or("unknown").trim().to_string();
        Some(LockInfo { pid, role })
    }

    /// Try to become the governor. `is_alive(pid)` must say whether a resguard instance
    /// with that PID is still running. On failure returns the live holder.
    pub fn try_acquire(
        &self,
        self_pid: u32,
        role: &str,
        is_alive: &dyn Fn(u32) -> bool,
    ) -> Result<(), LockInfo> {
        let _ = fs::create_dir_all(&self.dir);
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.lock_path())
            {
                Ok(mut f) => {
                    let _ = write!(f, "{}\n{}\n", self_pid, role);
                    return Ok(());
                }
                Err(_) => match self.read_lock() {
                    Some(info) if info.pid == self_pid => return Ok(()),
                    Some(info) if is_alive(info.pid) => return Err(info),
                    _ => {
                        // Stale or unreadable lock: the previous holder is gone.
                        let _ = fs::remove_file(self.lock_path());
                    }
                },
            }
        }
        Err(self.read_lock().unwrap_or(LockInfo {
            pid: 0,
            role: "unknown".into(),
        }))
    }

    pub fn release(&self, self_pid: u32) {
        if self.read_lock().map_or(false, |l| l.pid == self_pid) {
            let _ = fs::remove_file(self.lock_path());
        }
    }

    // ------------------------------------------------------------ ledger

    pub fn write_ledger(&self, entries: &[Applied]) -> Result<(), String> {
        if entries.is_empty() {
            let _ = fs::remove_file(self.ledger_path());
            return Ok(());
        }
        let mut body = String::new();
        for e in entries {
            match e {
                Applied::Background { pid, name } => {
                    body.push_str(&format!("bg\t{}\t{}\n", pid, clean_field(name)))
                }
                Applied::Paused { pid, name } => {
                    body.push_str(&format!("pause\t{}\t{}\n", pid, clean_field(name)))
                }
            }
        }
        write_atomic(&self.ledger_path(), &body)
    }

    pub fn read_ledger(&self) -> Vec<Applied> {
        let Ok(text) = fs::read_to_string(self.ledger_path()) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let kind = parts.next()?;
                let pid: u32 = parts.next()?.parse().ok()?;
                let name = parts.next().unwrap_or("").to_string();
                match kind {
                    "bg" => Some(Applied::Background { pid, name }),
                    "pause" => Some(Applied::Paused { pid, name }),
                    _ => None,
                }
            })
            .collect()
    }

    // ------------------------------------------------------------ status

    pub fn write_status(&self, status: &Status) -> Result<(), String> {
        let mut body = format!(
            "pid\t{}\nrole\t{}\nupdated\t{}\n",
            status.pid,
            clean_field(&status.role),
            status.updated_unix
        );
        for (pid, name) in &status.demoted {
            body.push_str(&format!("bg\t{}\t{}\n", pid, clean_field(name)));
        }
        for (pid, secs_left, name) in &status.paused {
            body.push_str(&format!("pause\t{}\t{}\t{}\n", pid, secs_left, clean_field(name)));
        }
        for ev in &status.events {
            body.push_str(&format!(
                "ev\t{}\t{}\t{}\t{}\n",
                ev.seq,
                ev.kind.code(),
                clean_field(&ev.ts),
                clean_field(&ev.msg)
            ));
        }
        write_atomic(&self.status_path(), &body)
    }

    pub fn read_status(&self) -> Option<Status> {
        let text = fs::read_to_string(self.status_path()).ok()?;
        let mut st = Status::default();
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            match parts.as_slice() {
                ["pid", v] => st.pid = v.parse().unwrap_or(0),
                ["role", v] => st.role = v.to_string(),
                ["updated", v] => st.updated_unix = v.parse().unwrap_or(0),
                ["bg", pid, name] => {
                    if let Ok(pid) = pid.parse() {
                        st.demoted.push((pid, name.to_string()));
                    }
                }
                ["pause", pid, secs, name] => {
                    if let (Ok(pid), Ok(secs)) = (pid.parse(), secs.parse()) {
                        st.paused.push((pid, secs, name.to_string()));
                    }
                }
                ["ev", seq, kind, ts, msg] => {
                    if let Ok(seq) = seq.parse() {
                        st.events.push(StatusEvent {
                            seq,
                            kind: EventKind::from_code(kind),
                            ts: ts.to_string(),
                            msg: msg.to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
        Some(st)
    }

    pub fn remove_status(&self) {
        let _ = fs::remove_file(self.status_path());
    }

    // ------------------------------------------------------------ inbox

    pub fn send_command(&self, cmd: &Command) -> Result<(), String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = self
            .inbox_dir()
            .join(format!("{:024}-{}.cmd", nanos, std::process::id()));
        write_atomic(&path, &format!("{}\n", cmd.encode()))
    }

    pub fn take_commands(&self) -> Vec<Command> {
        let Ok(entries) = fs::read_dir(self.inbox_dir()) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "cmd"))
            .collect();
        files.sort();
        let mut out = Vec::new();
        for f in files {
            if let Ok(text) = fs::read_to_string(&f) {
                out.extend(text.lines().filter_map(Command::decode));
            }
            let _ = fs::remove_file(&f);
        }
        out
    }
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub pid: u32,
    pub role: String,
    pub updated_unix: u64,
    pub demoted: Vec<(u32, String)>,
    /// (pid, seconds left, name)
    pub paused: Vec<(u32, u64, String)>,
    pub events: Vec<StatusEvent>,
}

#[derive(Debug, Clone)]
pub struct StatusEvent {
    pub seq: u64,
    pub kind: EventKind,
    pub ts: String,
    pub msg: String,
}

/// Requests a dashboard can make of whoever governs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Background { pid: u32, on: bool },
    Pause { pid: u32, secs: u64 },
    Resume { pid: u32 },
    RestoreAll,
}

impl Command {
    pub fn encode(&self) -> String {
        match self {
            Command::Background { pid, on } => {
                format!("bg {} {}", pid, if *on { "on" } else { "off" })
            }
            Command::Pause { pid, secs } => format!("pause {} {}", pid, secs),
            Command::Resume { pid } => format!("resume {}", pid),
            Command::RestoreAll => "restore-all".to_string(),
        }
    }

    pub fn decode(line: &str) -> Option<Command> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["bg", pid, "on"] => Some(Command::Background {
                pid: pid.parse().ok()?,
                on: true,
            }),
            ["bg", pid, "off"] => Some(Command::Background {
                pid: pid.parse().ok()?,
                on: false,
            }),
            ["pause", pid, secs] => Some(Command::Pause {
                pid: pid.parse().ok()?,
                secs: secs.parse().ok()?,
            }),
            ["resume", pid] => Some(Command::Resume {
                pid: pid.parse().ok()?,
            }),
            ["restore-all"] => Some(Command::RestoreAll),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> ControlDir {
        let d = std::env::temp_dir().join(format!(
            "resguard_ctl_{}_{}_{}",
            tag,
            std::process::id(),
            now_unix()
        ));
        let _ = fs::remove_dir_all(&d);
        ControlDir::new(d)
    }

    #[test]
    fn lock_is_exclusive_and_stale_locks_are_taken_over() {
        let c = tmp_dir("lock");
        assert!(c.try_acquire(10, "daemon", &|_| true).is_ok());
        assert!(c.try_acquire(10, "daemon", &|_| true).is_ok(), "re-entrant");
        let held = c.try_acquire(20, "gui", &|pid| pid == 10).unwrap_err();
        assert_eq!(held.pid, 10);
        // Holder died.
        assert!(c.try_acquire(20, "gui", &|_| false).is_ok());
        assert_eq!(c.read_lock().unwrap().pid, 20);
        c.release(10); // not ours: no-op
        assert_eq!(c.read_lock().unwrap().pid, 20);
        c.release(20);
        assert!(c.read_lock().is_none());
        let _ = fs::remove_dir_all(&c.dir);
    }

    #[test]
    fn ledger_roundtrip_and_clear() {
        let c = tmp_dir("ledger");
        let entries = vec![
            Applied::Background { pid: 5, name: "rustc".into() },
            Applied::Paused { pid: 6, name: "weird\tname".into() },
        ];
        c.write_ledger(&entries).unwrap();
        let back = c.read_ledger();
        assert_eq!(back[0], entries[0]);
        assert_eq!(back[1], Applied::Paused { pid: 6, name: "weird name".into() });
        c.write_ledger(&[]).unwrap();
        assert!(c.read_ledger().is_empty());
        let _ = fs::remove_dir_all(&c.dir);
    }

    #[test]
    fn inbox_and_status_roundtrip() {
        let c = tmp_dir("inbox");
        c.send_command(&Command::Pause { pid: 7, secs: 30 }).unwrap();
        c.send_command(&Command::RestoreAll).unwrap();
        let cmds = c.take_commands();
        assert_eq!(cmds, vec![Command::Pause { pid: 7, secs: 30 }, Command::RestoreAll]);
        assert!(c.take_commands().is_empty());

        let st = Status {
            pid: 1,
            role: "daemon".into(),
            updated_unix: 99,
            demoted: vec![(5, "rustc".into())],
            paused: vec![(6, 12, "node".into())],
            events: vec![StatusEvent {
                seq: 3,
                kind: EventKind::Action,
                ts: "10:00:00".into(),
                msg: "Lowered priority".into(),
            }],
        };
        c.write_status(&st).unwrap();
        let back = c.read_status().unwrap();
        assert_eq!(back.demoted, st.demoted);
        assert_eq!(back.paused, st.paused);
        assert_eq!(back.events[0].seq, 3);
        assert_eq!(back.events[0].kind, EventKind::Action);
        let _ = fs::remove_dir_all(&c.dir);
    }
}
