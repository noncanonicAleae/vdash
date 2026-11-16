pub mod scan;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::anchor;
use crate::error::{VdashError, Result};
use crate::fingerprint;
use crate::lean;
use crate::paths::{Ctx, TargetState};
use crate::proc;
use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    Missing,
    NotTheorem,
    StatementMismatch,
    ClosureMismatch,
    UntrustedDependency,
    BadAxioms,
    BuildFailed,
    ForbiddenSource,
    KernelRecheckFailed,
    ComparatorDisagrees,
    AnchorMismatch,
}

impl ReasonCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReasonCode::Missing => "missing",
            ReasonCode::NotTheorem => "not_theorem",
            ReasonCode::StatementMismatch => "statement_mismatch",
            ReasonCode::ClosureMismatch => "closure_mismatch",
            ReasonCode::UntrustedDependency => "untrusted_dependency",
            ReasonCode::BadAxioms => "bad_axioms",
            ReasonCode::BuildFailed => "build_failed",
            ReasonCode::ForbiddenSource => "forbidden_source",
            ReasonCode::KernelRecheckFailed => "kernel_recheck_failed",
            ReasonCode::ComparatorDisagrees => "comparator_disagrees",
            ReasonCode::AnchorMismatch => "anchor_mismatch",
        }
    }
}

impl std::fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetVerdict {
    pub accepted: bool,
    pub reasons: Vec<ReasonCode>,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub version: u32,
    pub ok: bool,
    pub anchor_ok: bool,
    pub global_reasons: Vec<String>,
    pub targets: BTreeMap<String, TargetVerdict>,
}

fn reject_all(
    target_names: impl IntoIterator<Item = String>,
    reason: ReasonCode,
    detail: serde_json::Value,
    anchor_ok: bool,
    global_reasons: Vec<String>,
) -> Verdict {
    let mut targets = BTreeMap::new();
    for t in target_names {
        targets.insert(
            t,
            TargetVerdict {
                accepted: false,
                reasons: vec![reason],
                detail: detail.clone(),
            },
        );
    }
    Verdict {
        version: 1,
        ok: false,
        anchor_ok,
        global_reasons,
        targets,
    }
}

pub fn run(ctx: &Ctx) -> Result<Verdict> {
    // Step 0: anchor::verify
    let anchor_problems = anchor::verify(ctx);
    if !anchor_problems.is_empty() {
        let state_targets: Vec<String> = util::load_state(&ctx.campaign)
            .map(|s| s.targets.keys().cloned().collect())
            .unwrap_or_default();
        return Ok(reject_all(
            state_targets,
            ReasonCode::AnchorMismatch,
            serde_json::json!({ "problems": anchor_problems }),
            false,
            anchor_problems,
        ));
    }

    // Step 1: Create two fresh temp dirs inside .build/gate
    let gate_base = ctx.campaign.build_gate_dir();
    fs::create_dir_all(&gate_base).map_err(|e| VdashError::io_path(&gate_base, e))?;
    let tmp = TempDir::new_in(&gate_base).map_err(|e| VdashError::io_path(&gate_base, e))?;

    let chal_tree = tmp.path().join("chal_tree");
    let chal_build = tmp.path().join("chal_build");
    let sol_tree = tmp.path().join("sol_tree");
    let sol_build = tmp.path().join("sol_build");
    fs::create_dir_all(&chal_build).map_err(|e| VdashError::io_path(&chal_build, e))?;
    fs::create_dir_all(&sol_build).map_err(|e| VdashError::io_path(&sol_build, e))?;

    util::copy_tree(&ctx.campaign.heldout_tree(), &chal_tree, &|_| false)?;
    util::copy_tree(&ctx.campaign.lean_dir(), &sol_tree, &|rel| {
        rel == Path::new("Apc/Challenge.lean")
    })?;

    // Step 2a: Build Challenge tree from scratch (internal fault -> code 5)
    let chal_rep = lean::build::build(ctx, &chal_tree, &chal_build, None)
        .map_err(|e| VdashError::new(5, format!("challenge build failed: {}", e.message)))?;
    if !chal_rep.ok {
        return Err(VdashError::new(
            5,
            "held-out challenge tree failed to compile".to_string(),
        ));
    }
    let chal_fps = fingerprint::targets(ctx, &chal_tree, &chal_build)
        .map_err(|e| VdashError::new(5, format!("challenge fingerprint failed: {}", e.message)))?;
    let target_names: Vec<String> = chal_fps.keys().cloned().collect();

    // Step 2b: Build Solution (Apc.Solution and its import closure)
    if !sol_tree.join("Apc/Solution.lean").is_file() {
        return Ok(reject_all(
            target_names,
            ReasonCode::BuildFailed,
            serde_json::json!({ "error": "missing lean/Apc/Solution.lean" }),
            true,
            vec!["build_failed".to_string()],
        ));
    }

    let sol_target = ["Apc.Solution".to_string()];
    let sol_rep = match lean::build::build(ctx, &sol_tree, &sol_build, Some(&sol_target)) {
        Ok(r) => r,
        Err(e) if e.code == 1 => {
            return Ok(reject_all(
                target_names,
                ReasonCode::BuildFailed,
                serde_json::json!({ "error": e.message }),
                true,
                vec!["build_failed".to_string()],
            ));
        }
        Err(e) => return Err(e),
    };

    if !sol_rep.ok {
        let mut errs = Vec::new();
        for r in sol_rep.results.values() {
            for d in &r.diagnostics {
                if d.severity == "error" {
                    errs.push(format!("{}:{}:{}: {}", d.file, d.line, d.col, d.text));
                }
            }
        }
        return Ok(reject_all(
            target_names,
            ReasonCode::BuildFailed,
            serde_json::json!({ "errors": errs }),
            true,
            vec!["build_failed".to_string()],
        ));
    }

    // Step 3: scan hits on untrusted modules in the solution's import closure.
    // A module in the solution closure counts as trusted ONLY if it starts with
    // `Apc.Spec.` AND a file with the same relative path exists in `.heldout/lean/`
    // with identical bytes.
    // Only the solution's own closure matters; a broken import elsewhere must not fail the gate.
    let sol_graph = lean::imports::scan_imports(&sol_tree)?.graph;
    let sol_closure = lean::imports::closure_of(&sol_graph, &sol_target);
    let untrusted_mods: Vec<String> = sol_closure
        .into_iter()
        .filter(|m| {
            if !m.starts_with("Apc.Spec.") {
                return true;
            }
            let mut rel = PathBuf::new();
            for part in m.split('.') {
                rel.push(part);
            }
            rel.set_extension("lean");
            let held_file = chal_tree.join(&rel);
            let sol_file = sol_tree.join(&rel);
            match (fs::read(&held_file), fs::read(&sol_file)) {
                (Ok(hb), Ok(sb)) => hb != sb,
                _ => true,
            }
        })
        .collect();

    let hits = scan::scan_files(&sol_tree, &untrusted_mods)?;
    if !hits.is_empty() {
        return Ok(reject_all(
            target_names,
            ReasonCode::ForbiddenSource,
            serde_json::json!({ "hits": hits }),
            true,
            vec!["forbidden_source".to_string()],
        ));
    }

    // Step 4: kernel_recheck via leanchecker on each untrusted module of the closure
    if ctx.cfg.policy.kernel_recheck {
        let sol_lp = lean::lean_path(ctx, &sol_build)?;
        for m in &untrusted_mods {
            let _slot = lean::slots::acquire(ctx)?;
            let mut lc_cmd = lean::command(ctx, "leanchecker")?;
            lc_cmd.env("LEAN_PATH", &sol_lp).arg(m);
            let out = proc::run(
                &mut lc_cmd,
                None,
                Duration::from_secs(ctx.cfg.budget.lean_timeout_s),
            )?;
            if out.timed_out || out.status != Some(0) {
                return Ok(reject_all(
                    target_names,
                    ReasonCode::KernelRecheckFailed,
                    serde_json::json!({
                        "module": m,
                        "timed_out": out.timed_out,
                        "stderr": out.stderr_lossy().trim()
                    }),
                    true,
                    vec!["kernel_recheck_failed".to_string()],
                ));
            }
        }
    }

    // Step 5: Fingerprint solution for the challenge target names (root Apc.Solution)
    let sol_fps = fingerprint::closure(ctx, &sol_tree, &sol_build, "Apc.Solution", &target_names)?;

    // Step 7 check (optional comparator_cmd)
    let comparator_err = run_comparator(
        ctx,
        &chal_build,
        &sol_build,
        &chal_tree,
        &sol_tree,
        &target_names,
    );

    // Step 6: Per-target comparison
    let mut targets_out = BTreeMap::new();
    for (tname, c_tfp) in &chal_fps {
        let mut reasons = Vec::new();
        let mut detail = serde_json::Map::new();

        match sol_fps.get(tname) {
            None => {
                reasons.push(ReasonCode::Missing);
            }
            Some(s_tfp) => {
                if s_tfp.kind != "theorem" {
                    reasons.push(ReasonCode::NotTheorem);
                    detail.insert(
                        "kind".to_string(),
                        serde_json::Value::String(s_tfp.kind.clone()),
                    );
                }

                if s_tfp.ty != c_tfp.ty {
                    reasons.push(ReasonCode::StatementMismatch);
                }

                let mut changed = Vec::new();
                let mut added = Vec::new();
                let mut removed = Vec::new();

                for (k, c_ce) in &c_tfp.closure {
                    match s_tfp.closure.get(k) {
                        Some(s_ce) => {
                            if c_ce != s_ce && changed.len() < 10 {
                                changed.push(k.clone());
                            }
                        }
                        None => {
                            if removed.len() < 10 {
                                removed.push(k.clone());
                            }
                        }
                    }
                }
                for k in s_tfp.closure.keys() {
                    if !c_tfp.closure.contains_key(k) && added.len() < 10 {
                        added.push(k.clone());
                    }
                }

                if !changed.is_empty() || !added.is_empty() || !removed.is_empty() {
                    reasons.push(ReasonCode::ClosureMismatch);
                    detail.insert("changed".to_string(), serde_json::json!(changed));
                    detail.insert("added".to_string(), serde_json::json!(added));
                    detail.insert("removed".to_string(), serde_json::json!(removed));
                }

                let mut untrusted = Vec::new();
                for (cname, ce) in &s_tfp.closure {
                    if !ce.ext && !ce.module.starts_with("Apc.Spec") && untrusted.len() < 10 {
                        untrusted.push(cname.clone());
                    }
                }
                if !untrusted.is_empty() {
                    reasons.push(ReasonCode::UntrustedDependency);
                    detail.insert("untrusted".to_string(), serde_json::json!(untrusted));
                }

                let bad_axioms: Vec<String> = s_tfp
                    .axioms
                    .iter()
                    .filter(|a| !fingerprint::is_standard_axiom(a))
                    .cloned()
                    .collect();
                if !bad_axioms.is_empty() {
                    reasons.push(ReasonCode::BadAxioms);
                    detail.insert("bad_axioms".to_string(), serde_json::json!(bad_axioms));
                }
            }
        }

        if let Some(ref err) = comparator_err {
            reasons.push(ReasonCode::ComparatorDisagrees);
            detail.insert("comparator".to_string(), serde_json::json!(err));
        }

        let accepted = reasons.is_empty();
        targets_out.insert(
            tname.clone(),
            TargetVerdict {
                accepted,
                reasons,
                detail: serde_json::Value::Object(detail),
            },
        );
    }

    let ok = targets_out.values().all(|tv| tv.accepted);
    Ok(Verdict {
        version: 1,
        ok,
        anchor_ok: true,
        global_reasons: Vec::new(),
        targets: targets_out,
    })
}

/// Run an optional external `policy.comparator_cmd` against the built challenge
/// and solution trees.
///
/// Positional arguments: `<chal_build> <sol_build>`.
/// Environment variables passed to the comparator process:
/// - `VDASH_CHALLENGE_BUILD`: built challenge `.olean` directory
/// - `VDASH_SOLUTION_BUILD`: built solution `.olean` directory
/// - `VDASH_CHALLENGE_TREE`: challenge source tree (`.heldout/lean/`)
/// - `VDASH_SOLUTION_TREE`: solution source tree (`lean/`)
/// - `VDASH_TARGETS`: comma-separated target Lean declaration names
/// - `ELAN_TOOLCHAIN`: configured Lean toolchain (when resolvable)
/// - `VDASH_HOST_LEAN_PATH`: host package/Mathlib `LEAN_PATH` entries when
///   `use_mathlib = true` (without the solution build dir prefix)
///
/// Returns `Some(diagnostic)` if the command fails, times out, or cannot spawn;
/// `None` if no comparator is configured or it exits 0.
fn run_comparator(
    ctx: &Ctx,
    chal_build: &Path,
    sol_build: &Path,
    chal_tree: &Path,
    sol_tree: &Path,
    target_names: &[String],
) -> Option<String> {
    let cmd_str = ctx.cfg.policy.comparator_cmd.trim();
    if cmd_str.is_empty() {
        return None;
    }
    let parts: Vec<&str> = cmd_str.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    let mut ccmd = Command::new(parts[0]);
    ccmd.args(&parts[1..])
        .arg(chal_build)
        .arg(sol_build)
        .env("VDASH_CHALLENGE_BUILD", chal_build)
        .env("VDASH_SOLUTION_BUILD", sol_build)
        .env("VDASH_CHALLENGE_TREE", chal_tree)
        .env("VDASH_SOLUTION_TREE", sol_tree)
        .env("VDASH_TARGETS", target_names.join(","))
        .current_dir(&ctx.campaign.root);

    if let Ok(tc) = lean::toolchain(&ctx.cfg) {
        ccmd.env("ELAN_TOOLCHAIN", tc);
    }
    if ctx.cfg.use_mathlib {
        if let Ok(lp) = lean::lean_path(ctx, sol_build) {
            let prefix = format!("{}:", sol_build.display());
            if let Some(host_lp) = lp.strip_prefix(&prefix) {
                if !host_lp.is_empty() {
                    ccmd.env("VDASH_HOST_LEAN_PATH", host_lp);
                }
            }
        }
    }

    match proc::run(
        &mut ccmd,
        None,
        Duration::from_secs(ctx.cfg.budget.lean_timeout_s),
    ) {
        Ok(out) if !out.timed_out && out.status == Some(0) => None,
        Ok(out) if out.timed_out => Some(format!(
            "comparator timed out after {}s",
            ctx.cfg.budget.lean_timeout_s
        )),
        Ok(out) => {
            let stderr = out.stderr_lossy();
            let stdout = out.stdout_lossy();
            let msg = if !stderr.trim().is_empty() {
                stderr.trim().to_string()
            } else {
                stdout.trim().to_string()
            };
            let code = out
                .status
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            if msg.is_empty() {
                Some(format!("comparator exited with status {code}"))
            } else {
                Some(format!("comparator exited with status {code}: {msg}"))
            }
        }
        Err(e) => Some(format!("comparator failed to run: {e}")),
    }
}

pub fn update_state_from_verdict(ctx: &Ctx, verdict: &Verdict) -> Result<()> {
    util::update_state(&ctx.campaign, |state| {
        let round = state.round;
        for (tname, tv) in &verdict.targets {
            let prev_since = state
                .targets
                .get(tname)
                .and_then(|prev| if prev.accepted { prev.since_round } else { None });
            let since_round = if tv.accepted {
                Some(prev_since.unwrap_or(round))
            } else {
                None
            };
            let reasons = tv.reasons.iter().map(|r| r.as_str().to_string()).collect();
            state.targets.insert(
                tname.clone(),
                TargetState {
                    accepted: tv.accepted,
                    reasons,
                    since_round,
                },
            );
        }
    })?;
    Ok(())
}

pub fn gate_cmd(ctx: &Ctx, json: bool) -> Result<(String, u8)> {
    let verdict = run(ctx)?;
    let _ = update_state_from_verdict(ctx, &verdict);

    let out = if json {
        let mut s = serde_json::to_string_pretty(&verdict)
            .map_err(|e| VdashError::new(5, format!("failed to serialize gate verdict: {e}")))?;
        s.push('\n');
        s
    } else {
        let mut lines = Vec::new();
        for (tname, tv) in &verdict.targets {
            if tv.accepted {
                lines.push(format!("ACCEPTED {tname}"));
            } else {
                let r_strs: Vec<&str> = tv.reasons.iter().map(|r| r.as_str()).collect();
                lines.push(format!("REJECTED {tname}: {}", r_strs.join(", ")));
            }
        }
        let mut s = lines.join("\n");
        s.push('\n');
        s
    };

    let code = if !verdict.anchor_ok {
        3
    } else if verdict.ok {
        0
    } else {
        1
    };
    Ok((out, code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::jsonio;
    use crate::paths;

    fn mk_gate_ctx(tmp: &TempDir) -> (Ctx, PathBuf, PathBuf, PathBuf, PathBuf, Vec<String>) {
        let host = tmp.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();

        let camp = tmp.path().join("camp");
        paths::init_campaign(&camp, &host, None, &[BackendKind::Claude]).unwrap();
        let ctx = Ctx::for_test(&camp, tmp.path().join("state")).unwrap();

        let chal_build = camp.join(".heldout/build");
        let sol_build = camp.join(".build");
        let chal_tree = camp.join(".heldout/lean");
        let sol_tree = camp.join("lean");
        let targets = vec!["Apc.TargetOne".to_string(), "Apc.TargetTwo".to_string()];
        (ctx, chal_build, sol_build, chal_tree, sol_tree, targets)
    }

    #[test]
    fn comparator_empty_is_noop() {
        let tmp = TempDir::new().unwrap();
        let (ctx, cb, sb, ct, st, targets) = mk_gate_ctx(&tmp);
        assert_eq!(run_comparator(&ctx, &cb, &sb, &ct, &st, &targets), None);
    }

    #[test]
    fn comparator_passes_args_and_env_and_accepts_zero() {
        let tmp = TempDir::new().unwrap();
        let (mut ctx, cb, sb, ct, st, targets) = mk_gate_ctx(&tmp);
        ctx.cfg.use_mathlib = true;

        // Pre-populate the host lean_path cache so lean::lean_path does not invoke `lake`.
        jsonio::write_atomic(
            ctx.campaign.lean_path_cache(),
            &serde_json::json!({
                "version": 1,
                "host_project": ctx.cfg.host_project.display().to_string(),
                "manifest_mtime_ns": 0,
                "lean_path": "/opt/mathlib/lib/lean",
            }),
        )
        .unwrap();

        let dump = tmp.path().join("env_dump.txt");
        let script = tmp.path().join("cmp_ok.sh");
        fs::write(
            &script,
            format!(
                "printf '%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n' \
                 \"$1\" \"$2\" \"$ELAN_TOOLCHAIN\" \"$VDASH_CHALLENGE_BUILD\" \
                 \"$VDASH_SOLUTION_BUILD\" \"$VDASH_CHALLENGE_TREE\" \"$VDASH_SOLUTION_TREE\" \
                 \"$VDASH_TARGETS\" \"$VDASH_HOST_LEAN_PATH\" > {}\n",
                dump.display()
            ),
        )
        .unwrap();
        ctx.cfg.policy.comparator_cmd = format!("sh {}", script.display());

        let err = run_comparator(&ctx, &cb, &sb, &ct, &st, &targets);
        assert_eq!(err, None);

        let lines: Vec<String> = fs::read_to_string(&dump)
            .unwrap()
            .lines()
            .map(ToOwned::to_owned)
            .collect();
        assert_eq!(
            lines,
            vec![
                cb.display().to_string(),
                sb.display().to_string(),
                "leanprover/lean4:v4.33.0".to_string(),
                cb.display().to_string(),
                sb.display().to_string(),
                ct.display().to_string(),
                st.display().to_string(),
                "Apc.TargetOne,Apc.TargetTwo".to_string(),
                "/opt/mathlib/lib/lean".to_string(),
            ]
        );
    }

    #[test]
    fn comparator_reports_nonzero_and_missing_binary() {
        let tmp = TempDir::new().unwrap();
        let (mut ctx, cb, sb, ct, st, targets) = mk_gate_ctx(&tmp);

        let script = tmp.path().join("cmp_fail.sh");
        fs::write(&script, "echo 'mismatch on Apc.TargetOne' >&2\nexit 3\n").unwrap();
        ctx.cfg.policy.comparator_cmd = format!("sh {}", script.display());
        let err = run_comparator(&ctx, &cb, &sb, &ct, &st, &targets).unwrap();
        assert!(err.contains("status 3"), "got: {err}");
        assert!(err.contains("mismatch on Apc.TargetOne"), "got: {err}");

        ctx.cfg.policy.comparator_cmd = "nonexistent-vdash-comparator-bin".to_string();
        let err2 = run_comparator(&ctx, &cb, &sb, &ct, &st, &targets).unwrap();
        assert!(err2.contains("comparator failed to run"), "got: {err2}");
    }
}
