//! The proof engine (ROADMAP M2-11, M2-12, M3): the round loop, stages, control
//! messages, and resume after a kill.

pub mod plan;
pub mod team;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::backend::pool::Pool;
use crate::config::{self, Role, RoleEntry};
use crate::error::{VdashError, Result};
use crate::events::{self, Control, EngineLock};
use crate::gate;
use crate::paths::{Ctx, State, TargetState};
use crate::policy::{self, Scope, Snapshot};
use crate::util;

pub const TEAM_STAGES: [&str; 8] =
    ["gate_pre", "plan", "orchestrate", "leads", "verify", "integrate", "amend", "gate_post"];
pub const SINGLE_STAGES: [&str; 4] = ["gate_pre", "work", "verify", "gate_post"];

#[derive(Debug, Clone, Default)]
pub struct Opts {
    pub rounds: Option<u32>,
    pub single: bool,
}

pub enum Flow {
    Continue,
    Stop,
}

pub struct Engine<'p> {
    pub ctx: Ctx,
    pub pool: &'p Pool,
    pub stop: Arc<AtomicBool>,
    pub opts: Opts,
    /// Snapshot taken before this round's agents ran (None after a resume).
    pub snap: Option<Snapshot>,
    /// Amendment decisions received from the user, for the amend stage.
    pub decisions: Vec<(u32, crate::amend::Decision)>,
    /// Set once the engine has re-verified every "proved" goal in this run.
    pub rechecked: bool,
    /// Last gate verdict keyed by `gate::input_digest`, avoiding duplicate runs
    /// between `gate_post` and the next round's `gate_pre` (or unchanged rounds).
    last_gate: Option<(String, gate::Verdict)>,
}

impl<'p> Engine<'p> {
    pub fn new(ctx: Ctx, pool: &'p Pool, opts: Opts, stop: Arc<AtomicBool>) -> Self {
        pool.set_stop(Arc::clone(&stop));
        Engine { ctx, pool, stop, opts, snap: None, decisions: Vec::new(), rechecked: false, last_gate: None }
    }

    pub fn state(&self) -> Result<State> {
        util::load_state(&self.ctx.campaign)
    }

    pub fn update(&self, f: impl FnOnce(&mut State)) -> Result<State> {
        util::update_state(&self.ctx.campaign, f)
    }

    pub fn note(&self, round: u32, text: impl Into<String>) {
        events::emit(&self.ctx, round, "note", json!({"text": text.into()}));
    }

    pub fn bulletin(&self, round: u32, who: &str, text: &str) {
        let line = format!("- round {round} · {who}: {}", text.trim().replace('\n', " "));
        let _ = util::append_bulletin(&self.ctx.campaign, &line);
        events::emit(&self.ctx, round, "bulletin", json!({"text": format!("{who}: {text}")}));
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Handle control messages (M2-11). Blocks while paused.
    pub fn checkpoint(&mut self, round: u32) -> Result<Flow> {
        let mut paused = false;
        loop {
            for c in events::drain(&self.ctx) {
                match c {
                    Control::Pause => {
                        if !paused {
                            self.note(round, "paused by the user");
                            events::emit(&self.ctx, round, "paused", json!({}));
                        }
                        paused = true;
                    }
                    Control::Resume => {
                        if paused {
                            self.note(round, "resumed");
                            events::emit(&self.ctx, round, "resumed", json!({}));
                        }
                        paused = false;
                    }
                    Control::Stop => {
                        self.note(round, "stop requested by the user");
                        self.stop.store(true, Ordering::SeqCst);
                    }
                    Control::SetRole { role, entries } => match self.set_role(&role, &entries) {
                        Ok(()) => self.note(round, format!("role {role} now uses {}", entries.join(", "))),
                        Err(e) => self.note(round, format!("could not set role {role}: {}", e.message)),
                    },
                    Control::Guidance { text } => self.add_guidance(round, &text)?,
                    Control::ResolveEscalation { ns, answer } => {
                        match crate::escalation::resolve(&self.ctx, round, &ns, &answer, "user") {
                            Ok(r) => {
                                for rel in &r.rels {
                                    self.own(rel);
                                }
                                let list = r.closed.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ");
                                let how = if r.guidance.is_empty() { "dismissed" } else { "answered" };
                                self.note(round, format!("escalation {list} {how} by the user"));
                                if !r.guidance.is_empty() {
                                    self.add_guidance(round, &r.guidance)?;
                                }
                            }
                            Err(e) => self.note(round, format!("could not resolve escalation: {}", e.message)),
                        }
                    }
                    Control::AmendDecision { n, approve, accept_weakening } => {
                        use crate::amend::Decision;
                        let d = match (approve, accept_weakening) {
                            (false, _) => Decision::Reject,
                            (true, false) => Decision::Approve,
                            (true, true) => Decision::AcceptWeakening,
                        };
                        self.decisions.push((n, d));
                        let what = match d {
                            Decision::Reject => "rejected",
                            Decision::Approve => "approved",
                            Decision::AcceptWeakening => "approved, accepting the weaker guarantee,",
                        };
                        self.note(round, format!("amendment {n}: {what} by the user"));
                    }
                }
            }
            if self.stopping() {
                return Ok(Flow::Stop);
            }
            if !paused {
                return Ok(Flow::Continue);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Queue guidance for the orchestrator's next call and append it to STRATEGY.md.
    fn add_guidance(&mut self, round: u32, text: &str) -> Result<()> {
        self.update(|s| s.pending_guidance.push(text.to_string()))?;
        let p = self.ctx.campaign.strategy_file();
        let mut cur = std::fs::read_to_string(&p).unwrap_or_default();
        if !cur.is_empty() && !cur.ends_with('\n') {
            cur.push('\n');
        }
        cur.push_str(&format!("\n## Guidance ({})\n\n{}\n", util::now_iso(), text.trim()));
        std::fs::write(&p, cur).map_err(|e| VdashError::io_path(&p, e))?;
        self.own("STRATEGY.md");
        self.note(round, "guidance queued for the orchestrator's next call");
        Ok(())
    }

    fn set_role(&mut self, role: &str, entries: &[String]) -> Result<()> {
        let r: Role = role.parse()?;
        let es: Vec<RoleEntry> = entries.iter().map(|e| e.parse()).collect::<Result<_>>()?;
        config::set_role(&self.ctx.campaign.root, r, &es)?;
        self.pool.set_role(r, es);
        self.ctx.cfg = config::load(&self.ctx.campaign.root)?;
        self.own("campaign.toml");
        Ok(())
    }

    /// Mark a file the engine itself just wrote (plan, config, guidance) as accepted:
    /// in the in-memory snapshot, and by hash in `state.json`, so that enforcing
    /// against git after a restart in mid-round does not revert it.
    pub fn own(&mut self, rel: &str) {
        if let Some(s) = self.snap.as_mut() {
            s.accept(&self.ctx, rel);
        }
        record_engine_file(&self.ctx, rel);
    }

    /// Run the gatekeeper and record per-target verdicts, reusing the previous
    /// verdict in memory when `gate::input_digest` and the trust anchor are unchanged.
    pub fn gate(&mut self, round: u32) -> Result<gate::Verdict> {
        let use_cache = self.ctx.cfg.policy.comparator_cmd.trim().is_empty()
            && crate::anchor::verify(&self.ctx).is_empty();
        if use_cache {
            if let Ok(digest) = gate::input_digest(&self.ctx) {
                if let Some((ref prev_digest, ref prev_verdict)) = self.last_gate {
                    if prev_digest == &digest {
                        let v = prev_verdict.clone();
                        record_verdict(&self.ctx, round, &v)?;
                        return Ok(v);
                    }
                }
                let v = gate::run(&self.ctx)?;
                self.last_gate = Some((digest, v.clone()));
                record_verdict(&self.ctx, round, &v)?;
                return Ok(v);
            }
        }
        self.last_gate = None;
        let v = gate::run(&self.ctx)?;
        record_verdict(&self.ctx, round, &v)?;
        Ok(v)
    }

    /// Restore every change not in `allowed` (Proof scope). Uses the in-memory
    /// snapshot, or git after a resume.
    pub fn enforce(&mut self, round: u32, allowed: &BTreeSet<String>) -> Result<usize> {
        let vs = match &self.snap {
            Some(s) => policy::enforce(&self.ctx, s, Scope::Proof, allowed)?,
            None => enforce_against_git(&self.ctx, allowed)?,
        };
        policy::report(&self.ctx, round, &vs);
        Ok(vs.len())
    }
}

/// Write a gate verdict into state.json and emit a `gate` event.
pub fn record_verdict(ctx: &Ctx, round: u32, v: &gate::Verdict) -> Result<()> {
    util::update_state(&ctx.campaign, |s| {
        let mut targets = BTreeMap::new();
        for (name, tv) in &v.targets {
            let prev = s.targets.get(name);
            let since = if tv.accepted {
                prev.and_then(|p| if p.accepted { p.since_round } else { None }).or(Some(round))
            } else {
                None
            };
            targets.insert(
                name.clone(),
                TargetState { accepted: tv.accepted, reasons: tv.reasons.iter().map(|r| r.to_string()).collect(), since_round: since },
            );
        }
        if !targets.is_empty() {
            s.targets = targets;
        }
        // A target goal is proved exactly when the gate accepts it.
        for g in s.goals.values_mut() {
            if let Some(t) = s.targets.get(&g.lean_name) {
                if t.accepted {
                    g.status = "proved".into();
                    g.verified_round.get_or_insert(round);
                } else if g.status == "proved" {
                    g.status = "open".into();
                    g.verified_round = None;
                }
            }
        }
    })?;
    let accepted = v.targets.values().filter(|t| t.accepted).count();
    let targets: serde_json::Map<String, Value> = v
        .targets
        .iter()
        .map(|(k, t)| (k.clone(), json!({"accepted": t.accepted, "reasons": t.reasons.iter().map(|r| r.to_string()).collect::<Vec<_>>()})))
        .collect();
    events::emit(ctx, round, "gate", json!({"accepted": accepted, "total": v.targets.len(), "anchor_ok": v.anchor_ok, "targets": targets}));
    Ok(())
}

/// After a resume there is no in-memory snapshot: restore disallowed changes from
/// the last commit instead (the engine commits at the end of every round).
/// Record the hash of an engine-written campaign file for this round (see `Engine::own`).
pub fn record_engine_file(ctx: &Ctx, rel: &str) {
    let Ok(bytes) = std::fs::read(ctx.campaign.root.join(rel)) else {
        return;
    };
    let h = util::sha256_hex(&bytes);
    let _ = util::update_state(&ctx.campaign, |s| {
        if !s.round_data.is_object() {
            s.round_data = json!({});
        }
        if !s.round_data["engine_files"].is_object() {
            s.round_data["engine_files"] = json!({});
        }
        s.round_data["engine_files"][rel] = json!(h);
    });
}

pub fn enforce_against_git(ctx: &Ctx, allowed: &BTreeSet<String>) -> Result<Vec<policy::Violation>> {
    let engine_files = util::load_state(&ctx.campaign).map(|s| s.round_data["engine_files"].clone()).unwrap_or_default();
    let written_by_engine = |path: &str| {
        engine_files[path].as_str().is_some_and(|h| {
            std::fs::read(ctx.campaign.root.join(path)).is_ok_and(|b| util::sha256_hex(&b) == h)
        })
    };
    let out = util::git(&ctx.campaign, &["status", "--porcelain", "-uall", "--no-renames"])?;
    let mut vs = Vec::new();
    for line in out.lines() {
        if line.len() < 4 {
            continue;
        }
        let code = &line[..2];
        let path = line[3..].trim_matches('"').to_string();
        // Engine-protected live files, and the held-out copy (the gate checks it
        // against the anchor, and it may not be committed yet).
        if path == "state.json" || path == "bulletin.md" || path.starts_with(".heldout/") {
            continue;
        }
        // Written by the engine earlier this round (before a restart), unchanged since.
        if written_by_engine(&path) {
            continue;
        }
        let ok = allowed.iter().any(|a| if a.ends_with('/') { path.starts_with(a.as_str()) } else { a == &path })
            && (path == "lean/Apc/Solution.lean" || (path.starts_with("lean/Apc/Proof/") && path.ends_with(".lean")));
        if ok {
            continue;
        }
        if code == "??" {
            let _ = std::fs::remove_file(ctx.campaign.root.join(&path));
            vs.push(policy::Violation { path, change: "created" });
        } else {
            util::git(&ctx.campaign, &["checkout", "HEAD", "--", &path])?;
            vs.push(policy::Violation { path, change: "modified" });
        }
    }
    Ok(vs)
}

fn all_accepted(s: &State) -> bool {
    !s.targets.is_empty() && s.targets.values().all(|t| t.accepted)
}

/// Run the engine until every target is accepted, the round budget is used, or a
/// stop is requested. Returns the process exit code.
pub fn run(ctx: Ctx, pool: &Pool, opts: Opts, stop: Arc<AtomicBool>) -> Result<u8> {
    let _lock = EngineLock::acquire(&ctx)?;
    let st = util::load_state(&ctx.campaign)?;
    if !matches!(st.phase.as_str(), "approved" | "proving" | "done") {
        return Err(VdashError::new(2, format!("the spec is not approved yet (phase {}); run `vdash approve` first", st.phase)));
    }
    let mut eng = Engine::new(ctx, pool, opts, stop);
    events::emit(&eng.ctx, st.round, "engine_started", json!({"pid": std::process::id(), "mode": if eng.opts.single {"single"} else {"team"}}));
    let cache = crate::models::load(&eng.ctx.models_cache);
    for w in crate::models::role_warnings(&eng.ctx.cfg, &cache, crate::models::now_unix()) {
        events::emit(&eng.ctx, st.round, "note", json!({"text": format!("model check: {w}")}));
    }
    let res = eng.main_loop();
    let (code, reason) = match &res {
        Ok(0) => (0, "all targets accepted".to_string()),
        Ok(130) => (130, "stopped".to_string()),
        Ok(c) => (*c, "round budget used".to_string()),
        Err(e) => (e.code, e.message.clone()),
    };
    let round = eng.state().map(|s| s.round).unwrap_or(0);
    events::emit(&eng.ctx, round, "engine_stopped", json!({"reason": reason, "code": code}));
    res
}

impl<'p> Engine<'p> {
    fn main_loop(&mut self) -> Result<u8> {
        let stages: &[&str] = if self.opts.single { &SINGLE_STAGES } else { &TEAM_STAGES };
        let max_rounds = self.opts.rounds.unwrap_or(self.ctx.cfg.budget.max_rounds);
        let mut rounds_run = 0u32;
        self.update(|s| {
            if s.phase == "approved" {
                s.phase = "proving".into();
            }
        })?;
        loop {
            let st = self.state()?;
            let (round, start) = match stages.iter().position(|x| *x == st.stage) {
                Some(i) if st.stage != "idle" => {
                    self.note(st.round, format!("resuming round {} at stage {}", st.round, st.stage));
                    self.snap = None;
                    (st.round, i)
                }
                _ => {
                    let r = st.round + 1;
                    let carry = st.round_data.get("next_revise_plan").and_then(Value::as_bool).unwrap_or(false);
                    self.snap = None;
                    self.update(|s| {
                        s.round = r;
                        s.round_data = json!({"revise_plan": carry});
                    })?;
                    (r, 0)
                }
            };
            for stage in &stages[start..] {
                if let Flow::Stop = self.checkpoint(round)? {
                    return Ok(130);
                }
                self.update(|s| s.stage = stage.to_string())?;
                events::emit(&self.ctx, round, "stage", json!({"stage": stage}));
                let done = self.run_stage(round, stage)?;
                if done {
                    self.finish_round(round)?;
                    self.update(|s| s.phase = "done".into())?;
                    return Ok(0);
                }
            }
            self.finish_round(round)?;
            rounds_run += 1;
            let st = self.state()?;
            if all_accepted(&st) {
                self.update(|s| s.phase = "done".into())?;
                return Ok(0);
            }
            if rounds_run >= max_rounds {
                self.note(round, format!("round budget used ({rounds_run} rounds)"));
                return Ok(1);
            }
        }
    }

    fn finish_round(&mut self, round: u32) -> Result<()> {
        self.update(|s| s.stage = "idle".into())?;
        let st = self.state()?;
        let accepted = st.targets.values().filter(|t| t.accepted).count();
        let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
        for g in st.goals.values() {
            *by_status.entry(g.status.clone()).or_default() += 1;
        }
        let cost: f64 = st.usage.values().flat_map(|m| m.values()).map(|u| u.cost_usd).sum();
        let calls: u64 = st.usage.values().flat_map(|m| m.values()).map(|u| u.calls).sum();
        let goals = by_status.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(", ");
        let para = format!(
            "\n## Round {round} ({})\n\n{accepted}/{} targets accepted. Goals: {}. Agent calls so far: {calls}, cost so far: ${cost:.2}.\n",
            util::now_iso(),
            st.targets.len(),
            if goals.is_empty() { "none".into() } else { goals }
        );
        let p = self.ctx.campaign.status_log_file();
        let mut cur = std::fs::read_to_string(&p).unwrap_or_default();
        cur.push_str(&para);
        std::fs::write(&p, cur).map_err(|e| VdashError::io_path(&p, e))?;
        util::commit_all(&self.ctx.campaign, &format!("vdash: round {round}"))?;
        events::emit(&self.ctx, round, "round_done", json!({"accepted": accepted, "total": st.targets.len()}));
        Ok(())
    }

    /// Returns true when every target is accepted and the run should end now.
    fn run_stage(&mut self, round: u32, stage: &str) -> Result<bool> {
        match stage {
            "gate_pre" => {
                let v = self.gate(round)?;
                if !v.anchor_ok {
                    return Err(VdashError::new(3, format!("trust anchor check failed: {}", v.global_reasons.join("; "))));
                }
                Ok(!v.targets.is_empty() && v.targets.values().all(|t| t.accepted))
            }
            "gate_post" => {
                let v = self.gate(round)?;
                if !v.anchor_ok {
                    return Err(VdashError::new(3, format!("trust anchor check failed: {}", v.global_reasons.join("; "))));
                }
                Ok(false)
            }
            "work" => {
                self.single_work(round)?;
                Ok(false)
            }
            "verify" if self.opts.single => {
                let allowed: BTreeSet<String> = ["lean/Apc/Solution.lean".to_string()].into();
                self.enforce(round, &allowed)?;
                Ok(false)
            }
            "plan" => team::plan(self, round).map(|_| false),
            "orchestrate" => team::orchestrate(self, round).map(|_| false),
            "leads" => team::leads(self, round).map(|_| false),
            "verify" => team::verify(self, round).map(|_| false),
            "integrate" => team::integrate(self, round).map(|_| false),
            "amend" => team::amend(self, round).map(|_| false),
            other => Err(VdashError::new(5, format!("unknown stage {other}"))),
        }
    }

    /// Degenerate campaign (M2-12): one worker on Solution.lean for every target.
    fn single_work(&mut self, round: u32) -> Result<()> {
        let st = self.state()?;
        let open: Vec<String> = st
            .targets
            .iter()
            .filter(|(_, t)| !t.accepted)
            .map(|(n, t)| format!("- {n}: last gate verdict: {}", if t.reasons.is_empty() { "not yet checked".into() } else { t.reasons.join(", ") }))
            .collect();
        if open.is_empty() {
            return Ok(());
        }
        let challenge = std::fs::read_to_string(self.ctx.campaign.heldout_tree().join("Apc/Challenge.lean")).unwrap_or_default();
        let sol_path = self.ctx.campaign.solution_file();
        self.snap = Some(policy::snapshot(&self.ctx));
        let task = "Prove every unaccepted target below in lean/Apc/Solution.lean. Solution.lean must restate each target theorem with exactly the statement in Challenge.lean (same name, same namespace) and prove it. It must not import Apc.Challenge. Use goal_id \"all\".";
        let open_s = open.join("\n");
        let build = |diag: Option<&str>| {
            let t = match diag {
                None => task.to_string(),
                Some(_) => format!("{task} The engine checked Solution.lean after your last reply and it does not compile cleanly yet: fix the errors below."),
            };
            crate::agents::Request::new(Role::Worker, crate::schemas::Reply::Worker, t)
                .section("goal", format!("goal_id: all\nfile: lean/Apc/Solution.lean\nunaccepted targets:\n{open_s}"))
                .section("challenge", challenge.clone())
                .section("file", std::fs::read_to_string(&sol_path).unwrap_or_default())
                .section("diagnostics", diag.unwrap_or("").to_string())
                .section("bulletin", bulletin_tail(&self.ctx))
                .label("all")
        };
        let (_, out) = team::run_worker_checked(self, round, "lean/Apc/Solution.lean", true, build)?;
        if let crate::agents::Outcome::Ok(v) = out {
            let notes = v["notes"].as_str().unwrap_or("");
            if !notes.is_empty() {
                self.bulletin(round, "worker", notes);
            }
        }
        Ok(())
    }
}

pub fn bulletin_tail(ctx: &Ctx) -> String {
    let s = std::fs::read_to_string(ctx.campaign.bulletin_file()).unwrap_or_default();
    util::clip(&s, 2000, true)
}

/// Install SIGINT/SIGTERM handling: the first signal requests a graceful stop
/// (running agents finish, no new ones start); a second exits at once.
pub fn install_signal_handlers(stop: Arc<AtomicBool>) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    for sig in [SIGINT, SIGTERM] {
        // Order matters: the conditional shutdown sees the flag before it is set.
        let _ = signal_hook::flag::register_conditional_shutdown(sig, 130, Arc::clone(&stop));
        let _ = signal_hook::flag::register(sig, Arc::clone(&stop));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::paths;
    use tempfile::TempDir;

    #[test]
    fn git_enforce_keeps_engine_written_files_after_a_restart() {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
        let camp = tmp.path().join("camp");
        paths::init_campaign(&camp, &host, None, &[BackendKind::Claude]).unwrap();
        let ctx = Ctx::for_test(&camp, tmp.path().join("state")).unwrap();
        std::fs::write(camp.join("plan.json"), "{\"revision\": 5}\n").unwrap();
        util::commit_all(&ctx.campaign, "baseline").unwrap();
        let toml = std::fs::read_to_string(camp.join("campaign.toml")).unwrap();

        // The engine writes a new plan and config mid-round, then is killed.
        std::fs::write(camp.join("plan.json"), "{\"revision\": 6}\n").unwrap();
        record_engine_file(&ctx, "plan.json");
        std::fs::write(camp.join("campaign.toml"), format!("{toml}\n# by the engine\n")).unwrap();
        record_engine_file(&ctx, "campaign.toml");
        // An agent edits a trusted file the engine never wrote.
        std::fs::write(camp.join("STRATEGY.md"), "agent edit\n").unwrap();

        let vs = enforce_against_git(&ctx, &BTreeSet::new()).unwrap();
        let paths: Vec<&str> = vs.iter().map(|v| v.path.as_str()).collect();
        assert_eq!(paths, vec!["STRATEGY.md"]);
        assert_eq!(std::fs::read_to_string(camp.join("plan.json")).unwrap(), "{\"revision\": 6}\n");
        assert!(std::fs::read_to_string(camp.join("campaign.toml")).unwrap().contains("# by the engine"));

        // An edit after the engine's write no longer matches the recorded hash.
        std::fs::write(camp.join("plan.json"), "{\"revision\": 99}\n").unwrap();
        let vs = enforce_against_git(&ctx, &BTreeSet::new()).unwrap();
        assert_eq!(vs.iter().map(|v| v.path.as_str()).collect::<Vec<_>>(), vec!["plan.json"]);
        assert_eq!(std::fs::read_to_string(camp.join("plan.json")).unwrap(), "{\"revision\": 5}\n");
    }
}
