mod common;

use std::fs;
use std::sync::Arc;

use vdash::amend;
use vdash::anchor;
use vdash::approve;
use vdash::backend::fake::FakeBackend;
use vdash::backend::pool::Pool;
use vdash::config::{BackendKind, Role};
use vdash::events;
use vdash::gate;
use vdash::paths::{init_campaign, Ctx, ISORT_REFERENCE_SOLUTION};
use vdash::util;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn m6_02_early_rejections_and_escalation_without_lean() {
    let tmp = TempDir::new().unwrap();
    let host = tmp.path().join("host");
    fs::create_dir_all(&host).unwrap();
    fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();

    let camp_dir = tmp.path().join("camp");
    init_campaign(&camp_dir, &host, Some("isort"), &[BackendKind::Claude]).unwrap();
    let ctx = Ctx::for_test(&camp_dir, tmp.path().join("state")).unwrap();

    // Also create a proof file and Solution.lean to verify they never leak into critic prompt
    fs::write(
        ctx.campaign.proof_work_dir().join("SecretProof.lean"),
        "-- TOP_SECRET_PROOF_BODY_DO_NOT_LEAK\n",
    )
    .unwrap();
    fs::write(
        ctx.campaign.solution_file(),
        "-- TOP_SECRET_SOLUTION_BODY_DO_NOT_LEAK\n",
    )
    .unwrap();

    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    let pool = Pool::new(&ctx, vec![fb.clone()]);

    // 1. Early rejection: file is not a trusted file
    let p_untrusted = json!({
        "file": "lean/Apc/Solution.lean",
        "old_text": "-- TOP_SECRET_SOLUTION_BODY_DO_NOT_LEAK",
        "new_text": "-- edited",
        "justification": "try editing untrusted file",
        "evidence_kind": "argument",
        "evidence_file": null,
        "evidence_lean_name": null,
        "alternatives_tried": "none"
    });
    let (r1, app1) = amend::propose(&ctx, &pool, 1, &p_untrusted).unwrap();
    assert!(!app1);
    assert_eq!(r1.n, 1);
    assert_eq!(r1.status, "rejected");
    assert!(r1.critic_reason.contains("not a trusted"));
    assert_eq!(fb.count(Role::Critic), 0);

    // 2. Early rejection: old_text does not occur in file
    let p_missing_old = json!({
        "file": "lean/Apc/Spec/Defs.lean",
        "old_text": "THIS_DOES_NOT_EXIST_IN_DEFS",
        "new_text": "replacement",
        "justification": "nonexistent old_text",
        "evidence_kind": "argument",
        "evidence_file": null,
        "evidence_lean_name": null,
        "alternatives_tried": "none"
    });
    let (r2, app2) = amend::propose(&ctx, &pool, 1, &p_missing_old).unwrap();
    assert!(!app2);
    assert_eq!(r2.n, 2);
    assert_eq!(r2.status, "rejected");
    assert!(r2.critic_reason.contains("exactly once"));
    assert_eq!(fb.count(Role::Critic), 0);

    // 3. Early rejection: new_text adds more than 15 lines
    let mut twenty_lines = String::from("  | [] => True\n");
    for i in 0..17 {
        twenty_lines.push_str(&format!("  -- extra line {i}\n"));
    }
    let p_too_long = json!({
        "file": "lean/Apc/Spec/Defs.lean",
        "old_text": "  | [] => True",
        "new_text": twenty_lines,
        "justification": "adds more than 15 lines",
        "evidence_kind": "argument",
        "evidence_file": null,
        "evidence_lean_name": null,
        "alternatives_tried": "none"
    });
    let (r3, app3) = amend::propose(&ctx, &pool, 1, &p_too_long).unwrap();
    assert!(!app3);
    assert_eq!(r3.n, 3);
    assert_eq!(r3.status, "rejected");
    assert!(r3.critic_reason.contains("more than 15 lines"));
    assert_eq!(fb.count(Role::Critic), 0);

    // 4. Escalation: critic returns `escalate` -> writes both amendments/004-*.md and escalations/004-*.md
    fb.reply(
        Role::Critic,
        json!({
            "verdict": "escalate",
            "touches_trusted_assumptions": true,
            "closer_to_code": false,
            "reason": "Criterion 2 failed: weakens the guarantee to True."
        }),
    );
    let p_escalate = json!({
        "file": "lean/Apc/Spec/Defs.lean",
        "old_text": "  | x :: y :: r => x ≤ y ∧ Sorted (y :: r)",
        "new_text": "  | _ :: y :: r => Sorted (y :: r)",
        "justification": "drop comparison so proof goes through",
        "evidence_kind": "counterexample",
        "evidence_file": null,
        "evidence_lean_name": null,
        "alternatives_tried": "induction"
    });
    let (r4, app4) = amend::propose(&ctx, &pool, 1, &p_escalate).unwrap();
    assert!(!app4);
    assert_eq!(r4.n, 4);
    assert_eq!(r4.status, "escalated");
    assert_eq!(fb.count(Role::Critic), 1);
    assert!(ctx.campaign.root.join(r4.rel_path()).is_file());
    assert!(ctx.campaign.root.join(r4.escalation_rel_path()).is_file());

    // Verify events emitted include both `amendment` and `escalation`
    let evs = events::tail(&ctx.campaign.events_file(), 20).0;
    assert!(evs
        .iter()
        .any(|e| e.kind == "amendment" && e.u64("n") == Some(4) && e.str("status") == "escalated"));
    assert!(evs
        .iter()
        .any(|e| e.kind == "escalation" && e.u64("n") == Some(4)));

    // An escalated proposal is not approved by a plain `approve`: it needs
    // `--accept-weakening`. A human can dismiss it with `reject`.
    let err = amend::approve_cmd(&ctx, 4, false).unwrap_err();
    assert_eq!(err.code, 2);
    assert!(err.message.contains("--accept-weakening"), "msg: {}", err.message);
    assert!(err.message.contains(&r4.escalation_rel_path()), "msg: {}", err.message);
    let err = amend::apply(&ctx, 4, "user", false).unwrap_err();
    assert!(err.message.contains("--accept-weakening"), "msg: {}", err.message);
    amend::reject_cmd(&ctx, 4).unwrap();
    assert_eq!(amend::load_record(&ctx, 4).unwrap().status, "rejected");
    assert!(ctx.campaign.root.join(r4.escalation_rel_path()).is_file());

    // Verify critic prompt contains NO content from Proof/ or Solution.lean
    let calls = fb.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let critic_prompt = &calls[0].prompt;
    assert!(!critic_prompt.contains("TOP_SECRET_PROOF_BODY_DO_NOT_LEAK"));
    assert!(!critic_prompt.contains("TOP_SECRET_SOLUTION_BODY_DO_NOT_LEAK"));
    assert!(critic_prompt.contains("lean/Apc/Spec/Defs.lean"));
}

#[test]
fn m6_02_and_m6_03_disproof_apply_gate_and_rollback_with_lean() {
    require_lean!();
    let host = common::real_host_project();

    let tmp = TempDir::new().unwrap();
    let camp_dir = tmp.path().join("isort_amend");
    let state_base = tmp.path().join("state");

    init_campaign(&camp_dir, &host, Some("isort"), &[BackendKind::Claude]).unwrap();

    // Start with a too-strict `Sorted` in Spec/Defs.lean (`x < y` instead of `x ≤ y`)
    // so that `isort_fixes_sorted` on `[1, 1]` is false and can be disproved!
    let defs_path = camp_dir.join("lean/Apc/Spec/Defs.lean");
    let orig_defs = fs::read_to_string(&defs_path).unwrap();
    let strict_defs = orig_defs.replace("x ≤ y ∧ Sorted (y :: r)", "x < y ∧ Sorted (y :: r)");
    fs::write(&defs_path, &strict_defs).unwrap();

    let mut ctx = Ctx::for_test(&camp_dir, state_base.join("vdash")).unwrap();
    ctx.cfg.policy.kernel_recheck = false;

    // Approve the strict spec
    approve::approve(&ctx).unwrap();
    assert!(anchor::verify(&ctx).is_empty());

    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    let pool = Pool::new(&ctx, vec![fb.clone()]);

    // 1. Write an UNVERIFIED disproof (uses `sorry`) in lean/Apc/Proof/Work/BadDisproof.lean
    let bad_disproof_path = ctx.campaign.proof_work_dir().join("BadDisproof.lean");
    fs::write(
        &bad_disproof_path,
        "import Apc.Spec.Defs\nnamespace Apc.Proof.Work\ntheorem not_strict : ¬ Spec.Sorted [1, 1] := by sorry\nend Apc.Proof.Work\n",
    )
    .unwrap();

    let p_unverified = json!({
        "file": "lean/Apc/Spec/Defs.lean",
        "old_text": "  | x :: y :: r => x < y ∧ Sorted (y :: r)",
        "new_text": "  | x :: y :: r => x ≤ y ∧ Sorted (y :: r)",
        "justification": "Strict `<` rejects duplicate elements.",
        "evidence_kind": "disproof",
        "evidence_file": "lean/Apc/Proof/Work/BadDisproof.lean",
        "evidence_lean_name": "Apc.Proof.Work.not_strict",
        "alternatives_tried": "direct induction"
    });
    let (r_unv, app_unv) = amend::propose(&ctx, &pool, 1, &p_unverified).unwrap();
    assert!(!app_unv);
    assert_eq!(r_unv.status, "rejected");
    assert_eq!(r_unv.critic_reason, "evidence not machine-checked");
    assert_eq!(fb.count(Role::Critic), 0, "unverified disproof must not call critic");

    // 2. Write a VERIFIED machine-checked disproof (no `sorry`) in lean/Apc/Proof/Work/Disproof.lean
    let good_disproof_path = ctx.campaign.proof_work_dir().join("Disproof.lean");
    fs::write(
        &good_disproof_path,
        "import Apc.Spec.Defs\nimport Apc.Spec.Impl\nnamespace Apc.Proof.Work\n-- SECRET_PROOF_COMMENT_MUST_NOT_REACH_CRITIC\ntheorem strict_sorted_fails_dups : ¬ Spec.Correct Spec.isort := by\n  intro h\n  have hs : Spec.Sorted (Spec.isort [1, 1]) := (h [1, 1]).1\n  exact Nat.lt_irrefl 1 hs.1\nend Apc.Proof.Work\n",
    )
    .unwrap();

    fb.reply(
        Role::Critic,
        json!({
            "verdict": "accept",
            "touches_trusted_assumptions": false,
            "closer_to_code": true,
            "reason": "Impl.insert uses `x ≤ y`, so `Sorted` must use `≤` rather than strict `<`."
        }),
    );

    let p_verified = json!({
        "file": "lean/Apc/Spec/Defs.lean",
        "old_text": "  | x :: y :: r => x < y ∧ Sorted (y :: r)",
        "new_text": "  | x :: y :: r => x ≤ y ∧ Sorted (y :: r)",
        "justification": "Impl.insert uses `≤`, so `Sorted` with `<` is false on lists with duplicates.",
        "evidence_kind": "disproof",
        "evidence_file": "lean/Apc/Proof/Work/Disproof.lean",
        "evidence_lean_name": "Apc.Proof.Work.strict_sorted_fails_dups",
        "alternatives_tried": "tried proving for strict `<`"
    });
    let (r_ver, app_ver) = amend::propose(&ctx, &pool, 1, &p_verified).unwrap();
    assert!(!app_ver, "manual policy leaves accepted proposal pending");
    assert_eq!(r_ver.n, 2);
    assert_eq!(r_ver.status, "pending");
    assert!(r_ver.evidence_verified);
    assert_eq!(fb.count(Role::Critic), 1, "verified disproof must reach critic");

    // Check that the critic prompt contained the theorem statement, NOT the proof file content
    {
        let calls = fb.calls.lock().unwrap();
        let prompt = &calls[0].prompt;
        assert!(prompt.contains("strict_sorted_fails_dups"));
        assert!(!prompt.contains("SECRET_PROOF_COMMENT_MUST_NOT_REACH_CRITIC"));
    }

    // Check `vdash amend list`
    let list_out = amend::list_cmd(&ctx).unwrap();
    assert!(list_out.contains("001  rejected"));
    assert!(list_out.contains("002  pending"));

    // 3. Before applying amendment 002, a solution written against the amended spec (`≤`) is REJECTED
    //    because `.heldout/` still has `<`!
    fs::write(ctx.campaign.solution_file(), ISORT_REFERENCE_SOLUTION).unwrap();
    let v_before = gate::run(&ctx).unwrap();
    assert!(!v_before.ok, "solution for `≤` must be rejected while heldout still has `<`");

    // 4. Approve & apply amendment 002 via `approve_cmd`
    let app_msg = amend::approve_cmd(&ctx, 2, false).unwrap();
    assert!(app_msg.contains("applied"));

    // Approving a non-pending amendment returns code 2
    let err_twice = amend::approve_cmd(&ctx, 2, false).unwrap_err();
    assert_eq!(err_twice.code, 2);

    // Anchor verification is clean after `amend::apply`
    let problems = anchor::verify(&ctx);
    assert!(problems.is_empty(), "anchor verify problems after apply: {problems:?}");

    // Now the gate ACCEPTS the reference solution written against the amended spec (`≤`)!
    let v_after = gate::run(&ctx).unwrap();
    assert!(
        v_after.ok,
        "gate should accept solution against amended spec, got: {v_after:?}"
    );

    // And a solution that redefines/uses the OLD strict `<` spec in the work tree is REJECTED!
    fs::write(&defs_path, &strict_defs).unwrap();
    let v_old = gate::run(&ctx).unwrap();
    assert!(!v_old.ok, "solution against old spec must be rejected");
    // Restore the amended work tree Defs.lean
    fs::write(&defs_path, &orig_defs).unwrap();

    // 5. Test broken amendment rollback: propose an amendment that breaks `Spec/Defs.lean`
    fb.reply(
        Role::Critic,
        json!({
            "verdict": "accept",
            "touches_trusted_assumptions": false,
            "closer_to_code": true,
            "reason": "ok"
        }),
    );
    let p_broken = json!({
        "file": "lean/Apc/Spec/Defs.lean",
        "old_text": "  | x :: y :: r => x ≤ y ∧ Sorted (y :: r)",
        "new_text": "  | x :: y :: r => THIS_DOES_NOT_TYPECHECK",
        "justification": "broken syntax test",
        "evidence_kind": "argument",
        "evidence_file": null,
        "evidence_lean_name": null,
        "alternatives_tried": "none"
    });
    let (r_brk, _) = amend::propose(&ctx, &pool, 2, &p_broken).unwrap();
    assert_eq!(r_brk.n, 3);
    assert_eq!(r_brk.status, "pending");

    let apply_err = amend::apply(&ctx, 3, "user", false).unwrap_err();
    assert_eq!(apply_err.code, 1);
    assert!(apply_err.message.contains("rolled back"));

    // Verify work tree, heldout copy, and anchor are completely rolled back and clean!
    assert_eq!(fs::read_to_string(&defs_path).unwrap(), orig_defs);
    assert_eq!(
        fs::read_to_string(ctx.campaign.heldout_dir().join("lean/Apc/Spec/Defs.lean")).unwrap(),
        orig_defs
    );
    assert!(anchor::verify(&ctx).is_empty());
    let st = util::load_state(&ctx.campaign).unwrap();
    assert_eq!(st.phase, "approved");

    // 6. An escalated weakening applies only with --accept-weakening, and is marked.
    fb.reply(
        Role::Critic,
        json!({
            "verdict": "escalate",
            "touches_trusted_assumptions": true,
            "closer_to_code": true,
            "reason": "makes Sorted trivially true"
        }),
    );
    let p_weaken = json!({
        "file": "lean/Apc/Spec/Defs.lean",
        "old_text": "  | x :: y :: r => x ≤ y ∧ Sorted (y :: r)",
        "new_text": "  | _ :: _ :: _ => True",
        "justification": "weakening test",
        "evidence_kind": "argument",
        "evidence_file": null,
        "evidence_lean_name": null,
        "alternatives_tried": "none"
    });
    let (r_wk, _) = amend::propose(&ctx, &pool, 3, &p_weaken).unwrap();
    assert_eq!(r_wk.n, 4);
    assert_eq!(r_wk.status, "escalated");
    assert_eq!(amend::approve_cmd(&ctx, 4, false).unwrap_err().code, 2);
    // The flag is refused on a record that is not escalated (003 is still pending).
    let err = amend::approve_cmd(&ctx, 3, true).unwrap_err();
    assert!(err.message.contains("not escalated"), "msg: {}", err.message);
    let msg = amend::approve_cmd(&ctx, 4, true).unwrap();
    assert!(msg.contains("weaker guarantee accepted"), "msg: {msg}");
    let rec = amend::load_record(&ctx, 4).unwrap();
    assert_eq!(rec.status, "applied");
    assert!(rec.weakening_accepted);
    assert!(fs::read_to_string(ctx.campaign.root.join(rec.rel_path())).unwrap().contains("Weakening accepted"));
    assert!(amend::list_cmd(&ctx).unwrap().contains("[WEAKENING ACCEPTED]"));
    assert!(anchor::verify(&ctx).is_empty());
}
