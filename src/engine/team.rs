//! The team round (ROADMAP M3-02..M3-06): plan, orchestrate, leads, verify,
//! integrate, amend. Agents never talk to each other; every hand-off is a
//! validated reply that the engine carries, and every claim is re-checked here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Mutex;

use serde_json::{json, Value};

use super::plan::{self, Goal, Plan};
use super::{bulletin_tail, Engine};
use crate::agents::{self, Outcome, Request};
use crate::config::Role;
use crate::error::{VdashError, Result};
use crate::events;
use crate::fingerprint;
use crate::jsonio;
use crate::lean;
use crate::paths::Ctx;
use crate::policy::{self, Scope};
use crate::schemas::Reply;
use crate::util;

// ------------------------------------------------------------------ helpers

fn challenge_text(ctx: &Ctx) -> String {
    fs::read_to_string(ctx.campaign.heldout_tree().join("Apc/Challenge.lean"))
        .or_else(|_| fs::read_to_string(ctx.campaign.challenge_file()))
        .unwrap_or_default()
}

fn load_plan(ctx: &Ctx) -> Option<Plan> {
    jsonio::read_json(ctx.campaign.plan_file()).ok()
}

fn rd(eng: &Engine, path: &[&str]) -> Value {
    let st = match eng.state() {
        Ok(s) => s,
        Err(_) => return Value::Null,
    };
    let mut v = st.round_data;
    for p in path {
        v = v.get(*p).cloned().unwrap_or(Value::Null);
    }
    v
}

fn set_rd(eng: &Engine, path: &[&str], value: Value) -> Result<()> {
    eng.update(|s| {
        if !s.round_data.is_object() {
            s.round_data = json!({});
        }
        let mut cur = &mut s.round_data;
        for (i, p) in path.iter().enumerate() {
            if i + 1 == path.len() {
                cur[*p] = value.clone();
            } else {
                if !cur.get(*p).is_some_and(Value::is_object) {
                    cur[*p] = json!({});
                }
                cur = cur.get_mut(*p).unwrap();
            }
        }
    })?;
    Ok(())
}

fn push_rd(eng: &Engine, key: &str, items: Vec<Value>) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    eng.update(|s| {
        if !s.round_data.is_object() {
            s.round_data = json!({});
        }
        let arr = s.round_data[key].as_array().cloned().unwrap_or_default();
        let mut arr = arr;
        arr.extend(items.iter().cloned());
        s.round_data[key] = Value::Array(arr);
    })?;
    Ok(())
}

fn goals_listing(eng: &Engine, plan: &Plan, only: Option<&BTreeSet<String>>) -> String {
    let st = eng.state().ok();
    let mut out = String::new();
    for g in &plan.goals {
        if let Some(o) = only {
            if !o.contains(&g.id) {
                continue;
            }
        }
        let status = st.as_ref().and_then(|s| s.goals.get(&g.id)).map(|x| x.status.clone()).unwrap_or_else(|| "open".into());
        let deps = if g.depends_on.is_empty() { String::new() } else { format!(" after {}", g.depends_on.join(",")) };
        out.push_str(&format!("{} [{status}] {} {} in {}{deps}\n    {}\n", g.id, g.kind, g.lean_name, g.file, g.statement.replace('\n', "\n    ")));
    }
    out
}

fn verified_lemmas(eng: &Engine, plan: &Plan) -> String {
    let st = match eng.state() {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    plan.goals
        .iter()
        .filter(|g| matches!(g.kind.as_str(), "lemma" | "witness") && st.goals.get(&g.id).is_some_and(|x| x.status == "proved"))
        .map(|g| format!("{} ({}): {}", g.lean_name, g.file, g.statement))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `Apc.*` import graph for agent prompts, tagged with the goals that own each
/// module, whether `Apc.Solution` reaches it, and orphans (Proof files no goal owns).
fn import_graph_text(eng: &Engine, plan: &Plan) -> String {
    let tree = eng.ctx.campaign.lean_dir();
    let Ok(scan) = lean::imports::scan_imports(&tree) else { return String::new() };
    let st = eng.state().ok();
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for g in &plan.goals {
        if let Ok(m) = eng.ctx.campaign.module_name(&g.file) {
            let status = st.as_ref().and_then(|s| s.goals.get(&g.id)).map(|x| x.status.clone()).unwrap_or_else(|| "open".into());
            owners.entry(m).or_default().push(format!("{}:{status}", g.id));
        }
    }
    let reach = lean::imports::closure_of(&scan.graph, &["Apc.Solution".to_string()]);
    let mut tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for m in scan.graph.keys() {
        let t = tags.entry(m.clone()).or_default();
        if let Some(o) = owners.get(m) {
            t.push(format!("goal {}", o.join(" ")));
        } else if m.starts_with("Apc.Proof.") {
            t.push("ORPHAN: no goal owns this file".into());
        }
        if reach.contains(m) {
            t.push("in Solution's imports".into());
        }
    }
    lean::imports::format_import_graph(&scan, &tags)
}

/// Goals the solution still needs: every goal whose file `Apc.Solution` imports,
/// every unfinished goal whose `parent_target` is unaccepted, and every goal those
/// and the unaccepted targets reach through `depends_on`. The walk stops at
/// done goals (proved and still building, or accepted targets): what they once
/// depended on is no longer needed. `broken` goals count as not done.
fn needed_goals(eng: &Engine, plan: &Plan, st: &crate::paths::State, broken: &BTreeSet<String>) -> BTreeSet<String> {
    let reach = lean::imports::scan_imports(&eng.ctx.campaign.lean_dir())
        .map(|s| lean::imports::closure_of(&s.graph, &["Apc.Solution".to_string()]))
        .unwrap_or_default();
    let done = |g: &Goal| {
        if broken.contains(&g.id) {
            return false;
        }
        if g.kind == "target" {
            st.targets.get(&g.lean_name).is_some_and(|t| t.accepted)
        } else {
            st.goals.get(&g.id).is_some_and(|x| x.status == "proved")
        }
    };
    let mut needed: BTreeSet<String> = plan
        .goals
        .iter()
        .filter(|g| g.kind == "target" || eng.ctx.campaign.module_name(&g.file).is_ok_and(|m| reach.contains(&m)))
        .map(|g| g.id.clone())
        .collect();
    // A goal serving an unaccepted target (its `parent_target`) is needed even before
    // any `depends_on` edge or import reaches it: it may be a fresh lemma.
    let unaccepted_target = |pt: &str| {
        plan.goals.iter().filter(|t| t.kind == "target").any(|t| {
            (t.id == pt || t.lean_name == pt || t.lean_name.strip_prefix("Apc.") == Some(pt)) && !done(t)
        })
    };
    let mut stack: Vec<&Goal> = plan
        .goals
        .iter()
        .filter(|g| if g.kind == "target" { !done(g) } else { !done(g) && unaccepted_target(&g.parent_target) })
        .collect();
    let mut seen = BTreeSet::new();
    while let Some(g) = stack.pop() {
        if !seen.insert(g.id.clone()) {
            continue;
        }
        needed.insert(g.id.clone());
        if done(g) && g.kind != "target" {
            continue;
        }
        for d in &g.depends_on {
            if let Some(dg) = plan.goal(d) {
                if dg.kind == "target" && done(dg) {
                    needed.insert(dg.id.clone());
                } else {
                    stack.push(dg);
                }
            }
        }
    }
    needed
}

/// Proved non-target goals whose file no longer checks (the spec or a file it
/// depends on may have changed since the goal was accepted).
fn broken_proved(eng: &Engine, plan: &Plan) -> Result<BTreeSet<String>> {
    let st = eng.state()?;
    let mut by_file: BTreeMap<String, Vec<&Goal>> = BTreeMap::new();
    for g in &plan.goals {
        if g.kind != "target" && st.goals.get(&g.id).is_some_and(|x| x.status == "proved") {
            by_file.entry(g.file.clone()).or_default().push(g);
        }
    }
    let mut broken = BTreeSet::new();
    for (file, gs) in by_file {
        let names: Vec<String> = gs.iter().filter(|g| g.kind != "repair").map(|g| g.lean_name.clone()).collect();
        if check_decls(&eng.ctx, &file, &names).is_err() {
            broken.extend(gs.iter().map(|g| g.id.clone()));
        }
    }
    Ok(broken)
}

/// Reopen broken proved goals the solution still needs; retire unneeded open or
/// broken goals as `stale` (and revive stale goals that are needed again). A newly
/// stale goal asks the architect for a new plan.
fn retire_unneeded(eng: &Engine, round: u32, plan: &Plan, broken: &BTreeSet<String>) -> Result<()> {
    let st = eng.state()?;
    let needed = needed_goals(eng, plan, &st, broken);
    let (mut reopened, mut retired, mut revived) = (Vec::new(), Vec::new(), Vec::new());
    for g in plan.goals.iter().filter(|g| g.kind != "target" && g.kind != "repair") {
        let status = st.goals.get(&g.id).map(|x| x.status.as_str()).unwrap_or("open");
        let need = needed.contains(&g.id);
        match status {
            "proved" if broken.contains(&g.id) => {
                if need { reopened.push(g) } else { retired.push(g) }
            }
            "open" | "claimed" | "blocked" if !need => retired.push(g),
            "stale" if need => revived.push(g),
            _ => {}
        }
    }
    if reopened.is_empty() && retired.is_empty() && revived.is_empty() {
        return Ok(());
    }
    let ids = |v: &[&Goal]| v.iter().map(|g| g.id.as_str()).collect::<Vec<_>>().join(", ");
    if !reopened.is_empty() {
        let files: BTreeSet<&str> = reopened.iter().map(|g| g.file.as_str()).collect();
        eng.bulletin(round, "engine", &format!("proved goals no longer check in {}; reopened: {}", files.into_iter().collect::<Vec<_>>().join(", "), ids(&reopened)));
    }
    if !retired.is_empty() {
        eng.bulletin(round, "engine", &format!("goals retired as stale (nothing Solution imports or any unaccepted target needs them): {}", ids(&retired)));
    }
    if !revived.is_empty() {
        eng.bulletin(round, "engine", &format!("stale goals needed again; reopened: {}", ids(&revived)));
    }
    eng.update(|s| {
        for (gs, status) in [(&reopened, "open"), (&retired, "stale"), (&revived, "open")] {
            for g in gs.iter() {
                if let Some(x) = s.goals.get_mut(&g.id) {
                    x.status = status.into();
                    x.verified_round = None;
                }
            }
        }
    })?;
    if !retired.is_empty() {
        set_rd(eng, &["next_revise_plan"], json!(true))?;
    }
    Ok(())
}

/// Find orphaned files that break the solution and give each a `repair` goal.
/// An orphan is a `Apc.Proof.*` module reachable from `Apc.Solution` or from a goal's
/// file that no goal owns and that fails to build or contains sorry, while every
/// module it imports is fine (so the repair addresses a root cause).
fn adopt_orphans(eng: &Engine, round: u32, plan: Plan) -> Result<Plan> {
    let tree = eng.ctx.campaign.lean_dir();
    let scan = lean::imports::scan_imports(&tree)?;
    let owned: BTreeSet<String> = plan.goals.iter().filter(|g| g.kind != "repair").filter_map(|g| eng.ctx.campaign.module_name(&g.file).ok()).collect();
    let mut roots: Vec<String> = owned.iter().filter(|m| scan.graph.contains_key(*m)).cloned().collect();
    roots.push("Apc.Solution".into());
    roots.retain(|m| scan.graph.contains_key(m));
    let reach = lean::imports::closure_of(&scan.graph, &roots);
    let candidates: Vec<String> = reach.iter().filter(|m| m.starts_with("Apc.Proof.") && !owned.contains(*m)).cloned().collect();
    let mut repairs = Vec::new();
    if !candidates.is_empty() {
        // Build each root separately: one broken root must not hide the others' results.
        let mut results: BTreeMap<String, lean::build::ModuleResult> = BTreeMap::new();
        for r in &roots {
            if let Ok(rep) = lean::build::build(&eng.ctx, &tree, &eng.ctx.campaign.build_work_dir(), Some(std::slice::from_ref(r))) {
                record_module_times(&eng.ctx, &rep);
                results.extend(rep.results);
            }
        }
        let bad = |m: &str| results.get(m).is_some_and(|r| !r.ok || r.diagnostics.iter().any(|d| d.kind == "hasSorry"));
        let parent = plan.goals.iter().find(|g| g.kind == "target").map(|g| g.parent_target.clone()).unwrap_or_default();
        for m in candidates {
            let deps_fine = scan.graph[&m].iter().all(|d| !bad(d) && scan.graph.contains_key(d));
            if bad(&m) && deps_fine {
                if let Some(rel) = scan.rel_paths.get(&m) {
                    repairs.push(plan::repair_goal(&m, &format!("lean/{}", rel.display()), &parent));
                }
            }
        }
    }
    let new_plan = plan::with_repairs(&plan, repairs);
    if new_plan != plan {
        jsonio::write_atomic(eng.ctx.campaign.plan_file(), &new_plan)?;
        super::record_engine_file(&eng.ctx, "plan.json");
        let adopted: Vec<String> = new_plan.goals.iter().filter(|g| g.kind == "repair").map(|g| g.file.clone()).collect();
        if !adopted.is_empty() {
            eng.bulletin(round, "engine", &format!("orphaned files adopted as repair goals: {}", adopted.join(", ")));
        }
    }
    // Every listed repair goal is broken right now, so none of them is proved.
    eng.update(|s| {
        s.goals.retain(|id, _| !id.starts_with(plan::REPAIR_PREFIX) || new_plan.goal(id).is_some());
        for g in new_plan.goals.iter().filter(|g| g.kind == "repair") {
            let gs = s.goals.entry(g.id.clone()).or_insert(crate::paths::GoalState {
                status: "open".into(),
                lean_name: g.lean_name.clone(),
                file: g.file.clone(),
                lead: None,
                verified_round: None,
            });
            if gs.status == "proved" {
                gs.status = "open".into();
                gs.verified_round = None;
            }
        }
    })?;
    Ok(new_plan)
}

fn slug(s: &str) -> String {
    let mut out: String = s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    out.truncate(40);
    if out.is_empty() {
        "x".into()
    } else {
        out
    }
}

fn read_file(ctx: &Ctx, rel: &str) -> String {
    fs::read_to_string(ctx.campaign.root.join(rel)).unwrap_or_default()
}

/// Persist fresh (non-cached) module elaboration times into `state.json` and prune
/// entries whose `.lean` source file was removed (so `slow_modules` stays current).
fn record_module_times(ctx: &Ctx, rep: &lean::build::BuildReport) {
    if !ctx.campaign.state_file().is_file() {
        return;
    }
    let updates: Vec<(String, f64)> = rep
        .results
        .iter()
        .filter(|(_, mr)| mr.ok && !mr.cached && mr.seconds > 0.0)
        .map(|(m, mr)| (m.clone(), mr.seconds))
        .collect();
    let has_stale = util::load_state(&ctx.campaign)
        .map(|st| {
            st.module_times
                .keys()
                .any(|m| !ctx.campaign.root.join(ctx.campaign.module_path(m)).is_file())
        })
        .unwrap_or(false);
    if updates.is_empty() && !has_stale {
        return;
    }
    let _ = util::update_state(&ctx.campaign, |st| {
        st.module_times
            .retain(|m, _| ctx.campaign.root.join(ctx.campaign.module_path(m)).is_file());
        for (m, secs) in &updates {
            st.module_times.insert(m.clone(), *secs);
        }
    });
}

/// Build one module of the work tree and check its declarations (M3-05).
pub fn check_decls(ctx: &Ctx, file: &str, names: &[String]) -> std::result::Result<BTreeMap<String, fingerprint::DeclInfo>, String> {
    let module = ctx.campaign.module_name(file).map_err(|e| e.message)?;
    let tree = ctx.campaign.lean_dir();
    let build_dir = ctx.campaign.build_work_dir();
    let rep = lean::build::build(ctx, &tree, &build_dir, Some(std::slice::from_ref(&module))).map_err(|e| e.message)?;
    record_module_times(ctx, &rep);
    if !rep.ok {
        return Err(format!("{file} does not build"));
    }
    if (file != "lean/Apc/Solution.lean" || names.is_empty())
        && rep.results.get(&module).is_some_and(|r| r.diagnostics.iter().any(|d| d.kind == "hasSorry"))
    {
        return Err(format!("{file} still contains sorry"));
    }
    if names.is_empty() {
        return Ok(BTreeMap::new());
    }
    let info = fingerprint::axioms(ctx, &tree, &build_dir, &module, names).map_err(|e| e.message)?;
    for n in names {
        match info.get(n) {
            Some(d) if d.exists && d.kind == "theorem" => {
                if let Some(bad) = d.axioms.iter().find(|a| !fingerprint::is_standard_axiom(a)) {
                    return Err(format!("{n} uses the axiom {bad}"));
                }
            }
            Some(d) if d.exists => return Err(format!("{n} is a {}, not a theorem", d.kind)),
            _ => return Err(format!("{n} does not exist in {file}")),
        }
    }
    Ok(info)
}

/// Check a worker's `proved` claim: the file builds without sorry, every claimed
/// name is a theorem with standard axioms, and every witness exists and mentions
/// the definition it witnesses.
pub fn verify_claim(ctx: &Ctx, file: &str, names: &[String], defs: &[(String, String)]) -> std::result::Result<(), String> {
    let mut all: Vec<String> = names.to_vec();
    all.extend(defs.iter().map(|(_, w)| w.clone()));
    all.sort();
    all.dedup();
    if names.is_empty() {
        return Err("no lemma names were given".into());
    }
    check_decls(ctx, file, &all)?;
    if !defs.is_empty() {
        let module = ctx.campaign.module_name(file).map_err(|e| e.message)?;
        let ws: Vec<String> = defs.iter().map(|(_, w)| w.clone()).collect();
        let fps = fingerprint::closure(ctx, &ctx.campaign.lean_dir(), &ctx.campaign.build_work_dir(), &module, &ws)
            .map_err(|e| e.message)?;
        for (d, w) in defs {
            let mentions = fps.get(w).is_some_and(|f| f.closure.contains_key(d));
            if !mentions {
                return Err(format!("witness {w} does not mention {d}"));
            }
        }
    }
    Ok(())
}

/// `Ok` when `file` builds with no sorry; otherwise the `vdash check` report.
pub fn file_status(ctx: &Ctx, file: &str) -> std::result::Result<(), String> {
    match lean::build::check_file_report(ctx, std::path::Path::new(file), &ctx.campaign.root) {
        Ok((report, ok, rep)) => {
            record_module_times(ctx, &rep);
            let last = report.trim_end().lines().last().unwrap_or("");
            if ok && last == "OK" {
                Ok(())
            } else {
                Err(report)
            }
        }
        Err(e) => Err(e.message),
    }
}

/// Extra worker attempts when the engine's own check of the file fails. Some
/// backends cannot run `vdash check` themselves (fact A14), so the engine closes the
/// loop: it checks the file after each reply and sends the errors back.
pub const WORKER_RETRIES: usize = 2;

/// Run a worker, then check its file; on errors, call it again with the
/// diagnostics, up to `WORKER_RETRIES` times. When `require_no_sorry` is `false`
/// (team mode on `Solution.lean`), `OK (N sorry)` from other unproved targets is
/// accepted as long as the claimed theorem(s) pass `check_decls` with no `sorryAx`.
/// Returns the last agent id and reply.
pub fn run_worker_checked<'a>(
    eng: &Engine,
    round: u32,
    file: &str,
    require_no_sorry: bool,
    build: impl Fn(Option<&str>) -> Request<'a>,
) -> Result<(String, Outcome)> {
    let mut diag: Option<String> = None;
    let mut last = (String::new(), Outcome::Failed("not run".into()));
    for attempt in 0..=WORKER_RETRIES {
        if eng.stopping() {
            break;
        }
        let (id, out) = agents::run(&eng.ctx, eng.pool, round, build(diag.as_deref()))?;
        let sandbox_blocked = out.ok().is_some_and(|v| {
            let notes = v["notes"].as_str().unwrap_or("").to_lowercase();
            notes.contains("vdash")
                && (notes.contains("not found")
                    || notes.contains("introuvable")
                    || notes.contains("127")
                    || notes.contains("unavailable")
                    || notes.contains("sandbox"))
        });
        let status_str = out.ok().and_then(|v| v["status"].as_str()).unwrap_or("");
        let stuck = status_str == "stuck" && !sandbox_blocked;
        let is_proved = status_str == "proved";
        let claimed_names: Vec<String> = out
            .ok()
            .and_then(|v| v["lean_names"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        last = (id, out);
        if stuck {
            break;
        }
        if let Some(snap) = &eng.snap {
            let vs = policy::restore_trusted(&eng.ctx, snap)?;
            policy::report(&eng.ctx, round, &vs);
        }
        let check_res = match file_status(&eng.ctx, file) {
            Ok(()) => Ok(()),
            Err(report) => {
                let last_line = report.trim_end().lines().last().unwrap_or("");
                if !require_no_sorry
                    && last_line.starts_with("OK (")
                    && !claimed_names.is_empty()
                    && check_decls(&eng.ctx, file, &claimed_names).is_ok()
                {
                    Ok(())
                } else {
                    Err(report)
                }
            }
        };
        match check_res {
            Ok(()) => {
                if is_proved || attempt >= WORKER_RETRIES {
                    break;
                }
                // File compiled cleanly, but the worker reported stuck/partial (e.g. missing `vdash` in sandbox).
                events::emit(&eng.ctx, round, "note", json!({"text": format!("{file} compiled cleanly; asking worker to confirm proved status")}));
                diag = Some("The engine checked your file and it compiled cleanly (OK). Do not mark stuck/partial due to missing vdash in the sandbox: if your proof is complete and matches the goal statement, reply with status \"proved\" and the theorem name(s) in lean_names.".into());
            }
            Err(report) => {
                if attempt < WORKER_RETRIES {
                    events::emit(&eng.ctx, round, "note", json!({"text": format!("{file} does not check yet; sending the errors back to the worker")}));
                }
                diag = Some(report);
            }
        }
    }
    Ok(last)
}

fn trusted_unchanged(eng: &Engine) -> bool {
    let Some(snap) = &eng.snap else { return false };
    eng.ctx.campaign.trusted_files().iter().all(|rel| {
        let key = rel.to_string_lossy().replace('\\', "/");
        let now = fs::read(eng.ctx.campaign.root.join(rel)).ok();
        snap.bytes(&key).map(|b| b.to_vec()) == now
    })
}

pub fn write_escalation(ctx: &Ctx, round: u32, who: &str, text: &str) -> Result<()> {
    let dir = ctx.campaign.escalations_dir();
    fs::create_dir_all(&dir).map_err(|e| VdashError::io_path(&dir, e))?;
    let n = fs::read_dir(&dir).map(|r| r.count()).unwrap_or(0) as u32 + 1;
    let first: String = text.lines().next().unwrap_or("issue").chars().take(40).collect();
    let p = dir.join(format!("{n:03}-{}.md", slug(&first).to_lowercase()));
    let body = format!("# Escalation {n}\n\n- round: {round}\n- from: {who}\n- at: {}\n\n{}\n", util::now_iso(), text.trim());
    fs::write(&p, body).map_err(|e| VdashError::io_path(&p, e))?;
    events::emit(ctx, round, "escalation", json!({"n": n, "text": first}));
    Ok(())
}

// ------------------------------------------------------------------ plan

fn fallback_plan(eng: &Engine, revision: u32) -> Plan {
    let st = eng.state().ok();
    let goals = st
        .map(|s| s.targets.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, t)| Goal {
            id: format!("t{}", i + 1),
            kind: "target".into(),
            lean_name: t.clone(),
            statement: format!("(the statement of {t} in Challenge.lean)"),
            file: "lean/Apc/Solution.lean".into(),
            depends_on: vec![],
            parent_target: t,
        })
        .collect();
    Plan { version: 1, revision, goals }
}

fn parse_goals(v: &Value) -> Option<Vec<Goal>> {
    serde_json::from_value(v.get("goals")?.clone()).ok()
}

pub fn plan(eng: &mut Engine, round: u32) -> Result<()> {
    let old = load_plan(&eng.ctx);
    let revise = rd(eng, &["revise_plan"]).as_bool().unwrap_or(false);
    if old.is_some() && !revise {
        return Ok(());
    }
    let st = eng.state()?;
    let rev = old.as_ref().map(|p| p.revision + 1).unwrap_or(1);
    let strategy = fs::read_to_string(eng.ctx.campaign.strategy_file()).unwrap_or_default();
    let base = Request::new(
        Role::Architect,
        Reply::Architect,
        format!(
            "Design the goal decomposition for this campaign (plan revision {rev}). Every target theorem in Challenge.lean needs exactly one goal of kind target in lean/Apc/Solution.lean. Keep proved goals unchanged."
        ),
    )
    .section("challenge", challenge_text(&eng.ctx))
    .section("import_graph", old.as_ref().map(|p| import_graph_text(eng, p)).unwrap_or_default())
    .section("current_plan", old.as_ref().map(|p| serde_json::to_string_pretty(&plan::with_repairs(p, vec![])).unwrap_or_default()).unwrap_or_default())
    .section("goal_status", st.goals.iter().map(|(k, g)| format!("{k}: {}", g.status)).collect::<Vec<_>>().join("\n"))
    .section("strategy", strategy)
    .section("bulletin", bulletin_tail(&eng.ctx))
    .label("plan");
    let (_, out) = agents::run(&eng.ctx, eng.pool, round, base)?;
    let mut goals = out.ok().and_then(parse_goals);
    if let Some(g) = &goals {
        let errs = plan::validate(g, &st);
        if !errs.is_empty() {
            // One repair call with the validation errors.
            let fix = Request::new(Role::Architect, Reply::Architect, "Your plan was rejected. Fix every error listed and reply with the complete corrected plan.")
                .section("challenge", challenge_text(&eng.ctx))
                .section("rejected_plan", json!({"goals": g}).to_string())
                .section("errors", errs.join("\n"))
                .label("plan");
            let (_, out2) = agents::run(&eng.ctx, eng.pool, round, fix)?;
            goals = out2.ok().and_then(parse_goals).filter(|g2| plan::validate(g2, &st).is_empty());
        }
    }
    let new_plan = match goals {
        Some(g) => Plan { version: 1, revision: rev, goals: g },
        None => {
            eng.bulletin(round, "engine", "the architect's plan was unusable; keeping the previous plan");
            match &old {
                Some(p) => p.clone(),
                None => fallback_plan(eng, rev),
            }
        }
    };
    jsonio::write_atomic(eng.ctx.campaign.plan_file(), &new_plan)?;
    eng.own("plan.json");
    eng.update(|s| plan::merge_into_state(old.as_ref(), &new_plan, s))?;
    events::emit(&eng.ctx, round, "plan", json!({"revision": new_plan.revision, "goals": new_plan.goals.len()}));
    Ok(())
}

// ------------------------------------------------------------------ orchestrate

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeadAssign {
    pub lead_id: String,
    pub goal_ids: Vec<String>,
    pub brief: String,
}

pub fn orchestrate(eng: &mut Engine, round: u32) -> Result<()> {
    let mut plan = load_plan(&eng.ctx).unwrap_or_else(|| fallback_plan(eng, 1));
    let broken = if eng.rechecked { BTreeSet::new() } else { broken_proved(eng, &plan)? };
    eng.rechecked = true;
    retire_unneeded(eng, round, &plan, &broken)?;
    plan = adopt_orphans(eng, round, plan)?;
    let st = eng.state()?;
    let open: BTreeSet<String> = plan
        .goals
        .iter()
        .filter(|g| !matches!(st.goals.get(&g.id).map(|x| x.status.as_str()), Some("proved") | Some("refuted") | Some("stale")))
        .map(|g| g.id.clone())
        .collect();
    if open.is_empty() {
        set_rd(eng, &["orchestrator"], json!({"leads": [], "integrate": false}))?;
        return Ok(());
    }
    let summaries: String = st.leads.iter().map(|(k, l)| format!("{k} (round {}): {}", l.last_round, l.summary)).collect::<Vec<_>>().join("\n");
    let verdicts: String = st.targets.iter().map(|(k, t)| format!("{k}: {}", if t.accepted { "ACCEPTED".into() } else { t.reasons.join(", ") })).collect::<Vec<_>>().join("\n");
    let mut slow: Vec<(&String, &f64)> = st.module_times.iter().collect();
    slow.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
    let slow: String = slow.iter().take(5).map(|(m, t)| format!("{m}: {t:.1}s")).collect::<Vec<_>>().join("\n");
    let escalations: String = fs::read_dir(eng.ctx.campaign.escalations_dir())
        .map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    let max_leads = eng.ctx.cfg.budget.max_leads;
    let req = Request::new(
        Role::Orchestrator,
        Reply::Orchestrator,
        format!("Round {round}. Group the open goals into at most {max_leads} leads. Follow the user's guidance if any."),
    )
    .section("goals", goals_listing(eng, &plan, None))
    .section("import_graph", import_graph_text(eng, &plan))
    .section("lead_summaries", summaries)
    .section("gate_verdicts", verdicts)
    .section("guidance", st.pending_guidance.join("\n\n"))
    .section("escalations", escalations)
    .section("slow_modules", slow)
    .section("bulletin", bulletin_tail(&eng.ctx))
    .label("orchestrate");
    let (_, out) = agents::run(&eng.ctx, eng.pool, round, req)?;
    eng.update(|s| s.pending_guidance.clear())?;
    let (mut leads, integrate, revise) = match out.ok() {
        Some(v) => (
            serde_json::from_value::<Vec<LeadAssign>>(v["leads"].clone()).unwrap_or_default(),
            v["integrate"].as_bool().unwrap_or(false),
            v["revise_plan"].as_bool().unwrap_or(false),
        ),
        None => (Vec::new(), false, false),
    };
    // Post-conditions.
    let mut taken = BTreeSet::new();
    for l in leads.iter_mut() {
        l.goal_ids.retain(|g| open.contains(g) && taken.insert(g.clone()));
    }
    leads.retain(|l| !l.goal_ids.is_empty());
    let mut seen_ids = BTreeSet::new();
    leads.retain(|l| seen_ids.insert(l.lead_id.clone()));
    leads.truncate(max_leads);
    if leads.is_empty() {
        leads.push(LeadAssign { lead_id: "L1".into(), goal_ids: open.iter().cloned().collect(), brief: "Work on all open goals.".into() });
        eng.bulletin(round, "engine", "the orchestrator's grouping was unusable; one lead takes every open goal");
    }
    set_rd(eng, &["orchestrator"], json!({"leads": leads, "integrate": integrate}))?;
    // The engine may already have asked for a new plan this round (stale goals).
    let already = rd(eng, &["next_revise_plan"]).as_bool().unwrap_or(false);
    set_rd(eng, &["next_revise_plan"], json!(revise || already))?;
    eng.update(|s| {
        for l in &leads {
            let e = s.leads.entry(l.lead_id.clone()).or_insert(crate::paths::LeadState { goal_ids: vec![], summary: String::new(), last_round: round });
            e.goal_ids = l.goal_ids.clone();
            for g in &l.goal_ids {
                if let Some(gs) = s.goals.get_mut(g) {
                    gs.lead = Some(l.lead_id.clone());
                }
            }
        }
    })?;
    events::emit(&eng.ctx, round, "leads", json!({"leads": leads}));
    Ok(())
}

// ------------------------------------------------------------------ leads

struct Shared {
    /// File -> the lead that claimed it this round.
    claimed: Mutex<BTreeMap<String, String>>,
    allowed: Mutex<BTreeSet<String>>,
}

/// Claim `file` for `lead` this round: one assignment per file, first come first
/// served (also within one lead, so two workers never share a file).
fn claim(sh: &Shared, lead: &str, file: &str) -> bool {
    let mut c = sh.claimed.lock().unwrap();
    if c.contains_key(file) {
        return false;
    }
    c.insert(file.to_string(), lead.to_string());
    sh.allowed.lock().unwrap().insert(file.to_string());
    true
}

/// Claims carried into a resumed leads stage: only those of leads that already
/// finished this round (have a summary). Leads that are re-run start over, so the
/// files they held before the restart are free again. Older state has no `claims`
/// map; then nothing is carried (the `allowed` set is kept either way).
fn resumed_claims(prev: &Value, done: &Value) -> BTreeMap<String, String> {
    prev.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(f, l)| l.as_str().map(|l| (f.clone(), l.to_string())))
        .filter(|(_, l)| done.get(l).and_then(|d| d.get("summary")).is_some())
        .collect()
}

pub fn leads(eng: &mut Engine, round: u32) -> Result<()> {
    let leads: Vec<LeadAssign> = serde_json::from_value(rd(eng, &["orchestrator", "leads"])).unwrap_or_default();
    if leads.is_empty() {
        return Ok(());
    }
    let done = rd(eng, &["leads"]);
    let fresh = !done.is_object() || done.as_object().is_some_and(|m| m.is_empty());
    if fresh {
        eng.snap = Some(policy::snapshot(&eng.ctx));
    }
    // Warm the build cache for the spec so workers' checks start fast.
    let _ = lean::build::build(&eng.ctx, &eng.ctx.campaign.lean_dir(), &eng.ctx.campaign.build_work_dir(), Some(&["Apc.Challenge".to_string()]));
    let prev_allowed: BTreeSet<String> = serde_json::from_value(rd(eng, &["allowed"])).unwrap_or_default();
    let sh = Shared { claimed: Mutex::new(resumed_claims(&rd(eng, &["claims"]), &done)), allowed: Mutex::new(prev_allowed) };
    let plan = load_plan(&eng.ctx).unwrap_or_else(|| fallback_plan(eng, 1));
    let eng_ref: &Engine = eng;
    let results: Vec<Result<()>> = std::thread::scope(|s| {
        let hs: Vec<_> = leads
            .iter()
            .filter(|l| done.get(&l.lead_id).and_then(|d| d.get("summary")).is_none())
            .map(|l| {
                let sh = &sh;
                let plan = &plan;
                s.spawn(move || run_lead(eng_ref, round, l, plan, sh))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(VdashError::new(5, "a lead thread panicked")))).collect()
    });
    let allowed: Vec<String> = sh.allowed.lock().unwrap().iter().cloned().collect();
    set_rd(eng, &["allowed"], json!(allowed))?;
    set_rd(eng, &["claims"], json!(*sh.claimed.lock().unwrap()))?;
    for r in results {
        r?;
    }
    Ok(())
}

fn run_lead(eng: &Engine, round: u32, lead: &LeadAssign, plan: &Plan, sh: &Shared) -> Result<()> {
    let ctx = &eng.ctx;
    let id = lead.lead_id.as_str();
    let mine: BTreeSet<String> = lead.goal_ids.iter().cloned().collect();
    let notes = fs::read_to_string(ctx.campaign.lead_notes_file(id)).unwrap_or_default();
    let verified = verified_lemmas(eng, plan);
    let has_target = plan.goals.iter().any(|g| mine.contains(&g.id) && g.kind == "target");
    // (a) plan
    let req = Request::new(
        Role::TeamLead,
        Reply::LeadPlan,
        format!("You are lead {id}, round {round}. Reply with a PLAN for this round. Each assignment gets its own file."),
    )
    .section("brief", lead.brief.clone())
    .section("goals", goals_listing(eng, plan, Some(&mine)))
    .section("lead_notes", notes)
    .section("verified_lemmas", verified.clone())
    .section("import_graph", import_graph_text(eng, plan))
    .section("challenge", if has_target { challenge_text(ctx) } else { String::new() })
    .section("bulletin", bulletin_tail(ctx))
    .label(id);
    let (_, out) = agents::run(ctx, eng.pool, round, req)?;
    let lp = out.ok().cloned().unwrap_or_else(|| {
        json!({"refute": [], "shared": {"definitions": [], "lemma_statements": [], "conventions": ""}, "assignments":
            lead.goal_ids.iter().filter_map(|g| plan.goal(g)).map(|g| json!({"goal_id": g.id, "file": g.file, "instructions": "", "existing_lemmas": []})).collect::<Vec<_>>()})
    });
    set_rd(eng, &["leads", id, "plan"], lp.clone())?;
    if eng.stopping() {
        return Ok(());
    }
    // (b) falsify first
    let refute: Vec<String> = lp["refute"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).filter(|g| mine.contains(*g)).map(String::from).collect();
    let mut refuted: BTreeSet<String> = BTreeSet::new();
    let fals: Vec<Value> = std::thread::scope(|s| {
        let hs: Vec<_> = refute
            .iter()
            .filter_map(|gid| plan.goal(gid).map(|g| (gid, g)))
            .filter_map(|(gid, g)| {
                let file = format!("lean/Apc/Proof/Work/Refute_{}.lean", slug(gid));
                if !claim(sh, id, &file) {
                    return None;
                }
                Some(s.spawn(move || -> Value {
                    let req = Request::new(Role::Falsifier, Reply::Falsifier, format!("Try to disprove goal {gid}. Work only in {file}."))
                        .section("goal", format!("goal_id: {gid}\nlean_name: {}\nstatement:\n{}", g.lean_name, g.statement))
                        .section("file", read_file(&eng.ctx, &file))
                        .section("scratch_file", file.clone())
                        .label(gid.clone());
                    match agents::run(&eng.ctx, eng.pool, round, req) {
                        Ok((agent, Outcome::Ok(v))) => json!({"agent": agent, "file": file, "reply": v}),
                        Ok((agent, Outcome::Failed(f))) => json!({"agent": agent, "file": file, "failed": f}),
                        Err(e) => json!({"file": file, "failed": e.message}),
                    }
                }))
            })
            .collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    for f in &fals {
        let r = &f["reply"];
        if r["verdict"] == "false" {
            let gid = r["goal_id"].as_str().unwrap_or("");
            let name = r["lean_name"].as_str().unwrap_or("").to_string();
            let file = f["file"].as_str().unwrap_or("");
            // Provisional: only if the trusted files are untouched right now.
            if mine.contains(gid) && trusted_unchanged(eng) && !name.is_empty() && check_decls(ctx, file, &[name]).is_ok() {
                refuted.insert(gid.to_string());
                eng.bulletin(round, id, &format!("goal {gid} looks false (provisional): {}", r["evidence"].as_str().unwrap_or("")));
            }
        }
    }
    set_rd(eng, &["leads", id, "falsifiers"], json!(fals))?;
    set_rd(eng, &["leads", id, "refuted"], json!(refuted))?;
    if eng.stopping() {
        return Ok(());
    }
    // (c) workers
    #[derive(Clone)]
    struct Asg {
        goal: String,
        file: String,
        instructions: String,
        existing: Vec<String>,
    }
    let mut asgs: Vec<Asg> = Vec::new();
    for a in lp["assignments"].as_array().into_iter().flatten() {
        let goal = a["goal_id"].as_str().unwrap_or("").to_string();
        let mut file = a["file"].as_str().unwrap_or("").to_string();
        // A repair goal is about one file; it is always worked on in place.
        if let Some(g) = plan.goal(&goal).filter(|g| g.kind == "repair") {
            file = g.file.clone();
        }
        if !mine.contains(&goal) || refuted.contains(&goal) || !plan::valid_file(&file) {
            continue;
        }
        asgs.push(Asg {
            goal,
            file,
            instructions: a["instructions"].as_str().unwrap_or("").to_string(),
            existing: a["existing_lemmas"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect(),
        });
    }
    let mut kept = Vec::new();
    let mut deferred = Vec::new();
    for a in asgs {
        if claim(sh, id, &a.file) {
            kept.push(a);
        } else {
            eng.bulletin(round, id, &format!("assignment of {} deferred: {} is already taken this round", a.goal, a.file));
            deferred.push((a.goal, a.file));
        }
    }
    eng.update(|s| {
        for a in &kept {
            if let Some(g) = s.goals.get_mut(&a.goal) {
                if g.status == "open" || g.status == "blocked" {
                    g.status = "claimed".into();
                }
            }
        }
    })?;
    let shared = serde_json::to_string_pretty(&lp["shared"]).unwrap_or_default();
    let workers: Vec<Value> = std::thread::scope(|s| {
        let hs: Vec<_> = kept
            .iter()
            .filter_map(|a| plan.goal(&a.goal).map(|g| (a, g)))
            .map(|(a, g)| {
                let shared = &shared;
                s.spawn(move || -> Value {
                    if eng.stopping() {
                        return json!({"goal_id": a.goal, "file": a.file, "failed": "stopped"});
                    }
                    let build = |diag: Option<&str>| {
                        let task = match diag {
                            None if g.kind == "repair" => format!("Repair {} (goal {}): make it build with no errors and no sorry. Edit only that file.", a.file, a.goal),
                            None => format!("Prove goal {} in {}. Edit only that file.", a.goal, a.file),
                            Some(_) => format!("Prove goal {} in {}. Edit only that file. The engine checked the file after your last reply and it does not compile cleanly yet: fix the errors below.", a.goal, a.file),
                        };
                        Request::new(Role::Worker, Reply::Worker, task)
                            .section("goal", format!("goal_id: {}\nkind: {}\nlean_name: {}\nfile: {}\nstatement:\n{}", g.id, g.kind, g.lean_name, a.file, g.statement))
                            .section("instructions", a.instructions.clone())
                            .section("shared", shared.clone())
                            .section("existing_lemmas", a.existing.join("\n"))
                            .section("import_graph", import_graph_text(eng, plan))
                            .section("challenge", if g.kind == "target" { challenge_text(&eng.ctx) } else { String::new() })
                            .section("file", read_file(&eng.ctx, &a.file))
                            .section("diagnostics", diag.unwrap_or("").to_string())
                            .section("bulletin", bulletin_tail(&eng.ctx))
                            .label(a.goal.clone())
                    };
                    match run_worker_checked(eng, round, &a.file, false, build) {
                        Ok((agent, Outcome::Ok(v))) => json!({"agent": agent, "goal_id": a.goal, "file": a.file, "reply": v}),
                        Ok((agent, Outcome::Failed(f))) => json!({"agent": agent, "goal_id": a.goal, "file": a.file, "failed": f}),
                        Err(e) => json!({"goal_id": a.goal, "file": a.file, "failed": e.message}),
                    }
                })
            })
            .collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    set_rd(eng, &["leads", id, "workers"], json!(workers))?;
    // (d) summary
    let mut compact: Vec<Value> = workers
        .iter()
        .map(|w| json!({"goal_id": w["goal_id"], "file": w["file"], "status": w["reply"]["status"], "lean_names": w["reply"]["lean_names"], "notes": w["reply"]["notes"], "failed": w["failed"], "amendment": w["reply"]["amendment"]}))
        .collect();
    for (gid, file) in &deferred {
        compact.push(json!({"goal_id": gid, "file": file, "failed": "deferred: file already claimed by another lead this round"}));
    }
    let req = Request::new(
        Role::TeamLead,
        Reply::LeadSummary,
        format!("You are lead {id}, round {round}. Reply with a SUMMARY of this round. Note that the engine re-checks every claim; report what workers say, flagged as claims."),
    )
    .section("goals", goals_listing(eng, plan, Some(&mine)))
    .section("plan", serde_json::to_string_pretty(&lp).unwrap_or_default())
    .section("falsifiers", serde_json::to_string_pretty(&fals).unwrap_or_default())
    .section("workers", serde_json::to_string_pretty(&compact).unwrap_or_default())
    .label(id);
    let (_, out) = agents::run(ctx, eng.pool, round, req)?;
    let summ = out.ok().cloned().unwrap_or_else(|| {
        json!({"summary": format!("(no summary: the lead's reply failed) {} workers ran", workers.len()), "goal_status": {}, "notes": "", "bulletin": [], "amendments": [], "escalations": []})
    });
    let summary_text = summ["summary"].as_str().unwrap_or("").to_string();
    // Lead notes and escalations are files; they are written in the verify stage,
    // after the revert step, so the snapshot never sees them as agent edits.
    set_rd(eng, &["leads", id, "notes"], summ["notes"].clone())?;
    set_rd(eng, &["leads", id, "escalations"], summ["escalations"].clone())?;
    for b in summ["bulletin"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
        eng.bulletin(round, id, b);
    }
    let mut proposals: Vec<Value> = summ["amendments"].as_array().cloned().unwrap_or_default();
    for w in &workers {
        let a = &w["reply"]["amendment"];
        if a.is_object() && !proposals.contains(a) {
            proposals.push(a.clone());
        }
    }
    push_rd(eng, "proposals", proposals.into_iter().map(|p| json!({"from": id, "proposal": p})).collect())?;
    eng.update(|s| {
        let l = s.leads.entry(id.to_string()).or_insert(crate::paths::LeadState { goal_ids: lead.goal_ids.clone(), summary: String::new(), last_round: round });
        l.summary = summary_text.clone();
        l.last_round = round;
    })?;
    set_rd(eng, &["leads", id, "summary"], json!(summary_text))?;
    events::emit(ctx, round, "lead_summary", json!({"lead": id, "summary": summary_text}));
    Ok(())
}

// ------------------------------------------------------------------ verify

pub fn verify(eng: &mut Engine, round: u32) -> Result<()> {
    let allowed: BTreeSet<String> = serde_json::from_value(rd(eng, &["allowed"])).unwrap_or_default();
    eng.enforce(round, &allowed)?;
    let leads = rd(eng, &["leads"]);
    let plan = load_plan(&eng.ctx);
    // Engine-owned files from the leads stage (see run_lead).
    for (lid, l) in leads.as_object().into_iter().flatten() {
        if let Some(notes) = l["notes"].as_str().filter(|n| !n.is_empty()) {
            let p = eng.ctx.campaign.lead_notes_file(lid);
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            fs::write(&p, notes).map_err(|e| VdashError::io_path(&p, e))?;
        }
        if !l["escalations_written"].as_bool().unwrap_or(false) {
            for e in l["escalations"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
                write_escalation(&eng.ctx, round, lid, e)?;
            }
            set_rd(eng, &["leads", lid, "escalations_written"], json!(true))?;
        }
    }
    let mut results: Vec<(String, String, std::result::Result<(), String>, u32)> = Vec::new();
    for (_lid, l) in leads.as_object().into_iter().flatten() {
        for w in l["workers"].as_array().into_iter().flatten() {
            let gid = w["goal_id"].as_str().unwrap_or("").to_string();
            let r = &w["reply"];
            if r["status"] != "proved" {
                continue;
            }
            let file = w["file"].as_str().unwrap_or("").to_string();
            let names: Vec<String> = r["lean_names"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
            let defs: Vec<(String, String)> = r["new_definitions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| Some((d["name"].as_str()?.to_string(), d["witness"].as_str()?.to_string())))
                .collect();
            // The goal's own name must be among the proved names.
            let goal_name = plan.as_ref().and_then(|p| p.goal(&gid)).map(|g| g.lean_name.clone());
            let is_repair = plan.as_ref().and_then(|p| p.goal(&gid)).is_some_and(|g| g.kind == "repair");
            let res = match &goal_name {
                _ if is_repair => check_decls(&eng.ctx, &file, &[]).map(|_| ()),
                Some(n) if !names.contains(n) => Err(format!("the reply does not list the goal's theorem {n}")),
                _ => verify_claim(&eng.ctx, &file, &names, &defs),
            };
            results.push((gid, file, res, round));
        }
        // Provisional refutations, now that out-of-scope edits are reverted.
        for f in l["falsifiers"].as_array().into_iter().flatten() {
            let r = &f["reply"];
            let gid = r["goal_id"].as_str().unwrap_or("").to_string();
            let provisional = l["refuted"].as_array().is_some_and(|a| a.iter().any(|x| x == gid.as_str()));
            if !provisional {
                continue;
            }
            let name = r["lean_name"].as_str().unwrap_or("").to_string();
            let ok = check_decls(&eng.ctx, f["file"].as_str().unwrap_or(""), &[name]).is_ok();
            eng.update(|s| {
                if let Some(g) = s.goals.get_mut(&gid) {
                    g.status = if ok { "refuted".into() } else { "open".into() };
                }
            })?;
            if ok {
                set_rd(eng, &["next_revise_plan"], json!(true))?;
                events::emit(&eng.ctx, round, "goal", json!({"id": gid, "status": "refuted"}));
            } else {
                eng.bulletin(round, "engine", &format!("the refutation of {gid} did not survive re-checking; the goal is open again"));
            }
        }
    }
    let mut moved_files: BTreeMap<String, String> = BTreeMap::new();
    for (gid, file, res, r) in results {
        let is_target = plan.as_ref().and_then(|p| p.goal(&gid)).is_some_and(|g| g.kind == "target");
        match res {
            Ok(()) => {
                eng.update(|s| {
                    if let Some(g) = s.goals.get_mut(&gid) {
                        if !is_target {
                            g.status = "proved".into();
                            g.verified_round = Some(r);
                            if !file.is_empty() {
                                g.file = file.clone();
                            }
                        }
                    }
                })?;
                if !is_target {
                    if !file.is_empty() {
                        moved_files.insert(gid.clone(), file);
                    }
                    events::emit(&eng.ctx, round, "goal", json!({"id": gid, "status": "proved"}));
                }
            }
            Err(why) => {
                eng.update(|s| {
                    if let Some(g) = s.goals.get_mut(&gid) {
                        g.status = "open".into();
                    }
                })?;
                eng.bulletin(round, "engine", &format!("claim for {gid} rejected: {why}"));
                events::emit(&eng.ctx, round, "goal", json!({"id": gid, "status": "open", "reason": why}));
            }
        }
    }
    if !moved_files.is_empty() {
        if let Some(mut p) = plan {
            let mut changed = false;
            for g in &mut p.goals {
                if let Some(f) = moved_files.get(&g.id) {
                    if &g.file != f {
                        g.file = f.clone();
                        changed = true;
                    }
                }
            }
            if changed {
                jsonio::write_atomic(eng.ctx.campaign.plan_file(), &p)?;
            }
        }
    }
    // Anything still marked claimed did not finish.
    eng.update(|s| {
        for g in s.goals.values_mut() {
            if g.status == "claimed" {
                g.status = "open".into();
            }
        }
    })?;
    Ok(())
}

// ------------------------------------------------------------------ integrate

pub fn integrate(eng: &mut Engine, round: u32) -> Result<()> {
    let every = eng.ctx.cfg.budget.integrate_every.max(1);
    let asked = rd(eng, &["orchestrator", "integrate"]).as_bool().unwrap_or(false);
    if !round.is_multiple_of(every) && !asked {
        return Ok(());
    }
    let Some(plan) = load_plan(&eng.ctx) else { return Ok(()) };
    let st = eng.state()?;
    let work: Vec<&Goal> = plan
        .goals
        .iter()
        .filter(|g| matches!(g.kind.as_str(), "lemma" | "witness") && g.file.starts_with("lean/Apc/Proof/Work/") && st.goals.get(&g.id).is_some_and(|x| x.status == "proved"))
        .collect();
    if work.is_empty() {
        return Ok(());
    }
    let before_ok = lean::build::build(&eng.ctx, &eng.ctx.campaign.lean_dir(), &eng.ctx.campaign.build_work_dir(), Some(&["Apc.Solution".to_string()]))
        .map(|r| {
            record_module_times(&eng.ctx, &r);
            r.ok
        })
        .unwrap_or(false);
    let st = eng.state()?;
    let snap = policy::snapshot(&eng.ctx);
    let mut slow: Vec<(&String, &f64)> = st.module_times.iter().filter(|(m, _)| m.starts_with("Apc.Proof")).collect();
    slow.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
    let req = Request::new(Role::Integrator, Reply::Integrator, "Move the verified lemmas below into lean/Apc/Proof/Lib/, remove duplicates, and keep every file building.")
        .section("verified_lemmas", work.iter().map(|g| format!("{} in {}: {}", g.lean_name, g.file, g.statement)).collect::<Vec<_>>().join("\n"))
        .section("slow_modules", slow.iter().take(5).map(|(m, t)| format!("{m}: {t:.1}s")).collect::<Vec<_>>().join("\n"))
        .label("integrate");
    let (_, out) = agents::run(&eng.ctx, eng.pool, round, req)?;
    let allowed: BTreeSet<String> =
        ["lean/Apc/Proof/Lib/", "lean/Apc/Proof/Work/", "lean/Apc/Solution.lean"].iter().map(|s| s.to_string()).collect();
    let vs = policy::enforce(&eng.ctx, &snap, Scope::Proof, &allowed)?;
    policy::report(&eng.ctx, round, &vs);
    let after_ok = lean::build::build(&eng.ctx, &eng.ctx.campaign.lean_dir(), &eng.ctx.campaign.build_work_dir(), Some(&["Apc.Solution".to_string()]))
        .map(|r| {
            record_module_times(&eng.ctx, &r);
            r.ok
        })
        .unwrap_or(false);
    // Every proved lemma must still exist somewhere it can be checked.
    let mut lost = Vec::new();
    if let Some(v) = out.ok() {
        let moved: BTreeMap<String, String> = v["moved"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| Some((m["lean_name"].as_str()?.to_string(), m["to"].as_str()?.to_string())))
            .collect();
        for g in &work {
            let file = moved.get(&g.lean_name).cloned().unwrap_or_else(|| g.file.clone());
            if check_decls(&eng.ctx, &file, std::slice::from_ref(&g.lean_name)).is_err() {
                lost.push(g.lean_name.clone());
            }
        }
        if (after_ok || !before_ok) && lost.is_empty() {
            let new_plan = Plan {
                goals: plan
                    .goals
                    .iter()
                    .map(|g| {
                        let mut g = g.clone();
                        if let Some(f) = moved.get(&g.lean_name) {
                            if plan::valid_file(f) {
                                g.file = f.clone();
                            }
                        }
                        g
                    })
                    .collect(),
                ..plan.clone()
            };
            jsonio::write_atomic(eng.ctx.campaign.plan_file(), &new_plan)?;
            eng.update(|s| {
                for g in &new_plan.goals {
                    if let Some(gs) = s.goals.get_mut(&g.id) {
                        gs.file = g.file.clone();
                    }
                }
            })?;
            util::commit_all(&eng.ctx.campaign, &format!("vdash: integrate round {round}"))?;
            eng.bulletin(round, "integrator", v["notes"].as_str().unwrap_or("integrated verified lemmas"));
            return Ok(());
        }
    }
    // Roll back everything the integrator changed.
    let _ = policy::enforce(&eng.ctx, &snap, Scope::Proof, &BTreeSet::new())?;
    eng.bulletin(
        round,
        "engine",
        &format!(
            "integration rolled back ({})",
            if !lost.is_empty() { format!("lemmas no longer check: {}", lost.join(", ")) } else if !after_ok { "Solution.lean stopped building".into() } else { "the integrator's reply failed".into() }
        ),
    );
    Ok(())
}

// ------------------------------------------------------------------ amend

pub fn amend(eng: &mut Engine, round: u32) -> Result<()> {
    let proposals: Vec<Value> = rd(eng, &["proposals"]).as_array().cloned().unwrap_or_default();
    let decisions = std::mem::take(&mut eng.decisions);
    if proposals.is_empty() && decisions.is_empty() {
        return Ok(());
    }
    // Queued items are `{"from": lead, "proposal": {...}}`; pass the bare proposals,
    // one at a time, so one malformed proposal cannot stop the engine.
    let mut out = crate::amend::AmendOutcome::default();
    for item in proposals {
        let p = item.get("proposal").cloned().unwrap_or(item);
        match crate::amend::process(&eng.ctx, eng.pool, round, vec![p], vec![]) {
            Ok(o) => out.applied.extend(o.applied),
            Err(e) if e.code == 2 || e.code == 1 => eng.bulletin(round, "engine", &format!("an amendment proposal was rejected before review: {}", e.message)),
            Err(e) => return Err(e),
        }
    }
    if !decisions.is_empty() {
        let o = crate::amend::process(&eng.ctx, eng.pool, round, vec![], decisions)?;
        out.applied.extend(o.applied);
    }
    set_rd(eng, &["proposals"], json!([]))?;
    if !out.applied.is_empty() {
        eng.update(|s| {
            for t in s.targets.values_mut() {
                t.accepted = false;
                t.since_round = None;
            }
            for g in s.goals.values_mut() {
                if g.status == "proved" || g.status == "refuted" {
                    g.status = "open".into();
                    g.verified_round = None;
                }
            }
        })?;
        eng.snap = Some(policy::snapshot(&eng.ctx));
        set_rd(eng, &["next_revise_plan"], json!(true))?;
        eng.bulletin(
            round,
            "engine",
            &format!("amendment(s) {} applied to the spec; every target will be re-checked", out.applied.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ")),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::lean::build::{BuildReport, ModuleResult};
    use crate::paths;
    use tempfile::TempDir;

    #[test]
    fn resumed_leads_reclaim_their_own_files_but_not_finished_leads_files() {
        let prev = json!({"a.lean": "L1", "b.lean": "L2", "c.lean": "L3"});
        let done = json!({"L2": {"summary": "done"}, "L3": {"plan": {}}});
        let carried = resumed_claims(&prev, &done);
        assert_eq!(carried.into_iter().collect::<Vec<_>>(), vec![("b.lean".to_string(), "L2".to_string())]);
        assert!(resumed_claims(&Value::Null, &done).is_empty());

        let sh = Shared { claimed: Mutex::new(resumed_claims(&prev, &done)), allowed: Mutex::new(BTreeSet::new()) };
        assert!(claim(&sh, "L1", "a.lean"), "a re-run lead's old file is free again");
        assert!(!claim(&sh, "L1", "a.lean"), "one assignment per file, even within a lead");
        assert!(!claim(&sh, "L3", "a.lean"), "and not for another lead");
        assert!(!claim(&sh, "L1", "b.lean"), "a finished lead's file stays taken");
        assert_eq!(sh.allowed.lock().unwrap().len(), 1);
    }

    #[test]
    fn record_module_times_updates_uncached_preserves_cached_and_prunes_deleted() {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();

        let camp = tmp.path().join("camp");
        paths::init_campaign(&camp, &host, None, &[BackendKind::Claude]).unwrap();
        let ctx = Ctx::for_test(&camp, tmp.path().join("state")).unwrap();

        let slow_file = camp.join("lean/Apc/Proof/Lib/Slow.lean");
        let work_file = camp.join("lean/Apc/Proof/Work/Temp.lean");
        fs::create_dir_all(slow_file.parent().unwrap()).unwrap();
        fs::create_dir_all(work_file.parent().unwrap()).unwrap();
        fs::write(&slow_file, "-- slow\n").unwrap();
        fs::write(&work_file, "-- temp\n").unwrap();

        let mut results = BTreeMap::new();
        results.insert(
            "Apc.Proof.Lib.Slow".to_string(),
            ModuleResult { module: "Apc.Proof.Lib.Slow".into(), ok: true, cached: false, seconds: 12.5, diagnostics: vec![] },
        );
        results.insert(
            "Apc.Proof.Work.Temp".to_string(),
            ModuleResult { module: "Apc.Proof.Work.Temp".into(), ok: true, cached: false, seconds: 2.3, diagnostics: vec![] },
        );
        record_module_times(&ctx, &BuildReport { results, ok: true });

        let st1 = util::load_state(&ctx.campaign).unwrap();
        assert_eq!(st1.module_times.get("Apc.Proof.Lib.Slow"), Some(&12.5));
        assert_eq!(st1.module_times.get("Apc.Proof.Work.Temp"), Some(&2.3));

        // Cached rebuilds (seconds = 0.0) must not overwrite recorded times; deleted files are pruned.
        fs::remove_file(&work_file).unwrap();
        let mut cached_res = BTreeMap::new();
        cached_res.insert(
            "Apc.Proof.Lib.Slow".to_string(),
            ModuleResult { module: "Apc.Proof.Lib.Slow".into(), ok: true, cached: true, seconds: 0.0, diagnostics: vec![] },
        );
        record_module_times(&ctx, &BuildReport { results: cached_res, ok: true });

        let st2 = util::load_state(&ctx.campaign).unwrap();
        assert_eq!(st2.module_times.get("Apc.Proof.Lib.Slow"), Some(&12.5));
        assert!(!st2.module_times.contains_key("Apc.Proof.Work.Temp"));
    }
}
