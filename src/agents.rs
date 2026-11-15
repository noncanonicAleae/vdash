//! One agent call end to end (ROADMAP M2-09): skill + prompt → pool → validated
//! reply, with one repair attempt, a transcript file and usage accounting.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use crate::backend::pool::Pool;
use crate::backend::{AgentCall, Profile};
use crate::config::Role;
use crate::error::Result;
use crate::events;
use crate::jsonio;
use crate::paths::{BackendUsage, Ctx};
use crate::prompts;
use crate::schemas::{self, Reply};
use crate::skills;
use crate::util;

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok(Value),
    Failed(String),
}

impl Outcome {
    pub fn ok(&self) -> Option<&Value> {
        match self {
            Outcome::Ok(v) => Some(v),
            Outcome::Failed(_) => None,
        }
    }
}

/// Everything about one agent request.
pub struct Request<'a> {
    pub role: Role,
    pub reply: Reply,
    pub task: String,
    pub sections: Vec<(&'a str, String)>,
    /// Shown in events, e.g. a goal id.
    pub label: String,
    /// Override the role's default profile.
    pub profile: Option<Profile>,
}

impl<'a> Request<'a> {
    pub fn new(role: Role, reply: Reply, task: impl Into<String>) -> Self {
        Request { role, reply, task: task.into(), sections: Vec::new(), label: String::new(), profile: None }
    }

    pub fn section(mut self, name: &'a str, body: impl Into<String>) -> Self {
        self.sections.push((name, body.into()));
        self
    }

    pub fn label(mut self, l: impl Into<String>) -> Self {
        self.label = l.into();
        self
    }

    pub fn profile(mut self, p: Profile) -> Self {
        self.profile = Some(p);
        self
    }
}

static SEQ: AtomicU32 = AtomicU32::new(0);

/// A campaign-wide agent id such as `worker-17`. The counter lives in
/// `logs/agent_seq` under an exclusive lock, so the engine, `vdash btw` and the spec
/// commands never reuse an id (and never overwrite each other's transcripts).
pub fn next_agent_id(ctx: &Ctx, role: Role) -> String {
    use std::io::{Read, Seek, SeekFrom, Write};
    let path = ctx.campaign.logs_dir().join("agent_seq");
    let n = (|| -> Option<u64> {
        std::fs::create_dir_all(ctx.campaign.logs_dir()).ok()?;
        let mut f = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path).ok()?;
        rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive).ok()?;
        let mut s = String::new();
        f.read_to_string(&mut s).ok()?;
        let n = s.trim().parse::<u64>().unwrap_or(0) + 1;
        f.seek(SeekFrom::Start(0)).ok()?;
        f.set_len(0).ok()?;
        write!(f, "{n}").ok()?;
        let _ = rustix::fs::flock(&f, rustix::fs::FlockOperation::Unlock);
        Some(n)
    })()
    .unwrap_or_else(|| 1_000_000 + u64::from(SEQ.fetch_add(1, Ordering::Relaxed)));
    format!("{role}-{n}")
}

fn record_usage(ctx: &Ctx, role: Role, backend: &str, cost: f64, seconds: f64) {
    let _ = util::update_state(&ctx.campaign, |s| {
        let u = s
            .usage
            .entry(role.to_string())
            .or_default()
            .entry(backend.to_string())
            .or_insert(BackendUsage { calls: 0, cost_usd: 0.0, seconds: 0.0 });
        u.calls += 1;
        u.cost_usd += cost;
        u.seconds += seconds;
    });
}

/// Run one agent. Never returns a backend failure as `Err`: failures become
/// `Outcome::Failed` so callers can carry on. `Err` is reserved for code 4 (every
/// backend unusable) and local I/O errors.
pub fn run(ctx: &Ctx, pool: &Pool, round: u32, req: Request<'_>) -> Result<(String, Outcome)> {
    let id = next_agent_id(ctx, req.role);
    let skill = skills::load(&ctx.campaign, req.role)?;
    let prompt = prompts::build(&req.task, &req.sections);
    let schema = req.reply.schema().clone();
    let call = AgentCall {
        role: req.role,
        prompt: prompt.clone(),
        skill,
        schema: schema.clone(),
        cwd: ctx.campaign.root.clone(),
        profile: req.profile.unwrap_or_else(|| Profile::for_role(req.role)),
        model: None,
        timeout: Duration::from_secs(ctx.cfg.budget.agent_timeout_s),
    };
    let started = util::now_iso();
    events::emit(ctx, round, "agent_started", json!({"agent": id, "role": req.role.to_string(), "goal": req.label}));

    let mut attempts: Vec<Value> = Vec::new();
    let announce = |e: &crate::config::RoleEntry| {
        events::emit(ctx, round, "agent_backend", json!({"agent": id, "backend": e.backend.to_string(), "model": e.model}));
    };
    let served = pool.run_cb(ctx, round, req.role, &call, &announce)?;
    let mut result = served.result;
    let backend = served.entry.backend.to_string();
    let model = served.entry.model.clone();
    let mut cost = result.cost_usd.unwrap_or(0.0);
    let mut secs = result.seconds;

    let validate = |r: &crate::backend::AgentResult| -> Vec<String> {
        match &r.structured {
            None => vec!["no structured reply".to_string()],
            Some(v) => schemas::validate(v, &schema),
        }
    };

    let denied_turn = matches!(result.failure, Some(crate::backend::Failure::AgentError(_))) && !result.denials.is_empty();
    let mut outcome = if let (Some(f), false) = (&result.failure, denied_turn) {
        Outcome::Failed(f.label())
    } else {
        let mut errs = validate(&result);
        if denied_turn {
            errs.insert(
                0,
                format!(
                    "your previous turn was cut short because a tool call was denied ({}). Change files only with your edit and write tools, never through the shell (no `cat > f`, heredocs, `>`, `sed -i`, `cp`, `python3`). Use only the commands your role lists (`vdash check`, `vdash axioms`, `vdash search`, and `cat`, `head`, `tail`, `ls`, `wc`, `grep` for reading); if one is refused again, stop using the shell: the engine checks your file after you reply",
                    result.denials.join(", ")
                ),
            );
        }
        if errs.is_empty() {
            Outcome::Ok(result.structured.clone().unwrap())
        } else {
            attempts.push(json!({"argv": result.argv, "raw": result.raw, "errors": errs}));
            // One repair attempt on the same role.
            let repair_prompt = format!(
                "{prompt}\n<previous_reply>\n{}\n</previous_reply>\n<validation_errors>\n{}\n</validation_errors>\nYour previous reply did not match the required JSON shape. Reply again with one JSON object that fixes every error above.\n",
                util::clip(
                    &result.structured.as_ref().map(|v| v.to_string()).unwrap_or_else(|| result.text.clone()),
                    8000,
                    false
                ),
                errs.join("\n")
            );
            let mut c2 = call.clone();
            c2.prompt = repair_prompt;
            let s2 = pool.run(ctx, round, req.role, &c2)?;
            result = s2.result;
            cost += result.cost_usd.unwrap_or(0.0);
            secs += result.seconds;
            match &result.failure {
                Some(f) => Outcome::Failed(f.label()),
                None => {
                    let errs2 = validate(&result);
                    if errs2.is_empty() {
                        Outcome::Ok(result.structured.clone().unwrap())
                    } else {
                        Outcome::Failed(format!("bad_reply: {}", errs2.join("; ")))
                    }
                }
            }
        }
    };
    if !result.denials.is_empty() {
        events::emit(ctx, round, "note", json!({"text": format!("{id}: denied tool calls: {}", result.denials.join(" | "))}));
    }
    // A worker that claims success while its CLI reports denials is still judged by
    // the engine's own checks; nothing to do here.
    if let Outcome::Failed(ref f) = outcome {
        if f.is_empty() {
            outcome = Outcome::Failed("unknown".into());
        }
    }

    let transcript = json!({
        "agent": id,
        "role": req.role.to_string(),
        "reply_kind": req.reply.name(),
        "label": req.label,
        "backend": backend,
        "model": model,
        "started": started,
        "finished": util::now_iso(),
        "argv": result.argv,
        "prompt": prompt,
        "raw": result.raw,
        "earlier_attempts": attempts,
        "reply": outcome.ok(),
        "failure": match &outcome { Outcome::Failed(f) => Some(f.clone()), _ => None },
        "denials": result.denials,
        "cost_usd": cost,
        "tokens": result.tokens,
        "seconds": secs,
    });
    let path = ctx.campaign.round_logs_dir(round).join(format!("{id}.json"));
    jsonio::write_atomic(&path, &transcript)?;
    record_usage(ctx, req.role, &backend, cost, secs);
    let failure = match &outcome {
        Outcome::Failed(f) => Some(f.clone()),
        _ => None,
    };
    events::emit(
        ctx,
        round,
        "agent_finished",
        json!({"agent": id, "role": req.role.to_string(), "backend": backend, "goal": req.label, "failure": failure, "cost_usd": cost, "seconds": secs}),
    );
    Ok((id, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;
    use crate::backend::Failure;
    use crate::config::BackendKind;
    use crate::paths;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx(tmp: &TempDir) -> Ctx {
        let host = tmp.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
        let camp = tmp.path().join("c");
        paths::init_campaign(&camp, &host, None, &[BackendKind::Claude]).unwrap();
        Ctx::for_test(&camp, tmp.path().join("state")).unwrap()
    }

    fn good() -> Value {
        json!({"answer": "all fine", "skill_suggestions": []})
    }

    #[test]
    fn happy_path_writes_transcript_events_and_usage() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(&tmp);
        let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
        fb.reply(Role::Monitor, good());
        let pool = Pool::new(&c, vec![fb.clone()]);
        let (id, out) = run(&c, &pool, 2, Request::new(Role::Monitor, Reply::Monitor, "why?").section("status", "x")).unwrap();
        assert_eq!(out, Outcome::Ok(good()));
        let t: Value = jsonio::read_json(c.campaign.round_logs_dir(2).join(format!("{id}.json"))).unwrap();
        assert_eq!(t["role"], "monitor");
        assert!(t["prompt"].as_str().unwrap().contains("<status>"));
        let evs = events::tail(&c.campaign.events_file(), 10).0;
        assert!(evs.iter().any(|e| e.kind == "agent_finished" && e.str("agent") == id));
        let s = util::load_state(&c.campaign).unwrap();
        assert_eq!(s.usage["monitor"]["claude"].calls, 1);
    }

    #[test]
    fn repair_then_success_and_double_failure() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(&tmp);
        let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
        fb.reply(Role::Monitor, json!({"answer": 3})).reply(Role::Monitor, good());
        let pool = Pool::new(&c, vec![fb.clone()]);
        let (_, out) = run(&c, &pool, 1, Request::new(Role::Monitor, Reply::Monitor, "q")).unwrap();
        assert_eq!(out, Outcome::Ok(good()));
        let calls = fb.calls.lock().unwrap();
        assert!(calls[1].prompt.contains("<validation_errors>"));
        drop(calls);
        fb.reply(Role::Monitor, json!({})).reply(Role::Monitor, json!({}));
        let (_, out) = run(&c, &pool, 1, Request::new(Role::Monitor, Reply::Monitor, "q")).unwrap();
        assert!(matches!(out, Outcome::Failed(f) if f.starts_with("bad_reply")));
    }

    #[test]
    fn agent_ids_are_unique_across_threads_and_counter_persists() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(&tmp);
        let ids: Vec<String> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..16).map(|_| s.spawn(|| next_agent_id(&c, Role::Worker))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let set: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(set.len(), 16);
        assert_eq!(next_agent_id(&c, Role::Critic), "critic-17");
    }

    #[test]
    fn backend_failure_becomes_failed_outcome() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(&tmp);
        let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
        fb.fail(Role::Monitor, Failure::AgentError("boom".into()));
        let pool = Pool::new(&c, vec![fb]);
        let (_, out) = run(&c, &pool, 1, Request::new(Role::Monitor, Reply::Monitor, "q")).unwrap();
        assert_eq!(out, Outcome::Failed("agent_error: boom".into()));
    }
}
