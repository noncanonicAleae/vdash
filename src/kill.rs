//! `vdash kill`: end a campaign's engine for good, agent subprocesses included.
//!
//! `stop` is cooperative (the engine finishes its current agent first). `kill`
//! asks the engine to stop (SIGTERM, so it can still revert the round's edits),
//! kills every descendant process at once, and SIGKILLs the engine if it has not
//! exited after a grace period. Unreverted edits from a killed round are caught
//! by the next round's snapshot check, like any other agent edit.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{kill_process, kill_process_group, test_kill_process, Pid, Signal};

use crate::error::{Result, VdashError};
use crate::events;
use crate::paths::Ctx;

const GRACE: Duration = Duration::from_secs(8);

fn pid_of(n: u32) -> Option<Pid> {
    Pid::from_raw(n as i32)
}

fn alive(n: u32) -> bool {
    pid_of(n).is_some_and(|p| test_kill_process(p).is_ok())
}

/// `(pid, ppid)` of every process, from `/proc`.
fn process_table() -> Vec<(u32, u32)> {
    let Ok(rd) = fs::read_dir("/proc") else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let stat = fs::read_to_string(e.path().join("stat")).ok()?;
            // "pid (comm) S ppid ..." where comm may contain spaces and parens.
            let rest = &stat[stat.rfind(')')? + 1..];
            let ppid: u32 = rest.split_whitespace().nth(1)?.parse().ok()?;
            Some((pid, ppid))
        })
        .collect()
}

/// All transitive children of `root`.
pub fn descendants(root: u32) -> Vec<u32> {
    let table = process_table();
    let mut found = vec![root];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        for &(pid, ppid) in &table {
            if ppid == parent && !found.contains(&pid) {
                found.push(pid);
            }
        }
        i += 1;
    }
    found.remove(0);
    found
}

fn kill_all(pids: &[u32], sig: Signal) {
    for &n in pids {
        if let Some(p) = pid_of(n) {
            // Agents run in their own process group; take the whole group too.
            let _ = kill_process_group(p, sig);
            let _ = kill_process(p, sig);
        }
    }
}

/// Kill the campaign's engine and everything it started. Returns a summary.
pub fn kill(ctx: &Ctx) -> Result<String> {
    let Some(engine) = events::engine_running(ctx) else {
        return Err(VdashError::new(2, "no engine is running for this campaign"));
    };
    let mut all = descendants(engine);
    if let Some(p) = pid_of(engine) {
        let _ = kill_process(p, Signal::Term);
    }
    kill_all(&all, Signal::Term);
    // Keep collecting while the engine lives: a process it starts during the grace
    // period would otherwise be reparented to init when the engine dies, and survive.
    let collect = |all: &mut Vec<u32>| {
        for k in descendants(engine) {
            if !all.contains(&k) {
                all.push(k);
            }
        }
    };
    let start = Instant::now();
    while alive(engine) && start.elapsed() < GRACE {
        collect(&mut all);
        thread::sleep(Duration::from_millis(100));
    }
    let forced = alive(engine);
    if let Some(p) = pid_of(engine) {
        // Freeze it so it cannot start anything between the last sweep and its death.
        let _ = kill_process(p, Signal::Stop);
        collect(&mut all);
        let _ = kill_process(p, Signal::Kill);
    }
    kill_all(&all, Signal::Kill);
    thread::sleep(Duration::from_millis(200));
    events::emit(ctx, 0, "engine_killed", serde_json::json!({"pid": engine, "forced": forced}));
    let leftover = all.iter().filter(|&&n| alive(n)).count();
    Ok(format!(
        "vdash: killed engine {engine} and {} subprocess(es){}{}",
        all.len(),
        if forced { " (had to SIGKILL the engine)" } else { "" },
        if leftover > 0 { format!("; {leftover} still alive") } else { String::new() }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn finds_and_kills_descendants() {
        let mut sh = Command::new("sh").args(["-c", "sleep 60 & sleep 60 & wait"]).spawn().unwrap();
        thread::sleep(Duration::from_millis(300));
        let kids = descendants(sh.id());
        assert!(kids.len() >= 2, "{kids:?}");
        kill_all(&kids, Signal::Kill);
        let _ = sh.kill();
        let _ = sh.wait();
        thread::sleep(Duration::from_millis(200));
        // Reaped by init or gone: not running as children of sh any more.
        assert!(descendants(sh.id()).is_empty());
    }

    #[test]
    fn kill_ends_a_lock_holder_and_its_children() {
        let tmp = tempfile::TempDir::new().unwrap();
        let camp = tmp.path().join("c");
        let state = tmp.path().join("s");
        let host = tmp.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
        crate::paths::init_campaign(&camp, &host, None, &crate::config::BackendKind::ALL).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let ctx = Ctx::for_test(&camp, &state).unwrap();
        assert!(kill(&ctx).is_err(), "no engine yet");
        let lock = state.join("engine.lock");
        // A stand-in engine: holds the engine lock, records its pid, runs a child.
        let mut eng = Command::new("flock")
            .arg(&lock)
            .args(["sh", "-c"])
            // Ignores SIGTERM like a busy engine, and starts a "retry" child after the
            // kill began; that late child must not survive as an orphan.
            .arg(format!("trap '' TERM; echo $$ > {}; sleep 60 & sleep 1; (exec -a vdash-late-child-$$ sleep 61) & wait", lock.display()))
            .spawn()
            .unwrap();
        let start = Instant::now();
        while events::engine_running(&ctx).is_none() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(50));
        }
        let pid = events::engine_running(&ctx).expect("stand-in engine holds the lock");
        let msg = kill(&ctx).unwrap();
        assert!(msg.contains(&pid.to_string()), "{msg}");
        assert!(msg.contains("had to SIGKILL"), "{msg}");
        thread::sleep(Duration::from_millis(300));
        let late = format!("vdash-late-child-{pid}");
        let survivors: Vec<u32> = process_table()
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| fs::read(format!("/proc/{p}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains(&late)) && alive(*p))
            .collect();
        assert!(survivors.is_empty(), "late child survived the kill: {survivors:?}");
        let _ = eng.wait();
        assert!(events::engine_running(&ctx).is_none());
        assert!(!alive(pid));
    }
}
