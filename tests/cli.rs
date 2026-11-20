mod common;

use assert_cmd::Command;
use std::fs;
use tempfile::TempDir;

fn apc_cmd() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("vdash"))
}

fn make_host(tmp: &TempDir) -> std::path::PathBuf {
    let host = tmp.path().join("host");
    fs::create_dir_all(&host).unwrap();
    fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
    host
}

#[test]
fn version_prints_apc_0_1_0() {
    let assert = apc_cmd().arg("--version").assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert_eq!(stdout.trim(), "vdash 0.1.0");
}

#[test]
fn gate_outside_campaign_exits_2() {
    let tmp = TempDir::new().unwrap();
    let assert = apc_cmd()
        .current_dir(tmp.path())
        .env_remove("VDASH_CAMPAIGN")
        .arg("gate")
        .assert()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.starts_with("vdash: error: "),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn invalid_campaign_toml_exits_2_and_amend_list_works() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("campaign.toml"), "").unwrap();
    let assert = apc_cmd()
        .current_dir(tmp.path())
        .env_remove("VDASH_CAMPAIGN")
        .args(["amend", "list"])
        .assert()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.starts_with("vdash: error: "),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn init_creates_campaign_and_refuses_non_empty_or_bad_example() {
    let tmp = TempDir::new().unwrap();
    let host = make_host(&tmp);
    let camp_dir = tmp.path().join("my_campaign");

    // 1. Unknown example fails with code 2
    let assert = apc_cmd()
        .args([
            "init",
            camp_dir.to_str().unwrap(),
            "--host",
            host.to_str().unwrap(),
            "--example",
            "nope",
        ])
        .assert()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("unknown example"),
        "unexpected stderr: {stderr}"
    );

    // 2. Valid init succeeds
    apc_cmd()
        .args([
            "init",
            camp_dir.to_str().unwrap(),
            "--host",
            host.to_str().unwrap(),
        ])
        .assert()
        .success();

    for sub in [
        "sources",
        "skills",
        "spec",
        "lean/Apc/Spec",
        "lean/Apc/Proof/Lib",
        "lean/Apc/Proof/Work",
        ".heldout",
        ".build/work",
        ".build/gate",
        "leads",
        "amendments",
        "escalations",
        "logs",
        ".git",
    ] {
        assert!(
            camp_dir.join(sub).is_dir(),
            "missing directory {sub} in {}",
            camp_dir.display()
        );
    }

    let cfg = vdash::config::load(&camp_dir).unwrap();
    assert_eq!(cfg.id.len(), 32);
    assert!(cfg.id.chars().all(|c| c.is_ascii_hexdigit()));

    let state: serde_json::Value = vdash::jsonio::read_json(camp_dir.join("state.json")).unwrap();
    assert_eq!(state["version"], 1);
    assert_eq!(state["phase"], "init");
    assert_eq!(state["round"], 0);
    assert_eq!(state["stage"], "idle");

    let gitignore = fs::read_to_string(camp_dir.join(".gitignore")).unwrap();
    assert!(gitignore.contains(".build/"));
    assert!(gitignore.contains("logs/"));

    // 3. Second run on non-empty directory fails with code 2
    apc_cmd()
        .args([
            "init",
            camp_dir.to_str().unwrap(),
            "--host",
            host.to_str().unwrap(),
        ])
        .assert()
        .code(2);
}

#[test]
fn doctor_fails_with_code_1_on_nonexistent_backend_command() {
    let tmp = TempDir::new().unwrap();
    let host = make_host(&tmp);
    let camp_dir = tmp.path().join("doc_camp");

    apc_cmd()
        .args([
            "init",
            camp_dir.to_str().unwrap(),
            "--host",
            host.to_str().unwrap(),
        ])
        .assert()
        .success();

    fs::write(
        camp_dir.join("campaign.toml"),
        format!(
            r#"id = "0123456789abcdef0123456789abcdef"
host_project = "{}"
use_mathlib = false
[policy]
kernel_recheck = false
[backends.claude]
command = ["nonexistent_claude_binary_xyz_999"]
[roles]
spec_writer = "claude"
architect = "claude"
orchestrator = "claude"
team_lead = "claude"
integrator = "claude"
critic = "claude"
monitor = "claude"
reviewer = "claude"
falsifier = "claude"
worker = "claude"
"#,
            host.display()
        ),
    )
    .unwrap();

    let assert = apc_cmd()
        .args(["-C", camp_dir.to_str().unwrap(), "doctor"])
        .assert()
        .code(1);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.lines().any(|l| l.contains("claude") && l.contains("FAIL")),
        "expected FAIL line for claude in doctor output:\n{stdout}"
    );
}

#[test]
fn init_example_isort_writes_five_lean_files_and_disables_mathlib() {
    let tmp = TempDir::new().unwrap();
    let host = make_host(&tmp);
    let camp_dir = tmp.path().join("isort_camp");

    apc_cmd()
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

    for rel in [
        "lean/Apc/Spec/Impl.lean",
        "lean/Apc/Spec/Defs.lean",
        "lean/Apc/Spec/Mutants.lean",
        "lean/Apc/Spec/Tests.lean",
        "lean/Apc/Challenge.lean",
    ] {
        assert!(
            camp_dir.join(rel).is_file(),
            "missing example file {rel}"
        );
    }
    assert!(!camp_dir.join("lean/Apc/Solution.lean").exists());

    let cfg = vdash::config::load(&camp_dir).unwrap();
    assert!(!cfg.use_mathlib);
}

#[test]
fn search_model_login_and_help_banner_work() {
    let tmp = TempDir::new().unwrap();
    let host = make_host(&tmp);
    let camp_dir = tmp.path().join("isort_search_camp");

    let help_assert = apc_cmd().arg("--help").assert().success();
    let help_out = String::from_utf8_lossy(&help_assert.get_output().stdout);
    assert!(help_out.contains("vdash@lean4"), "expected neofetch-style Vdash banner in --help:\n{help_out}");

    let login_assert = apc_cmd().current_dir(tmp.path()).arg("login").assert().success();
    let login_out = String::from_utf8_lossy(&login_assert.get_output().stdout);
    assert!(login_out.contains("Account status:"), "unexpected login status:\n{login_out}");

    apc_cmd()
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

    let search_assert = apc_cmd()
        .args(["-C", camp_dir.to_str().unwrap(), "search", "--offline", "isort_correct"])
        .assert()
        .success();
    let search_out = String::from_utf8_lossy(&search_assert.get_output().stdout);
    assert!(search_out.contains("isort_correct"), "expected isort_correct in search output:\n{search_out}");

    apc_cmd()
        .args(["-C", camp_dir.to_str().unwrap(), "model", "worker", "gemini", "gemini-4-argon-high"])
        .assert()
        .success();
    let cfg = vdash::config::load(&camp_dir).unwrap();
    assert_eq!(cfg.entries_for(vdash::config::Role::Worker)[0].to_string(), "gemini:gemini-4-argon-high");
}

#[test]
fn isort_example_and_reference_solution_compile_with_lean() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let tree = tmp.path().join("lean");
    let build = tmp.path().join("build");
    fs::create_dir_all(tree.join("Apc/Spec")).unwrap();
    fs::create_dir_all(build.join("Apc/Spec")).unwrap();

    for (rel, content) in [
        ("Apc/Spec/Impl.lean", vdash::paths::ISORT_SPEC_IMPL),
        ("Apc/Spec/Defs.lean", vdash::paths::ISORT_SPEC_DEFS),
        ("Apc/Spec/Mutants.lean", vdash::paths::ISORT_SPEC_MUTANTS),
        ("Apc/Spec/Tests.lean", vdash::paths::ISORT_SPEC_TESTS),
        ("Apc/Challenge.lean", vdash::paths::ISORT_CHALLENGE),
        ("Apc/Solution.lean", vdash::paths::ISORT_REFERENCE_SOLUTION),
    ] {
        fs::write(tree.join(rel), content).unwrap();
    }

    let compile = |rel_stem: &str| -> (i32, String) {
        let src = tree.join(format!("{rel_stem}.lean"));
        let olean = build.join(format!("{rel_stem}.olean"));
        let ilean = build.join(format!("{rel_stem}.ilean"));
        let out = std::process::Command::new("lean")
            .env("ELAN_TOOLCHAIN", "leanprover/lean4:v4.33.0")
            .env("LEAN_PATH", &build)
            .arg("-R")
            .arg(&tree)
            .arg("-o")
            .arg(&olean)
            .arg("-i")
            .arg(&ilean)
            .arg("--json")
            .arg(&src)
            .output()
            .expect("failed to spawn lean (is elan on PATH?)");
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            stderr.trim().is_empty(),
            "unexpected stderr for {rel_stem}: {stderr}"
        );
        (code, stdout)
    };

    for clean_mod in [
        "Apc/Spec/Impl",
        "Apc/Spec/Defs",
        "Apc/Spec/Mutants",
        "Apc/Spec/Tests",
        "Apc/Solution",
    ] {
        let (code, stdout) = compile(clean_mod);
        assert_eq!(code, 0, "{clean_mod} failed to compile:\n{stdout}");
        assert!(
            stdout.trim().is_empty(),
            "{clean_mod} produced warnings/messages:\n{stdout}"
        );
    }

    let (code, stdout) = compile("Apc/Challenge");
    assert_eq!(code, 0, "Apc/Challenge failed to compile:\n{stdout}");
    let sorry_warnings = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["severity"] == "warning"
                && (v["kind"] == "hasSorry"
                    || v["data"].as_str().unwrap_or("").contains("sorry"))
        })
        .count();
    assert_eq!(
        sorry_warnings, 4,
        "expected 4 sorry warnings in Challenge.lean, got {sorry_warnings}:\n{stdout}"
    );
}

#[test]
fn restart_reopens_done_campaign_and_preserves_solution() {
    require_lean!();
    let tmp = TempDir::new().unwrap();
    let host = make_host(&tmp);
    let camp_dir = tmp.path().join("isort_restart");

    apc_cmd()
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

    // Cannot restart before approval (phase = init)
    apc_cmd()
        .args(["-C", camp_dir.to_str().unwrap(), "restart"])
        .assert()
        .failure();

    apc_cmd()
        .args(["-C", camp_dir.to_str().unwrap(), "approve"])
        .assert()
        .success();

    // Install reference solution, run gate, and mark campaign done
    let sol_path = camp_dir.join("lean/Apc/Solution.lean");
    fs::write(&sol_path, vdash::paths::ISORT_REFERENCE_SOLUTION).unwrap();
    apc_cmd()
        .args(["-C", camp_dir.to_str().unwrap(), "gate"])
        .assert()
        .success();

    let camp = vdash::paths::Campaign::new(&camp_dir);
    vdash::util::update_state(&camp, |st| {
        st.phase = "done".into();
        st.round = 1;
    })
    .unwrap();

    // Restart reopens spec phase, clears heldout, preserves Solution.lean
    let out = apc_cmd()
        .args(["-C", camp_dir.to_str().unwrap(), "restart"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(stdout.contains("restarted: campaign reopened in phase 'spec'"));

    let st = vdash::util::load_state(&camp).unwrap();
    assert_eq!(st.phase, "spec");
    assert_eq!(st.stage, "idle");
    assert!(st.targets.is_empty());
    assert_eq!(st.round_data["next_revise_plan"], serde_json::json!(true));
    assert!(!camp.heldout_dir().exists());
    assert_eq!(
        fs::read_to_string(&sol_path).unwrap(),
        vdash::paths::ISORT_REFERENCE_SOLUTION
    );

    // Add a new target theorem to Challenge.lean and re-approve
    let chal_path = camp.challenge_file();
    let mut chal = fs::read_to_string(&chal_path).unwrap();
    chal.push_str("\ntheorem isort_extra_true : True := by sorry\n");
    fs::write(&chal_path, chal).unwrap();

    apc_cmd()
        .args(["-C", camp_dir.to_str().unwrap(), "approve"])
        .assert()
        .success();

    let st_after = vdash::util::load_state(&camp).unwrap();
    assert_eq!(st_after.phase, "approved");
    assert_eq!(st_after.targets.len(), 5);
    assert!(
        st_after.targets.contains_key("isort_extra_true")
            || st_after.targets.contains_key("Apc.isort_extra_true")
    );
}


#[test]
fn model_refresh_reads_agy_models_and_rechecks_rejected_claude_models() {
    let tmp = TempDir::new().unwrap();
    let host = make_host(&tmp);
    let camp_dir = tmp.path().join("models_camp");
    apc_cmd().args(["init", camp_dir.to_str().unwrap(), "--host", host.to_str().unwrap()]).assert().success();
    // Stand-ins for `agy models` (fact A6) and a Claude CLI that rejects every model
    // as Fable is rejected on the Pro plan (fact C17).
    let agy = tmp.path().join("agy.sh");
    fs::write(
        &agy,
        "[ \"$1\" = models ] || exit 9\necho 'Fetching available models...'\nprintf 'gemini-4-neon-high\\tGemini 4 Neon (High)\\ngemini-4-hydrogen-high\\tGemini 4 Hydrogen (High)\\nclaude-opus-5-5-low\\tClaude Opus 5.5 (Low)\\n'\n",
    )
    .unwrap();
    let claude = tmp.path().join("claude.sh");
    fs::write(
        &claude,
        "cat >/dev/null\necho '{\"is_error\":true,\"result\":\"Fable 5.2 requires usage credits.\",\"api_error_status\":429,\"api_error_code\":\"credits_required\",\"api_error\":\"model_requires_usage_credits\"}'\n",
    )
    .unwrap();
    fs::write(
        camp_dir.join("campaign.toml"),
        format!(
            "id = \"0123456789abcdef0123456789abcdef\"\nhost_project = \"{}\"\nuse_mathlib = false\n[backends.claude]\ncommand = [\"sh\", \"{}\"]\n[backends.gemini]\ncommand = [\"sh\", \"{}\"]\n[roles]\nworker = [\"gemini:gemini-4-argon-high\", \"claude:fable\"]\n",
            host.display(),
            claude.display(),
            agy.display()
        ),
    )
    .unwrap();
    let cache_home = tmp.path().join("cache");
    let cache = cache_home.join("vdash/models.json");
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    fs::write(&cache, r#"{"rejected":{"claude:fable":{"reason":"old","at":9999999999}}}"#).unwrap();

    let run = |args: &[&str]| {
        let a = apc_cmd().env("XDG_CACHE_HOME", &cache_home).args(["-C", camp_dir.to_str().unwrap()]).args(args).assert().success();
        String::from_utf8_lossy(&a.get_output().stdout).to_string()
    };
    let out = run(&["model", "refresh"]);
    assert!(out.contains("gemini: 2 models from `agy models`"), "{out}");
    assert!(out.contains("claude:fable is still unavailable: Fable 5.2 requires usage credits."), "{out}");
    assert!(out.contains("gemini   gemini-4-neon-high, gemini-4-hydrogen-high\n"), "{out}");
    assert!(out.contains("warning: gemini:gemini-4-argon-high (worker) may not work: no longer listed by `agy models`; try `/model gemini gemini-4-neon-high`"), "{out}");
    assert!(out.contains("warning: claude:fable (worker)"), "{out}");
    let list = run(&["model"]);
    assert!(list.contains("unavailable: claude:fable (Fable 5.2 requires usage credits.)"), "{list}");
    assert!(!list.contains("claude   opus, sonnet, haiku, fable,"), "{list}");
    assert!(run(&["model", "refresh", "--quiet"]).is_empty());
}
