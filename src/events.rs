//! Event log, engine lock and control inbox (ROADMAP M2-08, §3.8, §3.9).
//!
//! The engine appends one JSON line per happening to `logs/events.jsonl`; the app
//! follows the file. The app talks back through small files in the state
//! directory's `inbox/`, which agents cannot reach under the policy snapshot.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{flock, FlockOperation};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{VdashError, Result};
use crate::jsonio;
use crate::paths::Ctx;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub ts: String,
    pub round: u32,
    pub kind: String,
    #[serde(flatten)]
    pub data: Map<String, Value>,
}

impl Event {
    pub fn new(round: u32, kind: &str, data: Value) -> Self {
        let data = match data {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            other => {
                let mut m = Map::new();
                m.insert("value".into(), other);
                m
            }
        };
        Event { ts: crate::util::now_iso(), round, kind: kind.to_string(), data }
    }

    pub fn str(&self, k: &str) -> &str {
        self.data.get(k).and_then(Value::as_str).unwrap_or("")
    }

    pub fn f64(&self, k: &str) -> Option<f64> {
        self.data.get(k).and_then(Value::as_f64)
    }

    pub fn u64(&self, k: &str) -> Option<u64> {
        self.data.get(k).and_then(Value::as_u64)
    }
}

pub fn emit(ctx: &Ctx, round: u32, kind: &str, data: Value) {
    let ev = Event::new(round, kind, data);
    if let Ok(line) = serde_json::to_string(&ev) {
        let _ = jsonio::append_line(ctx.campaign.events_file(), &line);
    }
}

/// Read the complete lines appended since `offset`. A trailing partial line is left
/// for the next call. An unparsable line becomes a `note` event.
pub fn follow(path: &Path, offset: u64) -> (Vec<Event>, u64) {
    let Ok(mut f) = File::open(path) else { return (Vec::new(), offset) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let offset = if len < offset { 0 } else { offset };
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return (Vec::new(), offset);
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return (Vec::new(), offset);
    }
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return (Vec::new(), offset);
    };
    let complete = &buf[..=last_nl];
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(complete).lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(line) {
            Ok(e) => out.push(e),
            Err(_) => out.push(Event::new(0, "note", serde_json::json!({"text": line}))),
        }
    }
    (out, offset + complete.len() as u64)
}

/// The last `n` events of the log, and the offset at its end.
pub fn tail(path: &Path, n: usize) -> (Vec<Event>, u64) {
    let (mut all, off) = follow(path, 0);
    if all.len() > n {
        all.drain(..all.len() - n);
    }
    (all, off)
}

// ---------------------------------------------------------------- engine lock

pub struct EngineLock {
    file: File,
    path: PathBuf,
}

impl Drop for EngineLock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = flock(&self.file, FlockOperation::Unlock);
        let _ = &self.path;
    }
}

fn lock_path(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("engine.lock")
}

impl EngineLock {
    pub fn acquire(ctx: &Ctx) -> Result<EngineLock> {
        fs::create_dir_all(&ctx.state_dir).map_err(|e| VdashError::io_path(&ctx.state_dir, e))?;
        let path = lock_path(ctx);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| VdashError::io_path(&path, e))?;
        if flock(&file, FlockOperation::NonBlockingLockExclusive).is_err() {
            let pid = engine_running(ctx).unwrap_or(0);
            return Err(VdashError::new(2, format!("an engine is already running (pid {pid})")));
        }
        file.set_len(0).map_err(|e| VdashError::io_path(&path, e))?;
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();
        Ok(EngineLock { file, path })
    }
}

/// The pid of the running engine, if one holds the lock.
pub fn engine_running(ctx: &Ctx) -> Option<u32> {
    let path = lock_path(ctx);
    let file = OpenOptions::new().read(true).write(true).open(&path).ok()?;
    if flock(&file, FlockOperation::NonBlockingLockShared).is_ok() {
        let _ = flock(&file, FlockOperation::Unlock);
        return None;
    }
    let s = fs::read_to_string(&path).ok()?;
    s.trim().parse().ok()
}

// ---------------------------------------------------------------- control inbox

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Control {
    Pause,
    Resume,
    Stop,
    SetRole { role: String, entries: Vec<String> },
    Guidance { text: String },
    AmendDecision { n: u32, approve: bool },
}

fn inbox_dir(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("inbox")
}

static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn send(ctx: &Ctx, c: &Control) -> Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!("{nanos:024}-{:06}-{seq:06}.json", std::process::id());
    jsonio::write_atomic(inbox_dir(ctx).join(name), c)
}

/// Pending control messages in send order, left in the inbox (the engine still
/// drains them). The backend pool uses this to apply `set_role` immediately.
pub fn peek(ctx: &Ctx) -> Vec<Control> {
    let Ok(rd) = fs::read_dir(inbox_dir(ctx)) else { return Vec::new() };
    let mut names: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    names.sort();
    names.iter().filter_map(|p| jsonio::read_json::<Control>(p).ok()).collect()
}

/// Pending control messages in send order; each is moved to `inbox/done/`.
pub fn drain(ctx: &Ctx) -> Vec<Control> {
    let dir = inbox_dir(ctx);
    let Ok(rd) = fs::read_dir(&dir) else { return Vec::new() };
    let mut names: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    names.sort();
    let done = dir.join("done");
    let _ = fs::create_dir_all(&done);
    let mut out = Vec::new();
    for p in names {
        let parsed: Result<Control> = jsonio::read_json(&p);
        if let Some(fname) = p.file_name() {
            let _ = fs::rename(&p, done.join(fname));
        }
        if let Ok(c) = parsed {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::paths;
    use tempfile::TempDir;

    fn ctx(tmp: &TempDir) -> Ctx {
        let camp = tmp.path().join("c");
        paths::init_campaign(&camp, "/nonexistent-lean-host", None, &[BackendKind::Claude])
            .or_else(|_| {
                // Host may be missing on other machines: fake one.
                let host = tmp.path().join("host");
                fs::create_dir_all(&host).unwrap();
                fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
                paths::init_campaign(&camp, &host, None, &[BackendKind::Claude])
            })
            .unwrap();
        Ctx::for_test(&camp, tmp.path().join("state")).unwrap()
    }

    #[test]
    fn emit_follow_round_trip_with_partial_line() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(&tmp);
        emit(&c, 1, "stage", serde_json::json!({"stage": "plan"}));
        emit(&c, 1, "note", serde_json::json!({"text": "hi"}));
        let p = c.campaign.events_file();
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{{\"ts\":\"x\",\"round\":1,\"kind\":\"no").unwrap();
        let (evs, off) = follow(&p, 0);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].str("stage"), "plan");
        writeln!(f, "te\",\"text\":\"late\"}}").unwrap();
        let (evs2, off2) = follow(&p, off);
        assert_eq!(evs2.len(), 1);
        assert_eq!(evs2[0].str("text"), "late");
        assert!(off2 > off);
        assert!(follow(&p, off2).0.is_empty());
    }

    #[test]
    fn engine_lock_is_exclusive() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(&tmp);
        assert_eq!(engine_running(&c), None);
        let l = EngineLock::acquire(&c).unwrap();
        assert_eq!(engine_running(&c), Some(std::process::id()));
        let e = EngineLock::acquire(&c).err().unwrap();
        assert_eq!(e.code, 2);
        drop(l);
        assert_eq!(engine_running(&c), None);
        let _l2 = EngineLock::acquire(&c).unwrap();
    }

    #[test]
    fn inbox_order_and_single_delivery() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(&tmp);
        send(&c, &Control::Pause).unwrap();
        send(&c, &Control::Guidance { text: "try induction".into() }).unwrap();
        send(&c, &Control::Stop).unwrap();
        let got = drain(&c);
        assert_eq!(
            got,
            vec![Control::Pause, Control::Guidance { text: "try induction".into() }, Control::Stop]
        );
        assert!(drain(&c).is_empty());
    }
}
