use std::fs::{self, File, OpenOptions};
use std::thread;
use std::time::Duration;

use rustix::fs::{flock, FlockOperation};

use crate::error::{VdashError, Result};
use crate::paths::Ctx;

pub struct SlotGuard {
    file: File,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

pub fn acquire(ctx: &Ctx) -> Result<SlotGuard> {
    let slots_dir = ctx.campaign.build_slots_dir();
    fs::create_dir_all(&slots_dir).map_err(|e| VdashError::io_path(&slots_dir, e))?;
    let max = ctx.cfg.budget.max_parallel_lean.max(1);
    let start = (std::process::id() as usize) % max;

    loop {
        for offset in 0..max {
            let i = (start + offset) % max;
            let lock_path = slots_dir.join(format!("{i}.lock"));
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|e| VdashError::io_path(&lock_path, e))?;
            if flock(&file, FlockOperation::NonBlockingLockExclusive).is_ok() {
                return Ok(SlotGuard { file });
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::paths;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn slots_bound_parallel_holders() {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();

        let camp = tmp.path().join("camp");
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        paths::init_campaign(&camp, &host, Some("isort"), &[BackendKind::Claude]).unwrap();

        let mut ctx = Ctx::for_test(&camp, &state).unwrap();
        ctx.cfg.budget.max_parallel_lean = 2;
        let ctx = Arc::new(ctx);

        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let ctx = Arc::clone(&ctx);
            let active = Arc::clone(&active);
            let max_seen = Arc::clone(&max_seen);
            handles.push(thread::spawn(move || {
                let _guard = acquire(&ctx).unwrap();
                let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(cur, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(50));
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(max_seen.load(Ordering::SeqCst) <= 2);
    }
}
