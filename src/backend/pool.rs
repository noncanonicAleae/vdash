//! Backend pool (ROADMAP M2-04): picks a backend for each call from the role's
//! entries, bounds concurrency per backend, and pauses a backend that hits its
//! usage limit while the other keeps working.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use super::{AgentCall, AgentResult, Backend, Failure};
use crate::config::{BackendKind, Role, RoleEntry};
use crate::events;
use crate::paths::Ctx;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub base: Duration,
    pub cap: Duration,
    pub max_wait: Duration,
}

impl Timing {
    pub fn from_ctx(ctx: &Ctx) -> Self {
        Timing {
            base: Duration::from_secs(60),
            cap: Duration::from_secs(1800),
            max_wait: Duration::from_secs(ctx.cfg.budget.quota_max_wait_s),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Health {
    pub in_use: usize,
    pub max: usize,
    pub paused_until: Option<Instant>,
    /// When this backend first hit its limit in the current streak.
    pub paused_since: Option<Instant>,
    pub step: u32,
    pub unavailable: bool,
    pub calls: u64,
}

struct Inner {
    health: BTreeMap<BackendKind, Health>,
    /// Live role assignments from `set_role` (the `/model` command). They win
    /// over the `Ctx` a call was started with, so a change applies to the very
    /// next attempt, including a call that is retrying after a usage limit.
    overrides: BTreeMap<Role, Vec<RoleEntry>>,
}

pub struct Pool {
    backends: BTreeMap<BackendKind, Arc<dyn Backend>>,
    inner: Mutex<Inner>,
    cv: Condvar,
    pub timing: Timing,
    /// The engine's stop flag. Once set, a failed attempt is returned instead of
    /// retried and nobody waits for a paused backend (a kill must not spawn agents).
    stop: Mutex<Option<Arc<AtomicBool>>>,
}

/// Which backend actually served a call.
pub struct Served {
    pub result: AgentResult,
    pub entry: RoleEntry,
}

impl Pool {
    pub fn new(ctx: &Ctx, backends: Vec<Arc<dyn Backend>>) -> Self {
        let mut map = BTreeMap::new();
        let mut health = BTreeMap::new();
        for b in backends {
            let k = b.kind();
            let max = ctx.cfg.backends.get(&k).map(|c| c.max_concurrent).unwrap_or(1).max(1);
            health.insert(k, Health { max, ..Default::default() });
            map.insert(k, b);
        }
        Pool {
            backends: map,
            inner: Mutex::new(Inner { health, overrides: BTreeMap::new() }),
            cv: Condvar::new(),
            stop: Mutex::new(None),
            timing: Timing::from_ctx(ctx),
        }
    }

    /// The real CLIs named in the config.
    pub fn from_config(ctx: &Ctx) -> Self {
        let mut v: Vec<Arc<dyn Backend>> = Vec::new();
        for (k, c) in &ctx.cfg.backends {
            match k {
                BackendKind::Claude => v.push(Arc::new(super::claude::ClaudeCli::new(c.command.clone()))),
                BackendKind::Gemini => v.push(Arc::new(super::gemini::AgyCli::new(c.command.clone()))),
            }
        }
        Pool::new(ctx, v)
    }

    pub fn health(&self) -> BTreeMap<BackendKind, Health> {
        self.inner.lock().unwrap().health.clone()
    }

    pub fn mark_unavailable(&self, k: BackendKind) {
        if let Some(h) = self.inner.lock().unwrap().health.get_mut(&k) {
            h.unavailable = true;
        }
        self.cv.notify_all();
    }

    /// Use `entries` for `role` from the next pick on, for every call in flight.
    pub fn set_role(&self, role: Role, entries: Vec<RoleEntry>) {
        if entries.is_empty() {
            return;
        }
        self.inner.lock().unwrap().overrides.insert(role, entries);
        self.cv.notify_all();
    }

    /// Apply `set_role` messages still waiting in the control inbox. The engine
    /// only drains the inbox between agents; reading it here means a `/model`
    /// switch also reaches a call stuck behind a backend's usage limit.
    fn apply_pending_roles(&self, ctx: &Ctx) {
        for c in events::peek(ctx) {
            if let events::Control::SetRole { role, entries } = c {
                let Ok(r) = role.parse::<Role>() else { continue };
                let Ok(es) = entries.iter().map(|e| e.parse()).collect::<crate::error::Result<Vec<RoleEntry>>>() else {
                    continue;
                };
                self.set_role(r, es);
            }
        }
    }

    fn candidates(&self, ctx: &Ctx, role: Role) -> Vec<RoleEntry> {
        self.apply_pending_roles(ctx);
        let live = self.inner.lock().unwrap().overrides.get(&role).cloned();
        let mut out: Vec<RoleEntry> = live.unwrap_or_else(|| ctx.cfg.entries_for(role).to_vec());
        if ctx.cfg.policy.spillover {
            for k in BackendKind::ALL {
                if !out.iter().any(|e| e.backend == k) {
                    out.push(RoleEntry { backend: k, model: None });
                }
            }
        }
        out.retain(|e| self.backends.contains_key(&e.backend));
        out
    }

    /// Run one call for `role`, choosing a backend. Blocks while every candidate is
    /// busy or paused. `Err` only when every candidate stayed paused past the wait
    /// budget (exit code 4) or none is configured.
    /// Share the engine's stop flag with the pool.
    pub fn set_stop(&self, flag: Arc<AtomicBool>) {
        *self.stop.lock().unwrap() = Some(flag);
    }

    /// True once the engine has been asked to stop.
    pub fn stopping(&self) -> bool {
        self.stop.lock().unwrap().as_ref().is_some_and(|f| f.load(AtomicOrdering::SeqCst))
    }

    pub fn run(&self, ctx: &Ctx, round: u32, role: Role, call: &AgentCall) -> crate::error::Result<Served> {
        self.run_cb(ctx, round, role, call, &|_| {})
    }

    /// Like `run`, calling `on_pick` each time a backend is chosen for an attempt.
    pub fn run_cb(
        &self,
        ctx: &Ctx,
        round: u32,
        role: Role,
        call: &AgentCall,
        on_pick: &dyn Fn(&RoleEntry),
    ) -> crate::error::Result<Served> {
        let mut retried_transient: BTreeMap<BackendKind, bool> = BTreeMap::new();
        loop {
            let cands = self.candidates(ctx, role);
            if cands.is_empty() {
                return Err(crate::error::VdashError::new(
                    4,
                    format!("no available backend for role {role} (check [roles] and `vdash doctor`)"),
                ));
            }
            // Pick under the lock.
            let picked = {
                let mut g = self.inner.lock().unwrap();
                let now = Instant::now();
                let mut best: Option<(usize, RoleEntry)> = None;
                let mut all_paused = true;
                let mut earliest: Option<Instant> = None;
                for e in &cands {
                    let h = g.health.get(&e.backend).unwrap();
                    if h.unavailable {
                        continue;
                    }
                    if let Some(t) = h.paused_until {
                        if t > now {
                            earliest = Some(earliest.map_or(t, |x: Instant| x.min(t)));
                            continue;
                        }
                    }
                    all_paused = false;
                    let free = h.max.saturating_sub(h.in_use);
                    if free > 0 && best.as_ref().is_none_or(|(f, _)| free > *f) {
                        best = Some((free, e.clone()));
                    }
                }
                if cands.iter().all(|e| g.health.get(&e.backend).is_none_or(|h| h.unavailable)) {
                    return Err(crate::error::VdashError::new(
                        4,
                        format!("every backend for role {role} is unavailable (not logged in?)"),
                    ));
                }
                if all_paused {
                    let oldest = cands
                        .iter()
                        .filter_map(|e| g.health.get(&e.backend))
                        .filter(|h| !h.unavailable)
                        .filter_map(|h| h.paused_since)
                        .min();
                    if let Some(t) = oldest {
                        if now.duration_since(t) >= self.timing.max_wait {
                            return Err(crate::error::VdashError::new(
                                4,
                                format!("every backend for role {role} stayed at its usage limit past the wait budget"),
                            ));
                        }
                    }
                }
                match best {
                    Some((_, e)) => {
                        let h = g.health.get_mut(&e.backend).unwrap();
                        h.in_use += 1;
                        h.calls += 1;
                        Some(e)
                    }
                    None if self.stopping() => {
                        return Err(crate::error::VdashError::new(130, format!("stopped while waiting for a backend for role {role}")));
                    }
                    None => {
                        let wait = earliest
                            .map(|t| t.saturating_duration_since(now))
                            .unwrap_or(Duration::from_millis(500))
                            .clamp(Duration::from_millis(5), Duration::from_secs(1));
                        let _ = self.cv.wait_timeout(g, wait).unwrap();
                        None
                    }
                }
            };
            let Some(entry) = picked else { continue };
            on_pick(&entry);
            let backend = self.backends[&entry.backend].clone();
            let mut c = call.clone();
            c.model = entry.model.clone();
            let result = backend.run(ctx, &c);
            // Release and update health.
            let mut g = self.inner.lock().unwrap();
            let h = g.health.get_mut(&entry.backend).unwrap();
            h.in_use -= 1;
            let stopping = self.stopping();
            match &result.failure {
                // A stop (or kill) is under way: report the failure, never retry it.
                Some(_) if stopping => {
                    drop(g);
                    self.cv.notify_all();
                    return Ok(Served { result, entry });
                }
                Some(Failure::Quota) => {
                    h.paused_since.get_or_insert_with(Instant::now);
                    let d = (self.timing.base * 2u32.saturating_pow(h.step)).min(self.timing.cap);
                    h.step = h.step.saturating_add(1);
                    h.paused_until = Some(Instant::now() + d);
                    drop(g);
                    self.cv.notify_all();
                    events::emit(ctx, round, "backend_paused", json!({"backend": entry.backend.to_string(), "seconds": d.as_secs_f64()}));
                    continue;
                }
                Some(Failure::NotLoggedIn) => {
                    h.unavailable = true;
                    drop(g);
                    self.cv.notify_all();
                    events::emit(ctx, round, "note", json!({"text": format!("{} is not logged in; it is unavailable for this run", entry.backend)}));
                    continue;
                }
                Some(Failure::Timeout) | Some(Failure::Crash) if !retried_transient.get(&entry.backend).copied().unwrap_or(false) => {
                    retried_transient.insert(entry.backend, true);
                    drop(g);
                    self.cv.notify_all();
                    continue;
                }
                _ => {
                    let was_paused = h.step > 0;
                    if result.failure.is_none() {
                        h.step = 0;
                        h.paused_until = None;
                        h.paused_since = None;
                    }
                    drop(g);
                    self.cv.notify_all();
                    if was_paused && result.failure.is_none() {
                        events::emit(ctx, round, "backend_resumed", json!({"backend": entry.backend.to_string()}));
                    }
                    return Ok(Served { result, entry });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;
    use crate::backend::Profile;
    use crate::config::{self, BackendKind, Role};
    use crate::paths;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn ctx_with(tmp: &TempDir, extra: &str) -> Ctx {
        let host = tmp.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
        let camp = tmp.path().join("c");
        paths::init_campaign(&camp, &host, None, &BackendKind::ALL).unwrap();
        let p = camp.join("campaign.toml");
        let mut t = std::fs::read_to_string(&p).unwrap();
        t.push_str(extra);
        std::fs::write(&p, t).unwrap();
        let _ = config::load(&camp).unwrap();
        Ctx::for_test(&camp, tmp.path().join("state")).unwrap()
    }

    fn call(role: Role) -> AgentCall {
        AgentCall {
            role,
            prompt: "p".into(),
            skill: "s".into(),
            schema: json!({}),
            cwd: ".".into(),
            profile: Profile::Readonly,
            model: None,
            timeout: Duration::from_secs(5),
        }
    }

    fn fast(p: &mut Pool) {
        p.timing = Timing { base: Duration::from_millis(20), cap: Duration::from_millis(200), max_wait: Duration::from_millis(400) };
    }

    #[test]
    fn shared_role_spreads_across_backends_under_load() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_with(&tmp, "\n[backends.claude]\ncommand=[\"claude\"]\nmax_concurrent=2\n[backends.gemini]\ncommand=[\"agy\"]\nmax_concurrent=2\n");
        let c = Arc::new(FakeBackend::new(BackendKind::Claude).with_delay(Duration::from_millis(80)));
        let g = Arc::new(FakeBackend::new(BackendKind::Gemini).with_delay(Duration::from_millis(80)));
        c.always(Role::Worker, json!({"ok": 1}));
        g.always(Role::Worker, json!({"ok": 1}));
        let pool = Pool::new(&ctx, vec![c.clone(), g.clone()]);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| pool.run(&ctx, 1, Role::Worker, &call(Role::Worker)).unwrap());
            }
        });
        assert_eq!(c.count(Role::Worker) + g.count(Role::Worker), 8);
        assert!(c.count(Role::Worker) >= 2 && g.count(Role::Worker) >= 2);
    }

    #[test]
    fn concurrency_cap_per_backend() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_with(&tmp, "\n[backends.claude]\ncommand=[\"claude\"]\nmax_concurrent=2\n");
        struct Counting {
            now: AtomicUsize,
            max: AtomicUsize,
        }
        impl Backend for Counting {
            fn kind(&self) -> BackendKind {
                BackendKind::Claude
            }
            fn run(&self, _: &Ctx, _: &AgentCall) -> AgentResult {
                let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
                self.max.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                self.now.fetch_sub(1, Ordering::SeqCst);
                AgentResult::default()
            }
        }
        let b = Arc::new(Counting { now: AtomicUsize::new(0), max: AtomicUsize::new(0) });
        let pool = Pool::new(&ctx, vec![b.clone()]);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| pool.run(&ctx, 1, Role::Architect, &call(Role::Architect)).unwrap());
            }
        });
        assert_eq!(b.max.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn quota_pauses_one_backend_and_shared_work_flows_to_the_other() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_with(&tmp, "");
        let c = Arc::new(FakeBackend::new(BackendKind::Claude));
        let g = Arc::new(FakeBackend::new(BackendKind::Gemini));
        c.fail(Role::Worker, Failure::Quota);
        c.always(Role::Worker, json!({"from": "claude"}));
        g.always(Role::Worker, json!({"from": "gemini"}));
        let mut pool = Pool::new(&ctx, vec![c.clone(), g.clone()]);
        fast(&mut pool);
        // Worker is ["claude","gemini"] by default; claude has more... both have 4 free.
        let s = pool.run(&ctx, 1, Role::Worker, &call(Role::Worker)).unwrap();
        // first pick is claude (ties keep the first entry), it hits quota, then gemini serves.
        assert_eq!(s.entry.backend, BackendKind::Gemini);
        assert!(pool.health()[&BackendKind::Claude].paused_until.is_some());
        let evs = events::tail(&ctx.campaign.events_file(), 10).0;
        assert!(evs.iter().any(|e| e.kind == "backend_paused" && e.str("backend") == "claude"));
    }

    #[test]
    fn pinned_role_waits_then_spills_over_when_allowed() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_with(&tmp, "");
        let c = Arc::new(FakeBackend::new(BackendKind::Claude));
        let g = Arc::new(FakeBackend::new(BackendKind::Gemini));
        c.fail(Role::Critic, Failure::Quota).fail(Role::Critic, Failure::Quota);
        c.always(Role::Critic, json!({"from": "claude"}));
        g.always(Role::Critic, json!({"from": "gemini"}));
        let mut pool = Pool::new(&ctx, vec![c.clone(), g.clone()]);
        fast(&mut pool);
        let s = pool.run(&ctx, 1, Role::Critic, &call(Role::Critic)).unwrap();
        assert_eq!(s.entry.backend, BackendKind::Claude, "critic is pinned to claude and waits");
        assert_eq!(c.count(Role::Critic), 3);
        assert_eq!(g.count(Role::Critic), 0);
        // With spillover the other backend may serve.
        let tmp2 = TempDir::new().unwrap();
        let ctx2 = ctx_with(&tmp2, "\n[policy]\nspillover = true\n");
        let c2 = Arc::new(FakeBackend::new(BackendKind::Claude));
        let g2 = Arc::new(FakeBackend::new(BackendKind::Gemini));
        c2.fail(Role::Critic, Failure::Quota);
        g2.always(Role::Critic, json!({"from": "gemini"}));
        let mut pool2 = Pool::new(&ctx2, vec![c2, g2.clone()]);
        fast(&mut pool2);
        let s2 = pool2.run(&ctx2, 1, Role::Critic, &call(Role::Critic)).unwrap();
        assert_eq!(s2.entry.backend, BackendKind::Gemini);
    }

    #[test]
    fn pending_set_role_redirects_a_call_stuck_on_quota() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_with(&tmp, "");
        let c = Arc::new(FakeBackend::new(BackendKind::Claude));
        let g = Arc::new(FakeBackend::new(BackendKind::Gemini));
        for _ in 0..50 {
            c.fail(Role::Critic, Failure::Quota);
        }
        g.always(Role::Critic, json!({"from": "gemini"}));
        let mut pool = Pool::new(&ctx, vec![c.clone(), g.clone()]);
        pool.timing = Timing { base: Duration::from_millis(20), cap: Duration::from_millis(200), max_wait: Duration::from_secs(30) };
        let start = Instant::now();
        let s = std::thread::scope(|sc| {
            let h = sc.spawn(|| pool.run(&ctx, 1, Role::Critic, &call(Role::Critic)));
            // Critic is pinned to claude, which is at its limit; switch it mid-call.
            std::thread::sleep(Duration::from_millis(100));
            events::send(&ctx, &events::Control::SetRole { role: "critic".into(), entries: vec!["gemini".into()] }).unwrap();
            h.join().unwrap().unwrap()
        });
        assert_eq!(s.entry.backend, BackendKind::Gemini);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(c.count(Role::Critic) >= 1);
        // The message stays in the inbox for the engine to persist.
        assert_eq!(events::drain(&ctx).len(), 1);
    }

    #[test]
    fn backoff_doubles_and_wait_budget_gives_code_4() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_with(&tmp, "");
        let c = Arc::new(FakeBackend::new(BackendKind::Claude));
        for _ in 0..50 {
            c.fail(Role::Critic, Failure::Quota);
        }
        let mut pool = Pool::new(&ctx, vec![c.clone()]);
        fast(&mut pool);
        let start = Instant::now();
        let e = pool.run(&ctx, 1, Role::Critic, &call(Role::Critic)).err().unwrap();
        assert_eq!(e.code, 4);
        assert!(start.elapsed() >= Duration::from_millis(400));
        // 20, 40, 80, 160, (cap 200)... within ~400ms only a handful of attempts.
        assert!(c.count(Role::Critic) <= 6, "{}", c.count(Role::Critic));
    }

    #[test]
    fn stop_flag_ends_retries_and_quota_waits() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_with(&tmp, "");
        let c = Arc::new(FakeBackend::new(BackendKind::Claude));
        c.fail(Role::Critic, Failure::Crash);
        let mut pool = Pool::new(&ctx, vec![c.clone()]);
        fast(&mut pool);
        let stop = Arc::new(AtomicBool::new(true));
        pool.set_stop(stop.clone());
        // A crash (a killed agent) is reported, not retried.
        let s = pool.run(&ctx, 1, Role::Critic, &call(Role::Critic)).unwrap();
        assert_eq!(s.result.failure, Some(Failure::Crash));
        assert_eq!(c.count(Role::Critic), 1);
        // With the only backend paused, a stopping pool gives up instead of waiting.
        c.fail(Role::Critic, Failure::Quota);
        pool.timing.base = Duration::from_secs(30);
        pool.timing.cap = Duration::from_secs(30);
        pool.timing.max_wait = Duration::from_secs(120);
        stop.store(false, Ordering::SeqCst);
        let t = std::thread::spawn({
            let stop = stop.clone();
            move || {
                std::thread::sleep(Duration::from_millis(50));
                stop.store(true, Ordering::SeqCst);
            }
        });
        let started = Instant::now();
        let e = pool.run(&ctx, 1, Role::Critic, &call(Role::Critic)).err().expect("stopped");
        t.join().unwrap();
        assert_eq!(e.code, 130);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn transient_failure_retried_once_and_not_logged_in_disables() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx_with(&tmp, "");
        let c = Arc::new(FakeBackend::new(BackendKind::Claude));
        c.fail(Role::Critic, Failure::Crash).fail(Role::Critic, Failure::Crash);
        let pool = Pool::new(&ctx, vec![c.clone()]);
        let s = pool.run(&ctx, 1, Role::Critic, &call(Role::Critic)).unwrap();
        assert_eq!(s.result.failure, Some(Failure::Crash));
        assert_eq!(c.count(Role::Critic), 2);

        let g = Arc::new(FakeBackend::new(BackendKind::Gemini));
        let c2 = Arc::new(FakeBackend::new(BackendKind::Claude));
        g.fail(Role::Worker, Failure::NotLoggedIn);
        c2.always(Role::Worker, json!({}));
        g.always(Role::Worker, json!({}));
        let pool = Pool::new(&ctx, vec![c2, g.clone()]);
        // Force gemini first by making claude busy is awkward; run a few calls.
        for _ in 0..3 {
            pool.run(&ctx, 1, Role::Worker, &call(Role::Worker)).unwrap();
        }
        let h = pool.health();
        if g.count(Role::Worker) > 0 {
            assert!(h[&BackendKind::Gemini].unavailable);
            assert_eq!(g.count(Role::Worker), 1);
        }
    }
}
