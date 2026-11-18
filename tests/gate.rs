mod common;

use std::fs;
use tempfile::TempDir;

use vdash::gate::{self, ReasonCode, Verdict};
use vdash::paths::{self, Campaign, Ctx};
use vdash::util;

#[test]
fn m1_11_gate_all_thirteen_scenarios() {
    require_lean!();

    let tmp = TempDir::new().unwrap();
    let camp_dir = tmp.path().join("gate_camp");
    let xdg_state = tmp.path().join("xdg_state");
    fs::create_dir_all(&xdg_state).unwrap();
    let host = common::real_host_project();
    let apc_bin = assert_cmd::cargo::cargo_bin!("vdash");

    // Initialize and approve isort campaign
    assert_cmd::Command::new(apc_bin)
        .args([
            "init",
            camp_dir.to_str().unwrap(),
            "--host",
            host.to_str().unwrap(),
            "--example",
            "isort",
        ])
        .assert()
        .success();

    assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["-C", camp_dir.to_str().unwrap(), "approve"])
        .assert()
        .success();

    let camp = Campaign::new(&camp_dir);
    let cfg = vdash::config::load(&camp_dir).unwrap();
    let state_dir = xdg_state.join("vdash").join(&cfg.id);
    let ctx = Ctx {
        campaign: camp.clone(),
        cfg,
        state_dir,
        exe: std::path::PathBuf::from(apc_bin),
    };

    let sol_path = camp.solution_file();
    let defs_path = camp.lean_spec_dir().join("Defs.lean");
    let orig_defs = fs::read_to_string(&defs_path).unwrap();
    let ref_sol = paths::ISORT_REFERENCE_SOLUTION;

    // (a) Reference solution -> all four accepted, exit 0
    fs::write(&sol_path, ref_sol).unwrap();
    let assert_a = assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["-C", camp_dir.to_str().unwrap(), "gate"])
        .assert()
        .code(0);
    let stdout_a = String::from_utf8_lossy(&assert_a.get_output().stdout);
    for t in [
        "Apc.isort_correct",
        "Apc.isort_fixes_sorted",
        "Apc.sorted_nonvacuous",
        "Apc.isortBad_incorrect",
    ] {
        assert!(
            stdout_a.contains(&format!("ACCEPTED {t}")),
            "missing ACCEPTED {t} in:\n{stdout_a}"
        );
    }
    let st_after_a = util::load_state(&camp).unwrap();
    for t in st_after_a.targets.values() {
        assert!(t.accepted);
        assert_eq!(t.since_round, Some(0));
    }

    // (l) Two consecutive runs -> identical --json output
    let json1 = assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["-C", camp_dir.to_str().unwrap(), "gate", "--json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let json2 = assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["-C", camp_dir.to_str().unwrap(), "gate", "--json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert_eq!(json1, json2, "(l) consecutive --json runs differed");
    let v_l: Verdict = serde_json::from_slice(&json1).unwrap();
    assert!(v_l.ok);
    assert!(v_l.anchor_ok);

    // Disable kernel_recheck in memory for faster negative-case checks (already tested in (a) & (l))
    let mut fast_ctx = ctx.clone();
    fast_ctx.cfg.policy.kernel_recheck = false;

    // (k) Comment-only change in work tree's Spec -> still accepted
    fs::write(&defs_path, format!("-- comment only\n{orig_defs}\n")).unwrap();
    let v_k = gate::run(&fast_ctx).unwrap();
    assert!(v_k.ok, "(k) comment-only change should be accepted: {v_k:?}");
    fs::write(&defs_path, &orig_defs).unwrap();

    // (b) Sorted weakened in work tree's Spec/Defs.lean -> closure_mismatch naming Apc.Spec.Sorted
    let weak_defs = orig_defs.replace("x ≤ y ∧ Sorted (y :: r)", "True");
    fs::write(&defs_path, &weak_defs).unwrap();
    let weak_sol = format!(
        "{}\ntheorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := ⟨[1, 2], by decide, trivial⟩\n",
        ref_sol
            .lines()
            .take_while(|l| !l.starts_with("theorem isort_correct"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // Even if we keep only valid proofs or adapt proofs to weak_defs:
    let weak_full_sol = r#"import Apc.Spec.Impl
import Apc.Spec.Defs
import Apc.Spec.Mutants

namespace Apc

theorem isort_correct : Spec.Correct Spec.isort := by
  intro l
  refine ⟨?_, ?_⟩
  · cases Spec.isort l with
    | nil => trivial
    | cons a as => cases as <;> trivial
  · sorry

theorem isort_fixes_sorted : ∀ (l : List Nat), Spec.Sorted l → Spec.isort l = l := by sorry

theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l :=
  ⟨[2, 1], by decide, trivial⟩

theorem isortBad_incorrect : ¬ Spec.Correct Spec.isortBad := by sorry

end Apc
"#;
    let _ = weak_sol;
    fs::write(&sol_path, weak_full_sol).unwrap();
    let v_b = gate::run(&fast_ctx).unwrap();
    assert!(!v_b.ok);
    let sn_b = &v_b.targets["Apc.sorted_nonvacuous"];
    assert!(!sn_b.accepted);
    assert!(
        sn_b.reasons.contains(&ReasonCode::ClosureMismatch),
        "(b) expected ClosureMismatch, got {:?}",
        sn_b
    );
    let changed_b = sn_b.detail["changed"].as_array().unwrap();
    assert!(
        changed_b.iter().any(|v| v.as_str() == Some("Apc.Spec.Sorted")),
        "(b) expected Apc.Spec.Sorted in changed, got {changed_b:?}"
    );
    fs::write(&defs_path, &orig_defs).unwrap();

    // (f) Hypothesis predicate Sorted redefined to return False in work tree (swarm trick) -> rejected
    let false_defs = orig_defs
        .replace("| [] => True", "| [] => False")
        .replace("| [_] => True", "| [_] => False")
        .replace("| x :: y :: r => x ≤ y ∧ Sorted (y :: r)", "| _ :: _ :: _ => False");
    fs::write(&defs_path, &false_defs).unwrap();
    let false_sol = r#"import Apc.Spec.Impl
import Apc.Spec.Defs
import Apc.Spec.Mutants

namespace Apc

theorem isort_fixes_sorted (l : List Nat) (h : Spec.Sorted l) : Spec.isort l = l := by
  cases l with
  | nil => exact False.elim h
  | cons x xs =>
    cases xs with
    | nil => exact False.elim h
    | cons y ys => exact False.elim h

end Apc
"#;
    fs::write(&sol_path, false_sol).unwrap();
    let v_f = gate::run(&fast_ctx).unwrap();
    assert!(!v_f.ok);
    assert!(!v_f.targets["Apc.isort_fixes_sorted"].accepted);
    assert!(v_f.targets["Apc.isort_fixes_sorted"]
        .reasons
        .contains(&ReasonCode::ClosureMismatch));
    fs::write(&defs_path, &orig_defs).unwrap();

    // (c.1) One proof replaced by sorry -> bad_axioms with sorryAx
    let sorry_sol = ref_sol.replace(
        "theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := by\n  refine ⟨[1, 2], ?_, ?_⟩\n  · decide\n  · exact ⟨by decide, trivial⟩",
        "theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := by sorry",
    );
    assert_ne!(sorry_sol, ref_sol);
    fs::write(&sol_path, &sorry_sol).unwrap();
    let v_c1 = gate::run(&fast_ctx).unwrap();
    assert!(!v_c1.ok);
    assert!(v_c1.targets["Apc.isort_correct"].accepted);
    assert!(!v_c1.targets["Apc.sorted_nonvacuous"].accepted);
    assert_eq!(
        v_c1.targets["Apc.sorted_nonvacuous"].reasons,
        vec![ReasonCode::BadAxioms]
    );
    assert!(v_c1.targets["Apc.sorted_nonvacuous"].detail["bad_axioms"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x.as_str() == Some("sorryAx")));

    // (c.2) Proof using native_decide -> forbidden_source
    let nd_sol = ref_sol.replace("· decide", "· native_decide");
    fs::write(&sol_path, &nd_sol).unwrap();
    let v_c2 = gate::run(&fast_ctx).unwrap();
    assert!(!v_c2.ok);
    for tv in v_c2.targets.values() {
        assert!(tv.reasons.contains(&ReasonCode::ForbiddenSource));
    }

    // (c.3) Custom axiom -> forbidden_source
    let ax_sol = ref_sol.replace(
        "theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := by\n  refine ⟨[1, 2], ?_, ?_⟩\n  · decide\n  · exact ⟨by decide, trivial⟩",
        "axiom cheat_ax : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l\ntheorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := cheat_ax",
    );
    fs::write(&sol_path, &ax_sol).unwrap();
    let v_c3 = gate::run(&fast_ctx).unwrap();
    assert!(!v_c3.ok);
    for tv in v_c3.targets.values() {
        assert!(tv.reasons.contains(&ReasonCode::ForbiddenSource));
    }

    // (d) Target restated with a different statement -> statement_mismatch
    let diff_stmt_sol = ref_sol.replace(
        "theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := by\n  refine ⟨[1, 2], ?_, ?_⟩\n  · decide\n  · exact ⟨by decide, trivial⟩",
        "theorem sorted_nonvacuous : True := trivial",
    );
    fs::write(&sol_path, &diff_stmt_sol).unwrap();
    let v_d = gate::run(&fast_ctx).unwrap();
    assert!(!v_d.ok);
    assert!(v_d.targets["Apc.sorted_nonvacuous"]
        .reasons
        .contains(&ReasonCode::StatementMismatch));

    // (g) Correct re-declared inside Apc.Proof.Foo and used in restated target -> rejected (closure_mismatch or untrusted_dependency)
    let foo_path = camp.proof_dir().join("Foo.lean");
    fs::write(
        &foo_path,
        "namespace Apc.Proof\ndef Correct (_ : List Nat → List Nat) : Prop := True\nend Apc.Proof\n",
    )
    .unwrap();
    let g_sol = format!(
        "import Apc.Proof.Foo\n{}",
        ref_sol.replace(
            "theorem isort_correct : Spec.Correct Spec.isort := by\n  intro l\n  exact ⟨isort_sorted l, isort_perm l⟩",
            "theorem isort_correct : Proof.Correct Spec.isort := trivial",
        )
    );
    fs::write(&sol_path, &g_sol).unwrap();
    let v_g = gate::run(&fast_ctx).unwrap();
    assert!(!v_g.ok);
    let ic_g = &v_g.targets["Apc.isort_correct"];
    assert!(!ic_g.accepted);
    assert!(
        ic_g.reasons.contains(&ReasonCode::ClosureMismatch)
            || ic_g.reasons.contains(&ReasonCode::UntrustedDependency),
        "(g) expected ClosureMismatch or UntrustedDependency, got {:?}",
        ic_g.reasons
    );
    fs::remove_file(&foo_path).unwrap();

    // (h) Target omitted from Solution.lean -> missing, others unaffected
    let omit_sol = ref_sol.replace(
        "theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := by\n  refine ⟨[1, 2], ?_, ?_⟩\n  · decide\n  · exact ⟨by decide, trivial⟩",
        "",
    );
    fs::write(&sol_path, &omit_sol).unwrap();
    let v_h = gate::run(&fast_ctx).unwrap();
    assert!(!v_h.ok);
    assert_eq!(
        v_h.targets["Apc.sorted_nonvacuous"].reasons,
        vec![ReasonCode::Missing]
    );
    assert!(v_h.targets["Apc.isort_correct"].accepted);
    assert!(v_h.targets["Apc.isort_fixes_sorted"].accepted);
    assert!(v_h.targets["Apc.isortBad_incorrect"].accepted);

    // (i) Target restated as def -> not_theorem
    let def_sol = ref_sol.replace(
        "theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l :=",
        "def sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l :=",
    );
    fs::write(&sol_path, &def_sol).unwrap();
    let v_i = gate::run(&fast_ctx).unwrap();
    assert!(!v_i.ok);
    assert!(v_i.targets["Apc.sorted_nonvacuous"]
        .reasons
        .contains(&ReasonCode::NotTheorem));

    // (j) `instance : LE Nat := ⟨fun _ _ => True⟩` placed before restated targets -> statement_mismatch
    let inst_sol = ref_sol.replace(
        "theorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := by\n  refine ⟨[1, 2], ?_, ?_⟩\n  · decide\n  · exact ⟨by decide, trivial⟩",
        "local instance (priority := high) : LE Nat := ⟨fun _ _ => True⟩\ntheorem sorted_nonvacuous : ∃ l : List Nat, 2 ≤ l.length ∧ Spec.Sorted l := ⟨[1, 2], trivial, ⟨by decide, trivial⟩⟩",
    );
    fs::write(&sol_path, &inst_sol).unwrap();
    let v_j = gate::run(&fast_ctx).unwrap();
    assert!(!v_j.ok);
    assert!(v_j.targets["Apc.sorted_nonvacuous"]
        .reasons
        .contains(&ReasonCode::StatementMismatch));

    // (m) Solution.lean importing Apc.Challenge -> rejected (build_failed)
    fs::write(&sol_path, format!("import Apc.Challenge\n{ref_sol}")).unwrap();
    let v_m = gate::run(&fast_ctx).unwrap();
    assert!(!v_m.ok);
    for tv in v_m.targets.values() {
        assert_eq!(tv.reasons, vec![ReasonCode::BuildFailed]);
    }

    // (n) New Spec/Evil.lean with `set_option debug.skipKernelTC true` imported by Solution.lean -> rejected (forbidden_source)
    let evil_spec = camp.lean_spec_dir().join("Evil.lean");
    fs::write(&evil_spec, "set_option debug.skipKernelTC true\n").unwrap();
    fs::write(&sol_path, format!("import Apc.Spec.Evil\n{ref_sol}")).unwrap();
    let v_n = gate::run(&fast_ctx).unwrap();
    assert!(!v_n.ok);
    for tv in v_n.targets.values() {
        assert!(tv.reasons.contains(&ReasonCode::ForbiddenSource));
    }
    fs::remove_file(&evil_spec).unwrap();

    // (e0) comparator_cmd agrees vs disagrees -> comparator_disagrees with detail
    fs::write(&sol_path, ref_sol).unwrap();
    let cfg_path = camp.config_file();
    let orig_cfg = fs::read_to_string(&cfg_path).unwrap();
    let with_cmp = |cmd: &str| {
        if orig_cfg.contains("comparator_cmd = \"\"") {
            orig_cfg.replace(
                "comparator_cmd = \"\"",
                &format!("comparator_cmd = \"{cmd}\""),
            )
        } else {
            format!("{orig_cfg}\n[policy]\ncomparator_cmd = \"{cmd}\"\n")
        }
    };
    fs::write(&cfg_path, with_cmp("true")).unwrap();
    let assert_cmp_ok = assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["-C", camp_dir.to_str().unwrap(), "gate", "--json"])
        .assert()
        .code(0);
    let v_cmp_ok: Verdict = serde_json::from_slice(&assert_cmp_ok.get_output().stdout).unwrap();
    assert!(v_cmp_ok.ok);

    fs::write(&cfg_path, with_cmp("false")).unwrap();
    let assert_cmp_fail = assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["-C", camp_dir.to_str().unwrap(), "gate", "--json"])
        .assert()
        .code(1);
    let v_cmp_fail: Verdict = serde_json::from_slice(&assert_cmp_fail.get_output().stdout).unwrap();
    assert!(!v_cmp_fail.ok);
    for tv in v_cmp_fail.targets.values() {
        assert_eq!(tv.reasons, vec![ReasonCode::ComparatorDisagrees]);
        assert!(tv.detail.get("comparator").is_some());
    }
    fs::write(&cfg_path, &orig_cfg).unwrap();

    // (e) .heldout/ edited -> exit 3, anchor_mismatch
    let held_impl = camp.heldout_dir().join("lean/Apc/Spec/Impl.lean");
    util::set_readonly(&held_impl, false).unwrap();
    fs::write(&held_impl, "-- tampered heldout\n").unwrap();
    let assert_e = assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["-C", camp_dir.to_str().unwrap(), "gate", "--json"])
        .assert()
        .code(3);
    let v_e: Verdict = serde_json::from_slice(&assert_e.get_output().stdout).unwrap();
    assert!(!v_e.ok);
    assert!(!v_e.anchor_ok);
    for tv in v_e.targets.values() {
        assert_eq!(tv.reasons, vec![ReasonCode::AnchorMismatch]);
    }
}

#[test]
fn gate_cache_reuses_unchanged_parallel_leanchecker_and_fresh_rebuilds() {
    require_lean!();

    let tmp = TempDir::new().unwrap();
    let camp_dir = tmp.path().join("gate_cache_camp");
    let xdg_state = tmp.path().join("xdg_state");
    fs::create_dir_all(&xdg_state).unwrap();
    let host = common::real_host_project();
    let apc_bin = assert_cmd::cargo::cargo_bin!("vdash");

    assert_cmd::Command::new(apc_bin)
        .args([
            "init",
            camp_dir.to_str().unwrap(),
            "--host",
            host.to_str().unwrap(),
            "--example",
            "isort",
        ])
        .assert()
        .success();

    assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["-C", camp_dir.to_str().unwrap(), "approve"])
        .assert()
        .success();

    let camp = Campaign::new(&camp_dir);
    let cfg = vdash::config::load(&camp_dir).unwrap();
    let state_dir = xdg_state.join("vdash").join(&cfg.id);
    let ctx = Ctx {
        campaign: camp.clone(),
        cfg,
        state_dir: state_dir.clone(),
        exe: std::path::PathBuf::from(apc_bin),
    };

    // Add two untrusted Proof modules imported by Solution.lean so parallel leanchecker runs.
    fs::create_dir_all(camp.proof_lib_dir()).unwrap();
    let mod_a = camp.proof_lib_dir().join("HelperA.lean");
    let mod_b = camp.proof_lib_dir().join("HelperB.lean");
    fs::write(&mod_a, "theorem helper_a : 1 + 1 = 2 := rfl\n").unwrap();
    fs::write(&mod_b, "theorem helper_b : 2 + 2 = 4 := rfl\n").unwrap();
    let sol = format!(
        "import Apc.Proof.Lib.HelperA\nimport Apc.Proof.Lib.HelperB\n{}",
        paths::ISORT_REFERENCE_SOLUTION
    );
    fs::write(camp.solution_file(), &sol).unwrap();

    let v1 = gate::run(&ctx).unwrap();
    assert!(v1.ok, "first gate run failed: {v1:?}");

    let gate_cache = state_dir.join("gate_cache");
    assert!(gate_cache.join("sol_fps.json").is_file());
    assert!(gate_cache.join("sol_build/Apc/Solution.olean.lc_ok").is_file());
    assert!(gate_cache.join("sol_build/Apc/Proof/Lib/HelperA.olean.lc_ok").is_file());
    assert!(gate_cache.join("sol_build/Apc/Proof/Lib/HelperB.olean.lc_ok").is_file());
    assert!(state_dir.join("fp_cache").is_dir());

    // Second run with no edits hits cache and produces identical verdict.
    let v2 = gate::run(&ctx).unwrap();
    assert_eq!(v1, v2);

    // Dropping HelperB from Solution.lean prunes stale HelperB.olean from gate_cache/sol_build.
    fs::write(
        camp.solution_file(),
        format!("import Apc.Proof.Lib.HelperA\n{}", paths::ISORT_REFERENCE_SOLUTION),
    )
    .unwrap();
    let v3 = gate::run(&ctx).unwrap();
    assert!(v3.ok, "third gate run failed: {v3:?}");
    assert!(!gate_cache.join("sol_build/Apc/Proof/Lib/HelperB.olean").exists());
    assert!(gate_cache.join("sol_build/Apc/Proof/Lib/HelperA.olean.lc_ok").is_file());

    // `--fresh` ignores the cache and exits 0.
    let out_fresh = assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["-C", camp_dir.to_str().unwrap(), "gate", "--fresh", "--json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let v_fresh: Verdict = serde_json::from_slice(&out_fresh).unwrap();
    assert_eq!(v_fresh, v3);
}
