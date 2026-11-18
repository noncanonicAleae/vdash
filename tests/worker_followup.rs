//! Worker follow-ups (performance pass, 2026-11-18): a worker whose file fails the
//! engine's check is resumed in its own session with only the diagnostics, and a
//! worker whose file then checks cleanly is recorded as proved by the engine without
//! a "confirm proved" call.

mod common;

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
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
    Ctx::for_test(&camp, tmp.path().join("state")).unwrap()
}

#[test]
fn failed_check_resumes_the_session_and_clean_file_is_promoted_without_a_call() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    let root = ctx.campaign.root.clone();
    let claude = Arc::new(FakeBackend::new(BackendKind::Claude));
    let file = "lean/Apc/Proof/Work/Helper.lean";

    let mut goals: Vec<_> = ["Apc.isort_correct", "Apc.isort_fixes_sorted", "Apc.sorted_nonvacuous", "Apc.isortBad_incorrect"]
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"id": format!("t{}", i + 1), "kind": "target", "lean_name": t, "statement": format!("see Challenge.lean: {t}"), "file": "lean/Apc/Solution.lean", "depends_on": [], "parent_target": t}))
        .collect();
    goals.push(json!({"id": "l1", "kind": "lemma", "lean_name": "Apc.helper_ok", "statement": "theorem Apc.helper_ok : 1 = 1", "file": file, "depends_on": [], "parent_target": "Apc.isort_correct"}));
    claude.reply(Role::Architect, json!({"goals": goals, "rationale": "test"}));
    claude.always(
        Role::Orchestrator,
        json!({"leads": [{"lead_id": "L1", "goal_ids": ["l1"], "brief": "prove l1"}], "revise_plan": false, "integrate": false, "notes": ""}),
    );
    claude.always(Role::TeamLead, json!({"summary": "round done", "goal_status": {}, "notes": "", "bulletin": [], "amendments": [], "escalations": []}));
    claude.reply(
        Role::TeamLead,
        json!({"refute": [], "shared": {"definitions": [], "lemma_statements": [], "conventions": ""},
            "assignments": [{"goal_id": "l1", "file": file, "instructions": "prove helper_ok", "existing_lemmas": []}]}),
    );

    // 1st call: a file that does not compile, claimed proved.
    let r1 = root.clone();
    claude.func(Role::Worker, move |_| {
        let p = r1.join(file);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "namespace Apc\ntheorem helper_ok : 1 = 2 := rfl\nend Apc\n").unwrap();
        json!({"goal_id": "l1", "status": "proved", "lean_names": ["Apc.helper_ok"], "new_definitions": [], "notes": "", "amendment": null})
    });
    // 2nd call: must continue the first session with only the diagnostics. It
    // fixes the file but reports `partial` (e.g. it could not run vdash itself).
    let resumed_ok = Arc::new(AtomicBool::new(false));
    let (r2, flag) = (root.clone(), resumed_ok.clone());
    claude.func(Role::Worker, move |call| {
        let short = call.prompt.contains("<diagnostics>") && !call.prompt.contains("<task>");
        flag.store(call.resume.is_some() && short, Ordering::SeqCst);
        fs::write(r2.join(file), "namespace Apc\ntheorem helper_ok : 1 = 1 := rfl\nend Apc\n").unwrap();
        json!({"goal_id": "l1", "status": "partial", "lean_names": [], "new_definitions": [], "notes": "could not run vdash", "amendment": null})
    });

    let pool = Pool::new(&ctx, vec![claude.clone()]);
    engine::run(ctx.clone(), &pool, Opts { rounds: Some(1), single: false }, Arc::new(AtomicBool::new(false))).unwrap();

    assert!(resumed_ok.load(Ordering::SeqCst), "the retry did not resume the worker's session with only the diagnostics");
    // ...and the session it resumed is the first worker call's.
    let calls = claude.calls.lock().unwrap();
    let first = calls.iter().position(|c| c.role == Role::Worker).unwrap();
    let second = calls.iter().rposition(|c| c.role == Role::Worker).unwrap();
    assert_eq!(calls[second].resumed, Some(format!("fake-claude-{}", first + 1)));
    drop(calls);
    // No third "confirm proved" call: the engine's check promoted the claim.
    assert_eq!(claude.count(Role::Worker), 2);
    let st = vdash::util::load_state(&ctx.campaign).unwrap();
    assert_eq!(st.goals["l1"].status, "proved");
}

#[test]
fn witness_claims_are_checked_in_one_closure_run_and_bad_ones_still_fail() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let ctx = approved_isort(&tmp);
    let file = "lean/Apc/Proof/Work/Wit.lean";
    let p = ctx.campaign.root.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(
        &p,
        "namespace Apc\ndef five : Nat := 5\ntheorem five_pos : 0 < five := by decide\ntheorem unrelated : 1 = 1 := rfl\ndef notThm : Nat := 1\nend Apc\n",
    )
    .unwrap();
    let names = vec!["Apc.five_pos".to_string()];
    let def = |w: &str| vec![("Apc.five".to_string(), w.to_string())];
    use vdash::engine::team::verify_claim;
    assert_eq!(verify_claim(&ctx, file, &names, &def("Apc.five_pos")), Ok(()));
    let e = verify_claim(&ctx, file, &names, &def("Apc.unrelated")).unwrap_err();
    assert!(e.contains("does not mention"), "{e}");
    let e = verify_claim(&ctx, file, &["Apc.notThm".to_string()], &def("Apc.five_pos")).unwrap_err();
    assert!(e.contains("not a theorem"), "{e}");
    let e = verify_claim(&ctx, file, &["Apc.missing".to_string()], &def("Apc.five_pos")).unwrap_err();
    assert!(e.contains("does not exist"), "{e}");
    // The plain path (no witnesses) answers the same claim from the memo the second time.
    assert_eq!(verify_claim(&ctx, file, &names, &[]), Ok(()));
    assert_eq!(verify_claim(&ctx, file, &names, &[]), Ok(()));
}
