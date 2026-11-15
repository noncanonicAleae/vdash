use std::fs;
use std::time::UNIX_EPOCH;

use crate::anchor;
use crate::error::{VdashError, Result};
use crate::events;
use crate::fingerprint;
use crate::lean;
use crate::paths::{Ctx, TargetState};
use crate::util;

fn mtime_ns(path: &std::path::Path) -> u128 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub fn approve(ctx: &Ctx) -> Result<String> {
    // 1. state.phase must be init or spec, and anchor must not already exist
    let state = util::load_state(&ctx.campaign)?;
    if state.phase != "init" && state.phase != "spec" {
        return Err(VdashError::new(
            2,
            format!(
                "cannot approve campaign in phase '{}' (expected 'init' or 'spec')",
                state.phase
            ),
        ));
    }
    if anchor::anchor_path(ctx).exists() {
        return Err(VdashError::new(
            2,
            format!(
                "anchor already exists at {}",
                anchor::anchor_path(ctx).display()
            ),
        ));
    }

    let trusted_rels = ctx.campaign.trusted_files();
    if !ctx.campaign.challenge_file().is_file() {
        return Err(VdashError::new(
            1,
            "missing lean/Apc/Challenge.lean".to_string(),
        ));
    }

    let mut trusted_mods = Vec::with_capacity(trusted_rels.len());
    for rel in &trusted_rels {
        trusted_mods.push(ctx.campaign.module_name(rel)?);
    }

    // 2. Build the trusted modules
    let tree = ctx.campaign.lean_dir();
    let build_dir = ctx.campaign.build_work_dir();
    let rep = lean::build::build(ctx, &tree, &build_dir, Some(&trusted_mods))?;
    if !rep.ok {
        let mut diags = Vec::new();
        for r in rep.results.values() {
            diags.extend(r.diagnostics.clone());
        }
        let lines = lean::build::format_capped_diagnostics(&diags);
        return Err(VdashError::new(
            1,
            format!("trusted modules failed to build:\n{}", lines.join("\n")),
        ));
    }

    // 3. Every hasSorry diagnostic must be in Challenge.lean; every theorem in Challenge.lean must have one
    for (mod_name, mr) in &rep.results {
        if mod_name != "Apc.Challenge" {
            for d in &mr.diagnostics {
                if d.kind == "hasSorry" || (d.severity == "warning" && d.text.contains("sorry")) {
                    return Err(VdashError::new(
                        1,
                        format!(
                            "sorry found in {} ({}:{}): only Challenge.lean may contain sorry",
                            mod_name, d.file, d.line
                        ),
                    ));
                }
            }
        }
    }

    let fps = fingerprint::targets(ctx, &tree, &build_dir)?;
    if fps.is_empty() {
        return Err(VdashError::new(
            1,
            "no target theorems found in Apc.Challenge".to_string(),
        ));
    }
    for (tname, tfp) in &fps {
        if !tfp.axioms.iter().any(|a| a == "sorryAx") {
            return Err(VdashError::new(
                1,
                format!(
                    "target theorem '{tname}' in Challenge.lean does not use sorry (already proved)"
                ),
            ));
        }
    }
    let chal_sorry_count = rep
        .results
        .get("Apc.Challenge")
        .map(|mr| {
            mr.diagnostics
                .iter()
                .filter(|d| {
                    d.kind == "hasSorry" || (d.severity == "warning" && d.text.contains("sorry"))
                })
                .count()
        })
        .unwrap_or(0);
    if chal_sorry_count != fps.len() {
        return Err(VdashError::new(
            1,
            format!(
                "Challenge.lean has {chal_sorry_count} sorry warnings for {} target theorems",
                fps.len()
            ),
        ));
    }

    let mut out_lines = Vec::new();

    // 4. Warn for each line starting with `theorem` or `lemma` in Spec/** outside Tests.lean
    for rel in &trusted_rels {
        let rel_str = rel.display().to_string();
        if rel_str.starts_with("lean/Apc/Spec/") && !rel_str.ends_with("/Tests.lean") {
            let full = ctx.campaign.root.join(rel);
            let content = fs::read_to_string(&full).map_err(|e| VdashError::io_path(&full, e))?;
            for (idx, line) in content.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("theorem ") || trimmed.starts_with("lemma ") {
                    out_lines.push(format!(
                        "warning: {}:{}: theorem/lemma in Spec outside Tests.lean",
                        rel_str,
                        idx + 1
                    ));
                }
            }
        }
    }

    // 5. Warn if spec/review-*.md exists and the newest is older than the newest trusted file
    let spec_dir = ctx.campaign.spec_dir();
    if spec_dir.is_dir() {
        let mut max_review_mtime = 0u128;
        let mut newest: Option<std::path::PathBuf> = None;
        if let Ok(entries) = fs::read_dir(&spec_dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().into_owned();
                if fname.starts_with("review-") && fname.ends_with(".md") {
                    let m = mtime_ns(&entry.path());
                    if m >= max_review_mtime {
                        newest = Some(entry.path());
                    }
                    max_review_mtime = max_review_mtime.max(m);
                }
            }
        }
        if let Some(text) = newest.and_then(|p| fs::read_to_string(p).ok()) {
            let failed = text.matches("**reviewer failed**").count();
            if failed > 0 {
                out_lines.push(format!(
                    "warning: {failed} reviewer(s) failed in the newest spec review; it is incomplete (rerun `vdash spec review`)"
                ));
            }
        }
        if max_review_mtime > 0 {
            let max_trusted_mtime = trusted_rels
                .iter()
                .map(|r| mtime_ns(&ctx.campaign.root.join(r)))
                .max()
                .unwrap_or(0);
            if max_review_mtime < max_trusted_mtime {
                out_lines.push(
                    "warning: newest spec/review-*.md is older than the newest trusted file"
                        .to_string(),
                );
            }
        }
    }

    // 6. Write Solution.lean if absent
    let sol_path = ctx.campaign.solution_file();
    if !sol_path.exists() {
        let chal_bytes = fs::read(ctx.campaign.challenge_file())
            .map_err(|e| VdashError::io_path(ctx.campaign.challenge_file(), e))?;
        fs::write(&sol_path, chal_bytes).map_err(|e| VdashError::io_path(&sol_path, e))?;
    }

    // 7. git add -A && git commit -q -m "vdash: baseline (spec approved)"
    let commit_hash = util::commit_all(&ctx.campaign, "vdash: baseline (spec approved)")?;

    // 8. Copy trusted files to .heldout/ preserving campaign-relative paths; make them read-only
    let heldout_dir = ctx.campaign.heldout_dir();
    for rel in &trusted_rels {
        let src = ctx.campaign.root.join(rel);
        let dst = heldout_dir.join(rel);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p).map_err(|e| VdashError::io_path(p, e))?;
        }
        if dst.exists() {
            let _ = util::set_readonly(&dst, false);
        }
        fs::copy(&src, &dst).map_err(|e| VdashError::io_path(&dst, e))?;
        util::set_readonly(&dst, true)?;
    }

    // 9. anchor::create
    anchor::create(ctx, &commit_hash)?;

    // 10. Write state.targets and phase = "approved"
    util::update_state(&ctx.campaign, |st| {
        st.phase = "approved".to_string();
        st.targets.clear();
        for tname in fps.keys() {
            st.targets.insert(
                tname.clone(),
                TargetState {
                    accepted: false,
                    reasons: vec!["missing".to_string()],
                    since_round: None,
                },
            );
        }
        for (mod_name, mr) in &rep.results {
            if mr.seconds > 0.0 {
                st.module_times.insert(mod_name.clone(), mr.seconds);
            }
        }
    })?;

    out_lines.push(format!(
        "approved: {} targets, baseline commit {}",
        fps.len(),
        commit_hash
    ));
    let mut out = out_lines.join("\n");
    out.push('\n');
    Ok(out)
}

/// Reopen an approved, proving, or done campaign in the `spec` phase so new
/// goals can be added while preserving existing proofs in `Solution.lean` and
/// `Proof/`.
pub fn restart(ctx: &Ctx) -> Result<String> {
    if let Some(pid) = events::engine_running(ctx) {
        return Err(VdashError::new(
            2,
            format!("an engine is currently running (pid {pid}); stop it first with `vdash stop`"),
        ));
    }

    let state = util::load_state(&ctx.campaign)?;
    if !matches!(state.phase.as_str(), "approved" | "proving" | "done") {
        return Err(VdashError::new(
            2,
            format!(
                "cannot restart campaign in phase '{}' (expected 'approved', 'proving', or 'done')",
                state.phase
            ),
        ));
    }

    let anchor_file = anchor::anchor_path(ctx);
    if anchor_file.exists() {
        fs::remove_file(&anchor_file).map_err(|e| VdashError::io_path(&anchor_file, e))?;
    }

    let heldout_dir = ctx.campaign.heldout_dir();
    if heldout_dir.exists() {
        for rel in anchor::collect_heldout_files(&heldout_dir) {
            let _ = util::set_readonly(&heldout_dir.join(rel), false);
        }
        fs::remove_dir_all(&heldout_dir).map_err(|e| VdashError::io_path(&heldout_dir, e))?;
    }

    util::update_state(&ctx.campaign, |st| {
        st.phase = "spec".to_string();
        st.stage = "idle".to_string();
        st.targets.clear();
        st.round_data = serde_json::json!({ "next_revise_plan": true });
    })?;

    util::commit_all(&ctx.campaign, "vdash: restart (reopen spec phase)")?;

    Ok("restarted: campaign reopened in phase 'spec' (existing proofs preserved)\n\
        next: edit sources/BRIEF.md or lean/Apc/Challenge.lean (or run `vdash spec revise \"...\"`), then `vdash approve` and `vdash prove`\n".to_string())
}
