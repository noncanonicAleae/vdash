mod common;

use std::fs;
use std::time::Duration;
use tempfile::TempDir;

use vdash::config::BackendKind;
use vdash::lean;
use vdash::paths::{self, Ctx};
use vdash::proc;

#[test]
fn lean_version_and_lean_path_cache_reuse() {
    require_lean!();

    let tmp = TempDir::new().unwrap();
    let camp_dir = tmp.path().join("camp");
    let state_dir = tmp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();

    let host = common::real_host_project();
    paths::init_campaign(&camp_dir, &host, None, &[BackendKind::Claude]).unwrap();

    let ctx = Ctx::for_test(&camp_dir, &state_dir).unwrap();
    let tc = lean::toolchain(&ctx.cfg).unwrap();
    assert_eq!(tc, "leanprover/lean4:v4.33.0");

    // 1. Child lean --version run from a temp dir reports the host's version
    let mut cmd = lean::command(&ctx, "lean").unwrap();
    cmd.arg("--version").current_dir(tmp.path());
    let out = proc::run(&mut cmd, None, Duration::from_secs(10)).unwrap();
    assert_eq!(out.status, Some(0));
    assert!(
        out.stdout_lossy().contains("4.33.0"),
        "unexpected lean --version: {}",
        out.stdout_lossy()
    );

    // 2. lean_path with use_mathlib = true populates cache and reuses it on second call
    let build_dir = ctx.campaign.build_work_dir();
    let lp1 = lean::lean_path(&ctx, &build_dir).unwrap();
    assert!(lp1.starts_with(build_dir.to_str().unwrap()));
    assert!(lp1.contains(':'));
    let cache_file = ctx.campaign.lean_path_cache();
    assert!(cache_file.is_file());

    // Tamper with host_project in memory or check that modifying cache's lean_path value
    // while keeping manifest_mtime intact is returned on the second call (proving reuse)
    let mut cached: serde_json::Value = vdash::jsonio::read_json(&cache_file).unwrap();
    cached["lean_path"] = serde_json::Value::String("/sentinel/cached/path".to_string());
    vdash::jsonio::write_atomic(&cache_file, &cached).unwrap();

    let lp2 = lean::lean_path(&ctx, &build_dir).unwrap();
    assert_eq!(
        lp2,
        format!("{}:/sentinel/cached/path", build_dir.display())
    );
}

#[test]
fn lean_path_without_mathlib_returns_build_dir_only() {
    let tmp = TempDir::new().unwrap();
    let host = tmp.path().join("host");
    fs::create_dir_all(&host).unwrap();
    fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();

    let camp_dir = tmp.path().join("camp");
    let state_dir = tmp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();

    paths::init_campaign(&camp_dir, &host, Some("isort"), &[BackendKind::Claude]).unwrap();
    let ctx = Ctx::for_test(&camp_dir, &state_dir).unwrap();
    let build_dir = ctx.campaign.build_work_dir();
    let lp = lean::lean_path(&ctx, &build_dir).unwrap();
    assert_eq!(lp, build_dir.display().to_string());
}

#[test]
fn lean_path_lake_failure_returns_code_2() {
    require_lean!();

    let tmp = TempDir::new().unwrap();
    let bad_host = tmp.path().join("bad_host");
    fs::create_dir_all(&bad_host).unwrap();
    fs::write(bad_host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
    // Invalid lakefile.toml so `lake env printenv LEAN_PATH` fails
    fs::write(bad_host.join("lakefile.toml"), "invalid === toml [\n").unwrap();

    let camp_dir = tmp.path().join("camp");
    let state_dir = tmp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    paths::init_campaign(&camp_dir, &bad_host, None, &[BackendKind::Claude]).unwrap();
    let ctx = Ctx::for_test(&camp_dir, &state_dir).unwrap();
    let err = lean::lean_path(&ctx, &ctx.campaign.build_work_dir()).unwrap_err();
    assert_eq!(err.code, 2);
}

#[test]
fn m1_04_incremental_build_cache_failure_and_sorry() {
    require_lean!();

    let tmp = TempDir::new().unwrap();
    let camp_dir = tmp.path().join("isort_camp");
    let state_dir = tmp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();

    let host = common::real_host_project();
    paths::init_campaign(&camp_dir, &host, Some("isort"), &[BackendKind::Claude]).unwrap();

    let ctx = Ctx::for_test(&camp_dir, &state_dir).unwrap();
    let tree = ctx.campaign.lean_dir();
    let build_dir = ctx.campaign.build_work_dir();

    // 1. Clean build of all modules in isort (Spec + Challenge)
    let rep1 = lean::build::build(&ctx, &tree, &build_dir, None).unwrap();
    assert!(rep1.ok);
    assert_eq!(rep1.results.len(), 5);
    for (m, res) in &rep1.results {
        assert!(res.ok, "{m} was not ok");
        assert!(!res.cached, "{m} should not be cached on first build");
    }
    // Challenge.lean has 4 sorry warnings with kind == "hasSorry" and ok == true
    let chal = &rep1.results["Apc.Challenge"];
    assert!(chal.ok);
    let sorry_count = chal
        .diagnostics
        .iter()
        .filter(|d| d.kind == "hasSorry")
        .count();
    assert_eq!(sorry_count, 4);

    // 2. Second build is 100% cached
    let rep2 = lean::build::build(&ctx, &tree, &build_dir, None).unwrap();
    assert!(rep2.ok);
    for (m, res) in &rep2.results {
        assert!(res.ok);
        assert!(res.cached, "{m} should be cached on second build");
    }

    // 3. Touching Defs.lean (changing bytes) rebuilds Defs and Challenge only
    let defs_path = tree.join("Apc/Spec/Defs.lean");
    let orig_defs = fs::read_to_string(&defs_path).unwrap();
    fs::write(&defs_path, format!("{orig_defs}\n-- touched comment\n")).unwrap();

    let rep3 = lean::build::build(&ctx, &tree, &build_dir, None).unwrap();
    assert!(rep3.ok);
    assert!(!rep3.results["Apc.Spec.Defs"].cached);
    assert!(!rep3.results["Apc.Challenge"].cached);
    assert!(rep3.results["Apc.Spec.Impl"].cached);
    assert!(rep3.results["Apc.Spec.Mutants"].cached);
    assert!(rep3.results["Apc.Spec.Tests"].cached);

    // 4. Syntax error in Defs.lean -> ok = false, located diagnostic, no olean,
    // and dependent Challenge.lean marked failed with "dependency Apc.Spec.Defs failed"
    fs::write(&defs_path, format!("{orig_defs}\ndef broken :=\n")).unwrap();
    let rep4 = lean::build::build(&ctx, &tree, &build_dir, None).unwrap();
    assert!(!rep4.ok);

    let defs_res = &rep4.results["Apc.Spec.Defs"];
    assert!(!defs_res.ok);
    assert!(defs_res.diagnostics.iter().any(|d| d.severity == "error" && d.line >= 1));
    assert!(!build_dir.join("Apc/Spec/Defs.olean").exists());
    assert!(!build_dir.join("Apc/Spec/Defs.olean.key").exists());

    let chal_res = &rep4.results["Apc.Challenge"];
    assert!(!chal_res.ok);
    assert_eq!(chal_res.diagnostics.len(), 1);
    assert_eq!(chal_res.diagnostics[0].text, "dependency Apc.Spec.Defs failed");
}

#[test]
fn m1_05_apc_check_three_final_lines_and_capping() {
    require_lean!();

    let tmp = TempDir::new().unwrap();
    let camp_dir = tmp.path().join("check_camp");
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

    // 1. File outside lean/ -> exit 2
    assert_cmd::Command::new(apc_bin)
        .args(["-C", camp_dir.to_str().unwrap(), "check", "campaign.toml"])
        .assert()
        .code(2);

    // 2. Clean file -> exit 0, last line "OK"
    let assert = assert_cmd::Command::new(apc_bin)
        .args([
            "-C",
            camp_dir.to_str().unwrap(),
            "check",
            "lean/Apc/Spec/Impl.lean",
        ])
        .assert()
        .code(0);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert_eq!(stdout.lines().last().unwrap(), "OK");

    // 3. Challenge.lean -> exit 0, last line "OK (4 sorry)"
    let assert = assert_cmd::Command::new(apc_bin)
        .args([
            "-C",
            camp_dir.to_str().unwrap(),
            "check",
            "lean/Apc/Challenge.lean",
        ])
        .assert()
        .code(0);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert_eq!(stdout.lines().last().unwrap(), "OK (4 sorry)");

    // 4. File with 100 errors -> exit 1, capped at 60 diagnostics + "… [40 more diagnostics]" + "FAILED (100 errors)"
    let mut bad_src = String::from("namespace Apc.Spec\n");
    for i in 0..100 {
        bad_src.push_str(&format!("def bad_{i} : Nat := \"not_a_nat_{i}\"\n"));
    }
    bad_src.push_str("end Apc.Spec\n");
    let bad_file = camp_dir.join("lean/Apc/Spec/ManyErrors.lean");
    fs::write(&bad_file, bad_src).unwrap();

    let assert = assert_cmd::Command::new(apc_bin)
        .args([
            "-C",
            camp_dir.to_str().unwrap(),
            "check",
            "lean/Apc/Spec/ManyErrors.lean",
        ])
        .assert()
        .code(1);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(*lines.last().unwrap(), "FAILED (100 errors)");
    assert!(
        lines.contains(&"… [40 more diagnostics]"),
        "expected cap notice in output:\n{stdout}"
    );
}

#[test]
fn m1_06_fingerprint_targets_closure_axioms_and_invariance() {
    require_lean!();

    let tmp = TempDir::new().unwrap();
    let camp_dir = tmp.path().join("fp_camp");
    let state_dir = tmp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let host = common::real_host_project();

    paths::init_campaign(&camp_dir, &host, Some("isort"), &[BackendKind::Claude]).unwrap();
    let ctx = Ctx::for_test(&camp_dir, &state_dir).unwrap();
    let tree = ctx.campaign.lean_dir();
    let build_dir = ctx.campaign.build_work_dir();

    // Build tree first
    let rep = lean::build::build(&ctx, &tree, &build_dir, None).unwrap();
    assert!(rep.ok);

    // 1. Four targets found in isort's Challenge
    let fp1 = vdash::fingerprint::targets(&ctx, &tree, &build_dir).unwrap();
    assert_eq!(fp1.len(), 4);
    assert!(fp1.contains_key("Apc.isort_correct"));
    assert!(fp1.contains_key("Apc.isort_fixes_sorted"));
    assert!(fp1.contains_key("Apc.sorted_nonvacuous"));
    assert!(fp1.contains_key("Apc.isortBad_incorrect"));

    // 2. Closure of Apc.isort_correct contains Apc.Spec.{Correct,Sorted,isort,insert} and List.Perm as ext
    let ic = &fp1["Apc.isort_correct"];
    for internal in [
        "Apc.Spec.Correct",
        "Apc.Spec.Sorted",
        "Apc.Spec.isort",
        "Apc.Spec.insert",
    ] {
        let entry = ic
            .closure
            .get(internal)
            .unwrap_or_else(|| panic!("missing {internal} in closure"));
        assert!(!entry.ext, "{internal} should not be ext");
        assert_eq!(entry.ser.as_ref().unwrap().len(), 64);
    }
    let perm_entry = ic
        .closure
        .get("List.Perm")
        .expect("missing List.Perm in closure");
    assert!(perm_entry.ext);
    assert!(perm_entry.ser.is_none());

    // 3. sorry proof reports sorryAx among axioms
    assert!(ic.axioms.contains(&"sorryAx".to_string()));
    let ax_info = vdash::fingerprint::axioms(
        &ctx,
        &tree,
        &build_dir,
        "Apc.Challenge",
        &["Apc.isort_correct".to_string(), "Apc.nonexistent".to_string()],
    )
    .unwrap();
    assert!(ax_info["Apc.isort_correct"].exists);
    assert_eq!(ax_info["Apc.isort_correct"].kind, "theorem");
    assert!(ax_info["Apc.isort_correct"]
        .axioms
        .contains(&"sorryAx".to_string()));
    assert!(ax_info["Apc.isort_correct"].statement.is_some());
    assert!(!ax_info["Apc.nonexistent"].exists);
    assert!(ax_info["Apc.nonexistent"].statement.is_none());

    // 4. Two runs give byte-identical JSON
    let fp2 = vdash::fingerprint::targets(&ctx, &tree, &build_dir).unwrap();
    assert_eq!(
        serde_json::to_string(&fp1).unwrap(),
        serde_json::to_string(&fp2).unwrap()
    );

    // 5. Changing only a comment in Defs.lean leaves every hash unchanged
    let defs_path = tree.join("Apc/Spec/Defs.lean");
    let orig_defs = fs::read_to_string(&defs_path).unwrap();
    fs::write(&defs_path, format!("-- header comment\n{orig_defs}\n-- footer\n")).unwrap();
    lean::build::build(&ctx, &tree, &build_dir, None).unwrap();
    let fp_comment = vdash::fingerprint::targets(&ctx, &tree, &build_dir).unwrap();
    assert_eq!(fp1, fp_comment);

    // 6. Renaming a bound variable in Sorted / Correct leaves every hash unchanged
    let renamed_defs = orig_defs.replace(
        "def Correct (f : List Nat → List Nat) : Prop :=\n  ∀ l, Sorted (f l) ∧ (f l).Perm l",
        "def Correct (fn_renamed : List Nat → List Nat) : Prop :=\n  ∀ xs_renamed, Sorted (fn_renamed xs_renamed) ∧ (fn_renamed xs_renamed).Perm xs_renamed",
    );
    assert_ne!(orig_defs, renamed_defs);
    fs::write(&defs_path, &renamed_defs).unwrap();
    lean::build::build(&ctx, &tree, &build_dir, None).unwrap();
    let fp_renamed = vdash::fingerprint::targets(&ctx, &tree, &build_dir).unwrap();
    assert_eq!(fp1, fp_renamed);

    // 7. Changing `≤` to `<` in Sorted changes the hash of Apc.Spec.Sorted and nothing unrelated (like isort/insert)
    let lt_defs = orig_defs.replace("x ≤ y", "x < y");
    assert_ne!(orig_defs, lt_defs);
    fs::write(&defs_path, &lt_defs).unwrap();
    lean::build::build(&ctx, &tree, &build_dir, None).unwrap();
    let fp_lt = vdash::fingerprint::targets(&ctx, &tree, &build_dir).unwrap();
    assert_ne!(
        fp1["Apc.isort_correct"].closure["Apc.Spec.Sorted"].ser,
        fp_lt["Apc.isort_correct"].closure["Apc.Spec.Sorted"].ser
    );
    assert_eq!(
        fp1["Apc.isort_correct"].closure["Apc.Spec.isort"].ser,
        fp_lt["Apc.isort_correct"].closure["Apc.Spec.isort"].ser
    );
    assert_eq!(
        fp1["Apc.isort_correct"].closure["Apc.Spec.insert"].ser,
        fp_lt["Apc.isort_correct"].closure["Apc.Spec.insert"].ser
    );
    fs::write(&defs_path, &orig_defs).unwrap();

    // 8. Universe-polymorphic theorem vs restated at fixed Type get different `type` hashes
    let poly_path = tree.join("Apc/Spec/Poly.lean");
    fs::write(
        &poly_path,
        "namespace Apc.Spec\ntheorem poly_id.{u} (α : Type u) (x : α) : x = x := rfl\ntheorem mono_id (α : Type) (x : α) : x = x := rfl\nend Apc.Spec\n",
    )
    .unwrap();
    lean::build::build(&ctx, &tree, &build_dir, Some(&["Apc.Spec.Poly".to_string()])).unwrap();
    let fp_poly = vdash::fingerprint::closure(
        &ctx,
        &tree,
        &build_dir,
        "Apc.Spec.Poly",
        &["Apc.Spec.poly_id".to_string(), "Apc.Spec.mono_id".to_string()],
    )
    .unwrap();
    assert_ne!(
        fp_poly["Apc.Spec.poly_id"].ty,
        fp_poly["Apc.Spec.mono_id"].ty
    );
}

#[test]
fn m1_07_apc_axioms_and_tcb() {
    require_lean!();

    let tmp = TempDir::new().unwrap();
    let camp_dir = tmp.path().join("ax_camp");
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

    // Write a proved lemma into Proof/Work/Lemma.lean
    fs::write(
        camp_dir.join("lean/Apc/Proof/Work/Lemma.lean"),
        "import Apc.Spec.Defs\nnamespace Apc\ntheorem proved_lem : Apc.Spec.Sorted [1, 2] := ⟨by decide, trivial⟩\nend Apc\n",
    )
    .unwrap();

    // 1. Proved lemma -> exit 0
    let assert = assert_cmd::Command::new(apc_bin)
        .args([
            "-C",
            camp_dir.to_str().unwrap(),
            "axioms",
            "lean/Apc/Proof/Work/Lemma.lean",
            "Apc.proved_lem",
        ])
        .assert()
        .code(0);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(stdout.contains("Apc.proved_lem: theorem, axioms:"));

    // 2. Lemma using sorry -> exit 1 and names sorryAx
    let assert = assert_cmd::Command::new(apc_bin)
        .args([
            "-C",
            camp_dir.to_str().unwrap(),
            "axioms",
            "lean/Apc/Challenge.lean",
            "Apc.isort_correct",
        ])
        .assert()
        .code(1);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("sorryAx"),
        "expected sorryAx in output: {stdout}"
    );

    // 3. Unknown name -> exit 1 MISSING
    let assert = assert_cmd::Command::new(apc_bin)
        .args([
            "-C",
            camp_dir.to_str().unwrap(),
            "axioms",
            "lean/Apc/Challenge.lean",
            "Apc.does_not_exist",
        ])
        .assert()
        .code(1);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("Apc.does_not_exist: MISSING"),
        "expected MISSING in output: {stdout}"
    );

    // 4. Non-theorem (def) -> exit 1 and "(not a theorem)"
    let assert = assert_cmd::Command::new(apc_bin)
        .args([
            "-C",
            camp_dir.to_str().unwrap(),
            "axioms",
            "lean/Apc/Spec/Impl.lean",
            "Apc.Spec.isort",
        ])
        .assert()
        .code(1);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("Apc.Spec.isort: def (not a theorem)"),
        "expected def (not a theorem) in output: {stdout}"
    );

    // 5. vdash tcb -> exit 0 and lists targets, non-external modules, and external namespaces
    let assert = assert_cmd::Command::new(apc_bin)
        .args(["-C", camp_dir.to_str().unwrap(), "tcb"])
        .assert()
        .code(0);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(stdout.contains("Apc.isort_correct"));
    assert!(stdout.contains("Apc.Spec.Defs"));
    assert!(stdout.contains("List"));
}

#[test]
fn m1_09_apc_approve_happy_path_sorry_in_spec_and_double_approve() {
    require_lean!();

    let tmp = TempDir::new().unwrap();
    let host = common::real_host_project();
    let apc_bin = assert_cmd::cargo::cargo_bin!("vdash");

    // Case A: sorry inside Spec/Defs.lean is refused with code 1 and nothing changed
    let bad_camp = tmp.path().join("bad_camp");
    let bad_state = tmp.path().join("bad_state");
    fs::create_dir_all(&bad_state).unwrap();
    assert_cmd::Command::new(apc_bin)
        .args([
            "init",
            bad_camp.to_str().unwrap(),
            "--host",
            host.to_str().unwrap(),
            "--example",
            "isort",
        ])
        .assert()
        .success();

    let defs_file = bad_camp.join("lean/Apc/Spec/Defs.lean");
    let orig_defs = fs::read_to_string(&defs_file).unwrap();
    fs::write(
        &defs_file,
        format!("{orig_defs}\ntheorem bad_spec_thm : True := by sorry\n"),
    )
    .unwrap();

    assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &bad_state)
        .args(["-C", bad_camp.to_str().unwrap(), "approve"])
        .assert()
        .code(1);
    assert!(!bad_camp.join("lean/Apc/Solution.lean").exists());
    let st_bad = vdash::util::load_state(&vdash::paths::Campaign::new(&bad_camp)).unwrap();
    assert_eq!(st_bad.phase, "init");

    // Case B: Happy path leaves a commit, .heldout/, an anchor, Solution.lean, and 4 targets
    let camp_dir = tmp.path().join("ok_camp");
    let xdg_state = tmp.path().join("xdg_state");
    fs::create_dir_all(&xdg_state).unwrap();
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
        .code(0);

    let camp = vdash::paths::Campaign::new(&camp_dir);
    assert!(camp.solution_file().is_file());
    for rel in camp.trusted_files() {
        let held = camp.heldout_dir().join(&rel);
        assert!(held.is_file(), "missing heldout file {}", held.display());
        let perm = fs::metadata(&held).unwrap().permissions();
        assert!(perm.readonly(), "heldout file {} is not read-only", held.display());
    }

    let cfg = vdash::config::load(&camp_dir).unwrap();
    let state_dir = xdg_state.join("vdash").join(&cfg.id);
    let ctx = Ctx {
        campaign: camp.clone(),
        cfg,
        state_dir,
        exe: std::path::PathBuf::from(apc_bin),
    };
    assert!(vdash::anchor::verify(&ctx).is_empty());

    let head = vdash::util::git(&camp, &["rev-parse", "HEAD"]).unwrap();
    assert!(!head.trim().is_empty());

    let st = vdash::util::load_state(&camp).unwrap();
    assert_eq!(st.phase, "approved");
    assert_eq!(st.targets.len(), 4);
    for t in st.targets.values() {
        assert!(!t.accepted);
        assert_eq!(t.reasons, vec!["missing".to_string()]);
    }

    // Case C: Approving twice is refused with code 2
    assert_cmd::Command::new(apc_bin)
        .env("XDG_STATE_HOME", &xdg_state)
        .args(["-C", camp_dir.to_str().unwrap(), "approve"])
        .assert()
        .code(2);
}
