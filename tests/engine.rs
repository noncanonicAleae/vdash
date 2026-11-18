//! Engine end to end on the isort example with scripted agents (M2-12, M3-07).

mod common;

use std::fs;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::json;
use tempfile::TempDir;

use vdash::backend::fake::FakeBackend;
use vdash::backend::pool::Pool;
use vdash::config::{BackendKind, Role};
use vdash::engine::{self, Opts};
use vdash::paths::{self, Ctx};

fn approved_isort(tmp: &TempDir) -> Ctx {
    let camp = tmp.path().join("camp");
    paths::init_campaign(&camp, common::real_host_project(), Some("isort"), &BackendKind::ALL).unwrap();
    let ctx = Ctx::for_test(&camp, tmp.path().join("state")).unwrap();
    vdash::approve::approve(&ctx).unwrap();
    // Reload config written by init (unchanged) and return.
    Ctx::for_test(&camp, tmp.path().join("state")).unwrap()
}

fn worker_reply(goal: &str, names: &[&str]) -> serde_json::Value {
    json!({"goal_id": goal, "status": "proved", "lean_names": names, "new_definitions": [], "notes": "done", "amendment": null})
}

const TARGETS: [&str; 4] = ["Apc.isort_correct", "Apc.isort_fixes_sorted", "Apc.sorted_nonvacuous", "Apc.isortBad_incorrect"];

fn run(ctx: &Ctx, pool: &Pool, rounds: u32, single: bool) -> u8 {
    engine::run(ctx.clone(), pool, Opts { rounds: Some(rounds), single }, Arc::new(AtomicBool::new(false))).unwrap()
}

#[test]
fn single_worker_with_reference_solution_finishes_in_one_round() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    let root = ctx.campaign.root.clone();
    fb.func(Role::Worker, move |_call| {
        fs::write(root.join("lean/Apc/Solution.lean"), paths::ISORT_REFERENCE_SOLUTION).unwrap();
        worker_reply("all", &TARGETS)
    });
    let pool = Pool::new(&ctx, vec![fb.clone()]);
    assert_eq!(run(&ctx, &pool, 3, true), 0);
    let st = vdash::util::load_state(&ctx.campaign).unwrap();
    assert_eq!(st.phase, "done");
    assert!(st.targets.values().all(|t| t.accepted), "{:?}", st.targets);
    assert_eq!(fb.count(Role::Worker), 1);
    let log = vdash::util::git(&ctx.campaign, &["log", "--oneline"]).unwrap();
    assert!(log.contains("vdash: round 1"), "{log}");
}

#[test]
fn spec_edit_by_worker_is_reverted_and_gate_decides() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    let root = ctx.campaign.root.clone();
    let defs_before = fs::read_to_string(root.join("lean/Apc/Spec/Defs.lean")).unwrap();
    let r2 = root.clone();
    fb.func(Role::Worker, move |_call| {
        fs::write(r2.join("lean/Apc/Solution.lean"), paths::ISORT_REFERENCE_SOLUTION).unwrap();
        // Weaken the spec: every list counts as sorted.
        fs::write(r2.join("lean/Apc/Spec/Defs.lean"), "import Apc.Spec.Impl\nnamespace Apc.Spec\ndef Sorted (_ : List Nat) : Prop := True\ndef Correct (f : List Nat → List Nat) : Prop := ∀ l, Sorted (f l) ∧ (f l).Perm l\nend Apc.Spec\n").unwrap();
        worker_reply("all", &TARGETS)
    });
    let pool = Pool::new(&ctx, vec![fb]);
    assert_eq!(run(&ctx, &pool, 2, true), 0);
    assert_eq!(fs::read_to_string(root.join("lean/Apc/Spec/Defs.lean")).unwrap(), defs_before);
    let bulletin = fs::read_to_string(ctx.campaign.bulletin_file()).unwrap();
    assert!(bulletin.contains("lean/Apc/Spec/Defs.lean"), "{bulletin}");
}

#[test]
fn sorry_worker_exhausts_rounds_and_second_engine_is_refused() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    fb.always(Role::Worker, worker_reply("all", &TARGETS)); // claims success, changes nothing
    let pool = Pool::new(&ctx, vec![fb.clone()]);
    let _lock = vdash::events::EngineLock::acquire(&ctx).unwrap();
    let e = engine::run(ctx.clone(), &pool, Opts { rounds: Some(1), single: true }, Arc::new(AtomicBool::new(false))).err().unwrap();
    assert_eq!(e.code, 2);
    drop(_lock);
    assert_eq!(run(&ctx, &pool, 2, true), 1);
    let st = vdash::util::load_state(&ctx.campaign).unwrap();
    assert!(st.targets.values().all(|t| !t.accepted));
    assert_eq!(fb.count(Role::Worker), 2 * (1 + vdash::engine::team::WORKER_RETRIES));
}

#[test]
fn team_round_with_both_backends_reaches_done() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    let claude = Arc::new(FakeBackend::new(BackendKind::Claude));
    let gemini = Arc::new(FakeBackend::new(BackendKind::Gemini));
    let goals: Vec<_> = TARGETS
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"id": format!("t{}", i + 1), "kind": "target", "lean_name": t, "statement": format!("see Challenge.lean: {t}"), "file": "lean/Apc/Solution.lean", "depends_on": [], "parent_target": t}))
        .collect();
    claude.reply(Role::Architect, json!({"goals": goals, "rationale": "one goal per target"}));
    claude.always(Role::Orchestrator, json!({"leads": [{"lead_id": "L1", "goal_ids": ["t1", "t2", "t3", "t4"], "brief": "all targets"}], "revise_plan": false, "integrate": false, "notes": ""}));
    // Lead: plan once (refute t4 first), then summary.
    claude.reply(Role::TeamLead, json!({"refute": ["t4"], "shared": {"definitions": [], "lemma_statements": [], "conventions": ""},
        "assignments": [{"goal_id": "t1", "file": "lean/Apc/Solution.lean", "instructions": "prove all four", "existing_lemmas": []},
                        {"goal_id": "t2", "file": "lean/Apc/Solution.lean", "instructions": "", "existing_lemmas": []}]}));
    claude.reply(Role::TeamLead, json!({"summary": "all four restated and proved in Solution.lean (claimed)", "goal_status": {"t1": "claimed"}, "notes": "round 1 done", "bulletin": ["Solution.lean holds every target"], "amendments": [], "escalations": []}));
    let root = ctx.campaign.root.clone();
    let worker = move |_call: &vdash::backend::AgentCall| {
        fs::write(root.join("lean/Apc/Solution.lean"), paths::ISORT_REFERENCE_SOLUTION).unwrap();
        worker_reply("t1", &["Apc.isort_correct"])
    };
    claude.func(Role::Worker, worker.clone());
    gemini.func(Role::Worker, worker);
    gemini.reply(Role::Falsifier, json!({"goal_id": "t4", "verdict": "plausible", "evidence": "no counterexample", "lean_name": null}));
    let pool = Pool::new(&ctx, vec![claude.clone(), gemini.clone()]);
    assert_eq!(run(&ctx, &pool, 2, false), 0);
    let st = vdash::util::load_state(&ctx.campaign).unwrap();
    assert_eq!(st.phase, "done");
    assert!(st.targets.values().all(|t| t.accepted));
    assert_eq!(st.goals["t1"].status, "proved");
    assert_eq!(gemini.count(Role::Falsifier), 1);
    // The second assignment to Solution.lean was deferred.
    let bulletin = fs::read_to_string(ctx.campaign.bulletin_file()).unwrap();
    assert!(bulletin.contains("deferred"), "{bulletin}");
    assert_eq!(fs::read_to_string(ctx.campaign.lead_notes_file("L1")).unwrap(), "round 1 done");
    assert!(fs::read_to_string(ctx.campaign.status_log_file()).unwrap().contains("4/4 targets accepted"));
}

#[test]
fn resume_after_crash_skips_finished_stages() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    // Simulate a crash after the leads stage of round 1: the worker's file is in place.
    fs::write(ctx.campaign.solution_file(), paths::ISORT_REFERENCE_SOLUTION).unwrap();
    vdash::util::update_state(&ctx.campaign, |s| {
        s.phase = "proving".into();
        s.round = 1;
        s.stage = "verify".into();
        s.round_data = json!({"allowed": ["lean/Apc/Solution.lean"], "leads": {}});
    })
    .unwrap();
    vdash::util::forget_state_cache(&ctx.campaign);
    // No agent may be called: verify, integrate, amend and gate_post need none.
    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    let pool = Pool::new(&ctx, vec![fb.clone()]);
    assert_eq!(run(&ctx, &pool, 1, false), 0);
    assert_eq!(fb.calls.lock().unwrap().len(), 0);
    let evs = vdash::events::tail(&ctx.campaign.events_file(), 50).0;
    assert!(evs.iter().any(|e| e.kind == "note" && e.str("text").contains("resuming round 1 at stage verify")));
}

#[test]
fn control_messages_stop_and_guidance() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    vdash::events::send(&ctx, &vdash::events::Control::Guidance { text: "use induction on the list".into() }).unwrap();
    vdash::events::send(&ctx, &vdash::events::Control::Stop).unwrap();
    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    let pool = Pool::new(&ctx, vec![fb.clone()]);
    assert_eq!(run(&ctx, &pool, 3, false), 130);
    assert_eq!(fb.calls.lock().unwrap().len(), 0);
    let st = vdash::util::load_state(&ctx.campaign).unwrap();
    assert_eq!(st.pending_guidance, vec!["use induction on the list".to_string()]);
    assert!(fs::read_to_string(ctx.campaign.strategy_file()).unwrap().contains("use induction"));
}

#[test]
fn amend_stage_reviews_proposals_and_survives_a_malformed_one() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    let defs = fs::read_to_string(ctx.campaign.root.join("lean/Apc/Spec/Defs.lean")).unwrap();
    assert!(defs.contains("x ≤ y"));
    let good = json!({"from": "L1", "proposal": {
        "file": "lean/Apc/Spec/Defs.lean", "old_text": "x ≤ y", "new_text": "x ≤ y",
        "justification": "no-op test proposal", "evidence_kind": "argument",
        "evidence_file": null, "evidence_lean_name": null, "alternatives_tried": "none"}});
    let bad = json!({"from": "L2", "proposal": {"file": "x"}});
    vdash::util::update_state(&ctx.campaign, |s| {
        s.round = 1;
        s.round_data = json!({"proposals": [bad, good]});
    })
    .unwrap();
    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    fb.reply(Role::Critic, json!({"verdict": "accept", "touches_trusted_assumptions": false, "closer_to_code": true, "reason": "fine"}));
    let pool = Pool::new(&ctx, vec![fb.clone()]);
    let mut eng = engine::Engine::new(ctx.clone(), &pool, Opts::default(), Arc::new(AtomicBool::new(false)));
    engine::team::amend(&mut eng, 1).unwrap();
    assert_eq!(fb.count(Role::Critic), 1);
    let list = vdash::amend::list_cmd(&ctx).unwrap();
    assert!(list.contains("001") && list.contains("pending"), "{list}");
    let bulletin = fs::read_to_string(ctx.campaign.bulletin_file()).unwrap();
    assert!(bulletin.contains("rejected before review"), "{bulletin}");
    let st = vdash::util::load_state(&ctx.campaign).unwrap();
    assert_eq!(st.round_data["proposals"], json!([]));
}

#[test]
fn solution_partial_sorry_checked_per_theorem_and_sandbox_stuck_recovered() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    let claude = Arc::new(FakeBackend::new(BackendKind::Claude));
    let root = ctx.campaign.root.clone();

    let goals: Vec<_> = TARGETS
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"id": format!("t{}", i + 1), "kind": "target", "lean_name": t, "statement": format!("see Challenge.lean: {t}"), "file": "lean/Apc/Solution.lean", "depends_on": [], "parent_target": t}))
        .collect();
    let mut all_goals = goals;
    all_goals.push(json!({"id": "l1", "kind": "lemma", "lean_name": "Apc.helper_ok", "statement": "theorem Apc.helper_ok : 1 = 1", "file": "lean/Apc/Proof/Work/Planned.lean", "depends_on": [], "parent_target": "Apc.isort_correct"}));

    claude.reply(Role::Architect, json!({"goals": all_goals, "rationale": "test"}));
    claude.always(Role::Orchestrator, json!({
        "leads": [
            {"lead_id": "L1", "goal_ids": ["t3", "l1"], "brief": "prove t3 and l1"},
            {"lead_id": "L2", "goal_ids": ["t4"], "brief": "prove t4"}
        ],
        "revise_plan": false,
        "integrate": false,
        "notes": ""
    }));
    // L1 plans t3 in Solution.lean and l1 in Work/Actual.lean (different from Planned.lean).
    let saw_deferred = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for _ in 0..4 {
        let saw_deferred = saw_deferred.clone();
        claude.func(Role::TeamLead, move |call| {
            if call.prompt.contains("Reply with a PLAN") {
                if call.prompt.contains("lead L1") {
                    json!({"refute": [], "shared": {"definitions": [], "lemma_statements": [], "conventions": ""},
                        "assignments": [
                            {"goal_id": "t3", "file": "lean/Apc/Solution.lean", "instructions": "prove sorted_nonvacuous", "existing_lemmas": []},
                            {"goal_id": "l1", "file": "lean/Apc/Proof/Work/Actual.lean", "instructions": "prove helper_ok", "existing_lemmas": []}
                        ]})
                } else {
                    json!({"refute": [], "shared": {"definitions": [], "lemma_statements": [], "conventions": ""},
                        "assignments": [{"goal_id": "t4", "file": "lean/Apc/Solution.lean", "instructions": "prove t4", "existing_lemmas": []}]})
                }
            } else {
                // Whichever lead lost the Solution.lean claim must see "deferred" in LeadSummary.
                if call.prompt.contains("deferred") {
                    saw_deferred.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                json!({"summary": "round done", "goal_status": {}, "notes": "", "bulletin": [], "amendments": [], "escalations": []})
            }
        });
    }

    for _ in 0..3 {
        let r_worker = root.clone();
        claude.func(Role::Worker, move |call| {
            if call.prompt.contains("goal_id: t3") {
                // Prove only sorted_nonvacuous in Solution.lean, leaving other targets sorry.
                let sol = r_worker.join("lean/Apc/Solution.lean");
                let s = fs::read_to_string(&sol).unwrap().replace(
                    "theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := by\n  sorry",
                    "theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := by\n  refine ⟨[1, 2], ?_, ?_⟩\n  · decide\n  · exact ⟨by decide, trivial⟩",
                );
                fs::write(&sol, s).unwrap();
                worker_reply("t3", &["Apc.sorted_nonvacuous"])
            } else if call.prompt.contains("goal_id: l1") {
                if call.prompt.contains("compiled cleanly") {
                    worker_reply("l1", &["Apc.helper_ok"])
                } else {
                    let p = r_worker.join("lean/Apc/Proof/Work/Actual.lean");
                    fs::create_dir_all(p.parent().unwrap()).unwrap();
                    fs::write(&p, "namespace Apc\ntheorem helper_ok : 1 = 1 := rfl\nend Apc\n").unwrap();
                    json!({"goal_id": "l1", "status": "stuck", "lean_names": ["Apc.helper_ok"], "new_definitions": [], "notes": "vdash: command not found (exit 127) in sandbox", "amendment": null})
                }
            } else {
                // Prove only isortBad_incorrect in Solution.lean, leaving other targets sorry.
                let sol = r_worker.join("lean/Apc/Solution.lean");
                let s = fs::read_to_string(&sol).unwrap().replace(
                    "theorem isortBad_incorrect : ¬ Spec.Correct Spec.isortBad := by\n  sorry",
                    "theorem isortBad_incorrect : ¬ Spec.Correct Spec.isortBad := by\n  intro h\n  have hperm : (Spec.isortBad [1, 1]).Perm [1, 1] := (h [1, 1]).2\n  have hlen : (Spec.isortBad [1, 1]).length = ([1, 1] : List Nat).length := List.Perm.length_eq hperm\n  revert hlen\n  decide",
                );
                fs::write(&sol, s).unwrap();
                worker_reply("t4", &["Apc.isortBad_incorrect"])
            }
        });
    }

    let pool = Pool::new(&ctx, vec![claude.clone()]);
    let _ = run(&ctx, &pool, 1, false);
    assert!(saw_deferred.load(std::sync::atomic::Ordering::Relaxed), "neither lead summary saw deferred");
    let st = vdash::util::load_state(&ctx.campaign).unwrap();
    // Whichever target won Solution.lean was accepted by the gate and NOT rejected for other targets' sorry.
    assert!(st.targets["Apc.sorted_nonvacuous"].accepted || st.targets["Apc.isortBad_incorrect"].accepted);
    let bulletin = fs::read_to_string(ctx.campaign.bulletin_file()).unwrap();
    assert!(!bulletin.contains("still contains sorry"), "{bulletin}");
    // l1 recovered from sandbox stuck and updated file in both state.json and plan.json.
    assert_eq!(st.goals["l1"].status, "proved");
    assert_eq!(st.goals["l1"].file, "lean/Apc/Proof/Work/Actual.lean");
    let plan: serde_json::Value = vdash::jsonio::read_json(ctx.campaign.plan_file()).unwrap();
    let l1_file = plan["goals"].as_array().unwrap().iter().find(|g| g["id"] == "l1").unwrap()["file"].as_str().unwrap();
    assert_eq!(l1_file, "lean/Apc/Proof/Work/Actual.lean");
    // t3 ran only once (no wasted retries on OK (N sorry)); l1 ran once too: its file
    // checks cleanly, so the engine records it proved without a confirm call.
    assert_eq!(claude.count(Role::Worker), 2);
}

/// The sequent-calculus failure modes: a goal marked proved whose file no longer
/// builds, an orphaned file that breaks Solution.lean, and an unrelated file with a
/// missing import. The engine reopens the stale goal, adopts the orphan as a repair
/// goal, shows agents the import graph, and nothing is blinded by the dangling import.
#[test]
fn stale_proved_goals_reopen_and_orphans_become_repair_goals() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    let root = ctx.campaign.root.clone();
    let work = root.join("lean/Apc/Proof/Work");
    let lib = root.join("lean/Apc/Proof/Lib");
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&lib).unwrap();
    fs::write(work.join("Broken.lean"), "namespace Apc\ntheorem broken_thing : 1 = 2 := rfl\nend Apc\n").unwrap();
    fs::write(work.join("Dangling.lean"), "import Apc.Proof.Work.Nope\n").unwrap();
    fs::write(lib.join("Stale.lean"), "namespace Apc\ntheorem stale_ok : 1 = 1 := by exact nonexistent_lemma\nend Apc\n").unwrap();
    // Leftovers from an earlier plan that nothing needs any more.
    fs::write(lib.join("Old.lean"), "namespace Apc\ntheorem old_ok : 1 = 1 := by exact nonexistent_lemma\nend Apc\n").unwrap();
    let sol = root.join("lean/Apc/Solution.lean");
    fs::write(&sol, format!("import Apc.Proof.Work.Broken\n{}", fs::read_to_string(&sol).unwrap())).unwrap();

    // A plan from before the spec changed: s1 is recorded as proved.
    let mut goals: Vec<_> = TARGETS
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"id": format!("t{}", i + 1), "kind": "target", "lean_name": t, "statement": format!("see Challenge.lean: {t}"), "file": "lean/Apc/Solution.lean", "depends_on": if i == 0 { vec!["s1"] } else { vec![] }, "parent_target": t}))
        .collect();
    goals.push(json!({"id": "o1", "kind": "lemma", "lean_name": "Apc.old_ok", "statement": "theorem Apc.old_ok : 1 = 1", "file": "lean/Apc/Proof/Lib/Old.lean", "depends_on": [], "parent_target": "Apc.gone_target"}));
    goals.push(json!({"id": "o2", "kind": "lemma", "lean_name": "Apc.old_open", "statement": "theorem Apc.old_open : 2 = 2", "file": "lean/Apc/Proof/Work/OldOpen.lean", "depends_on": [], "parent_target": "Apc.gone_target"}));
    goals.push(json!({"id": "s1", "kind": "lemma", "lean_name": "Apc.stale_ok", "statement": "theorem Apc.stale_ok : 1 = 1", "file": "lean/Apc/Proof/Lib/Stale.lean", "depends_on": [], "parent_target": "Apc.isort_correct"}));
    vdash::jsonio::write_atomic(ctx.campaign.plan_file(), &json!({"version": 1, "revision": 1, "goals": goals})).unwrap();
    vdash::util::update_state(&ctx.campaign, |s| {
        for g in &goals {
            let id = g["id"].as_str().unwrap().to_string();
            s.goals.insert(id.clone(), vdash::paths::GoalState {
                status: if id == "s1" || id == "o1" { "proved".into() } else { "open".into() },
                lean_name: g["lean_name"].as_str().unwrap().into(),
                file: g["file"].as_str().unwrap().into(),
                lead: None,
                verified_round: Some(1),
            });
        }
        s.round_data = json!({});
    })
    .unwrap();

    let claude = Arc::new(FakeBackend::new(BackendKind::Claude));
    let repair_id = "repair_Apc_Proof_Work_Broken";
    let orch_prompt = Arc::new(std::sync::Mutex::new(String::new()));
    let op = orch_prompt.clone();
    claude.func(Role::Orchestrator, move |call| {
        *op.lock().unwrap() = call.prompt.clone();
        json!({"leads": [{"lead_id": "L1", "goal_ids": [repair_id, "s1"], "brief": "repairs"}], "revise_plan": false, "integrate": false, "notes": ""})
    });
    claude.reply(Role::TeamLead, json!({"refute": [], "shared": {"definitions": [], "lemma_statements": [], "conventions": ""},
        "assignments": [
            // The wrong file: a repair goal is pinned to its own file.
            {"goal_id": repair_id, "file": "lean/Apc/Proof/Work/Elsewhere.lean", "instructions": "", "existing_lemmas": []},
            {"goal_id": "s1", "file": "lean/Apc/Proof/Lib/Stale.lean", "instructions": "", "existing_lemmas": []}]}));
    claude.reply(Role::TeamLead, json!({"summary": "done", "goal_status": {}, "notes": "", "bulletin": [], "amendments": [], "escalations": []}));
    let (w1, w2) = (work.clone(), lib.clone());
    let worker_prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let wp = worker_prompts.clone();
    let worker = move |call: &vdash::backend::AgentCall| {
        wp.lock().unwrap().push(call.prompt.clone());
        if call.prompt.contains("Repair lean/Apc/Proof/Work/Broken.lean") {
            fs::write(w1.join("Broken.lean"), "namespace Apc\ntheorem broken_thing : 2 = 2 := rfl\nend Apc\n").unwrap();
            worker_reply(repair_id, &["Apc.broken_thing"])
        } else {
            fs::write(w2.join("Stale.lean"), "namespace Apc\ntheorem stale_ok : 1 = 1 := rfl\nend Apc\n").unwrap();
            worker_reply("s1", &["Apc.stale_ok"])
        }
    };
    for _ in 0..4 {
        claude.func(Role::Worker, worker.clone());
    }
    let pool = Pool::new(&ctx, vec![claude.clone()]);
    assert_eq!(run(&ctx, &pool, 1, false), 1);

    let bulletin = fs::read_to_string(ctx.campaign.bulletin_file()).unwrap();
    assert!(bulletin.contains("proved goals no longer check in lean/Apc/Proof/Lib/Stale.lean; reopened: s1"), "{bulletin}");
    assert!(bulletin.contains("goals retired as stale (nothing Solution imports or any unaccepted target needs them): o1, o2"), "{bulletin}");
    assert!(bulletin.contains("orphaned files adopted as repair goals: lean/Apc/Proof/Work/Broken.lean"), "{bulletin}");
    let orch = orch_prompt.lock().unwrap().clone();
    assert!(orch.contains("<import_graph>"), "{orch}");
    assert!(orch.contains("Apc.Proof.Work.Nope [MISSING]"), "{orch}");
    assert!(orch.contains("Apc.Proof.Work.Dangling (lean/Apc/Proof/Work/Dangling.lean) [ORPHAN"), "{orch}");
    let prompts = worker_prompts.lock().unwrap().clone();
    assert!(prompts.iter().any(|p| p.contains("Repair lean/Apc/Proof/Work/Broken.lean") && p.contains("<import_graph>")), "{prompts:?}");
    let st = vdash::util::load_state(&ctx.campaign).unwrap();
    assert_eq!(st.goals["s1"].status, "proved", "{:?}", st.goals);
    assert_eq!(st.goals["o1"].status, "stale", "{:?}", st.goals);
    assert_eq!(st.goals["o2"].status, "stale", "{:?}", st.goals);
    assert!(orch.contains("o1 [stale]"), "{orch}");
    assert_eq!(st.round_data["next_revise_plan"], json!(true), "{:?}", st.round_data);
    assert_eq!(st.goals[repair_id].status, "proved", "{:?}", st.goals);
    assert!(!work.join("Elsewhere.lean").exists());
    // With the orphan repaired, the gate gets past the build to the per-target checks.
    assert!(st.targets.values().all(|t| !t.reasons.iter().any(|r| r == "build_failed")), "{:?}", st.targets);
}

/// M6-04: with an engine running, `/resolve` goes through the inbox; `checkpoint`
/// closes the file, queues the answer as guidance, and the orchestrator's list of
/// escalations drops it.
#[test]
fn resolve_escalation_through_the_engine_inbox() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    vdash::util::update_state(&ctx.campaign, |s| s.round = 3).unwrap();
    vdash::escalation::write_lead(&ctx, 2, "L1", "Should the sortedness lemma carry the cited hypothesis?").unwrap().unwrap();
    vdash::escalation::write_lead(&ctx, 2, "L2", "Is isortBad meant to fail on duplicates as well?").unwrap().unwrap();

    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    let pool = Pool::new(&ctx, vec![fb]);
    let mut eng = engine::Engine::new(ctx.clone(), &pool, Opts::default(), Arc::new(AtomicBool::new(false)));
    vdash::events::send(&ctx, &vdash::events::Control::ResolveEscalation { ns: vec![1], answer: "Yes, cite it.".into() }).unwrap();
    assert!(matches!(eng.checkpoint(3).unwrap(), engine::Flow::Continue));

    let es = vdash::escalation::list(&ctx);
    assert_eq!(es.iter().map(|e| (e.n, e.status.as_str())).collect::<Vec<_>>(), vec![(1, "resolved"), (2, "open")]);
    let st = vdash::util::load_state(&ctx.campaign).unwrap();
    assert_eq!(st.pending_guidance.len(), 1);
    assert!(st.pending_guidance[0].starts_with("Answer to escalation #1 (L1, round 2: Should the sortedness"), "{:?}", st.pending_guidance);
    assert!(fs::read_to_string(ctx.campaign.strategy_file()).unwrap().contains("Yes, cite it."));
    let open = engine::team::open_escalations_text(&ctx);
    assert!(!open.contains("#1 ") && open.contains("#2 (round 2, from L2): Is isortBad"), "{open}");

    // An amendment-backed or unknown number is refused with a note, not a crash.
    vdash::events::send(&ctx, &vdash::events::Control::ResolveEscalation { ns: vec![9], answer: String::new() }).unwrap();
    assert!(matches!(eng.checkpoint(3).unwrap(), engine::Flow::Continue));
    let evs = vdash::events::tail(&ctx.campaign.events_file(), 10).0;
    assert!(evs.iter().any(|e| e.kind == "note" && e.str("text").contains("there is no escalation #9")));
}

#[test]
fn engine_in_memory_gate_cache_handles_unchanged_and_changed_rounds() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    let root = ctx.campaign.root.clone();
    for _ in 0..(1 + vdash::engine::team::WORKER_RETRIES) {
        let r = root.clone();
        fb.func(Role::Worker, move |_call| {
            // Round 1: only write an unimported scratch file; Solution.lean is unchanged.
            fs::write(r.join("lean/Apc/Proof/Work/Scratch.lean"), "def x : Nat := 1\n").unwrap();
            worker_reply("all", &TARGETS)
        });
    }
    fb.func(Role::Worker, move |_call| {
        // Round 2: write reference solution.
        fs::write(root.join("lean/Apc/Solution.lean"), paths::ISORT_REFERENCE_SOLUTION).unwrap();
        worker_reply("all", &TARGETS)
    });
    let pool = Pool::new(&ctx, vec![fb]);
    assert_eq!(run(&ctx, &pool, 2, true), 0);
    let st = vdash::util::load_state(&ctx.campaign).unwrap();
    assert_eq!(st.phase, "done");
    assert!(st.targets.values().all(|t| t.accepted));
    let evs = vdash::events::tail(&ctx.campaign.events_file(), 100).0;
    let gate_evs: Vec<_> = evs.iter().filter(|e| e.kind == "gate").collect();
    assert_eq!(gate_evs.len(), 4, "expected 4 gate events across 2 rounds: {gate_evs:?}");
}
