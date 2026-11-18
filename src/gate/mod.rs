pub mod scan;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rustix::fs::{flock, FlockOperation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use walkdir::WalkDir;

use crate::anchor;
use crate::error::{VdashError, Result};
use crate::fingerprint;
use crate::jsonio;
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

struct GateLock(fs::File);

impl Drop for GateLock {
    fn drop(&mut self) {
        let _ = flock(&self.0, FlockOperation::Unlock);
    }
}

fn acquire_gate_lock(cache_root: &Path) -> Result<GateLock> {
    fs::create_dir_all(cache_root).map_err(|e| VdashError::io_path(cache_root, e))?;
    let lock_path = cache_root.join("gate.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| VdashError::io_path(&lock_path, e))?;
    flock(&file, FlockOperation::LockExclusive)
        .map_err(|e| VdashError::new(5, format!("cannot lock {}: {e}", lock_path.display())))?;
    Ok(GateLock(file))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedSolFps {
    version: u32,
    sol_key: String,
    target_names: Vec<String>,
    fps: fingerprint::Fingerprints,
}

fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

fn module_rel_stem(module: &str) -> PathBuf {
    let mut p = PathBuf::new();
    for part in module.split('.') {
        p.push(part);
    }
    p
}

fn module_olean_path(build_dir: &Path, module: &str) -> PathBuf {
    let mut p = build_dir.join(module_rel_stem(module));
    p.set_extension("olean");
    p
}

fn olean_lc_marker_path(olean: &Path) -> PathBuf {
    let mut os = olean.as_os_str().to_os_string();
    os.push(".lc_ok");
    PathBuf::from(os)
}

fn expected_lc_marker(olean_path: &Path, module_key: &str) -> Option<String> {
    let bytes = fs::read(olean_path).ok()?;
    Some(format!("{module_key} {}\n", sha256_hex_bytes(&bytes)))
}

fn prune_stale_build_files(build_dir: &Path, keep_modules: &BTreeSet<String>) {
    let apc_dir = build_dir.join("Apc");
    if !apc_dir.is_dir() {
        return;
    }
    let suffixes = [
        ".olean.diag.json",
        ".olean.key",
        ".olean.lc_ok",
        ".olean.lock",
        ".olean",
    ];
    for entry in WalkDir::new(&apc_dir).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(build_dir) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let Some(stem) = suffixes.iter().find_map(|sfx| rel_str.strip_suffix(sfx)) else {
            continue;
        };
        let mod_name = stem.replace('/', ".");
        if !keep_modules.contains(&mod_name) {
            let _ = fs::remove_file(path);
        }
    }
}

fn prune_old_chal_fps(cache_root: &Path, keep_name: &str) {
    let Ok(rd) = fs::read_dir(cache_root) else {
        return;
    };
    for entry in rd.flatten() {
        let fname = entry.file_name();
        let s = fname.to_string_lossy();
        if s.starts_with("chal_fps_") && s.ends_with(".json") && s != keep_name {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn hash_dir_files(hasher: &mut Sha256, label: &[u8], dir: &Path) {
    hasher.update(label);
    let mut files: Vec<PathBuf> = Vec::new();
    if dir.is_dir() {
        for entry in WalkDir::new(dir).follow_links(false).into_iter().flatten() {
            if entry.file_type().is_file() {
                if let Ok(rel) = entry.path().strip_prefix(dir) {
                    files.push(rel.to_path_buf());
                }
            }
        }
    }
    files.sort();
    for rel in files {
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        if let Ok(bytes) = fs::read(dir.join(&rel)) {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        } else {
            hasher.update(b"<unreadable>");
        }
        hasher.update(b"\n");
    }
}

/// Cryptographic digest of every input that [`run`] depends on, used by the
/// engine to skip redundant `gate_pre` / `gate_post` executions when nothing
/// in `.heldout/`, `amendments/`, `Apc.Solution`'s closure, or policy changed.
pub fn input_digest(ctx: &Ctx) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"gate-input-v1\n");
    hasher.update(ctx.cfg.id.as_bytes());
    hasher.update(b"\0");
    hasher.update([u8::from(ctx.cfg.use_mathlib)]);
    hasher.update([u8::from(ctx.cfg.policy.kernel_recheck)]);
    hasher.update(ctx.cfg.policy.comparator_cmd.as_bytes());
    hasher.update(b"\0");
    hasher.update(ctx.cfg.host_project.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    if let Ok(tc) = lean::toolchain(&ctx.cfg) {
        hasher.update(tc.as_bytes());
    }
    hasher.update(b"\0");
    if let Ok(lp) = lean::lean_path(ctx, Path::new("")) {
        hasher.update(lp.as_bytes());
    }
    hasher.update(b"\n");

    let anc_path = anchor::anchor_path(ctx);
    if let Ok(bytes) = fs::read(&anc_path) {
        hasher.update(b"anchor:");
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }

    hash_dir_files(&mut hasher, b"heldout:\n", &ctx.campaign.heldout_dir());
    hash_dir_files(&mut hasher, b"amendments:\n", &ctx.campaign.amendments_dir());

    let lean_dir = ctx.campaign.lean_dir();
    if !ctx.campaign.solution_file().is_file() {
        hasher.update(b"sol:missing\n");
    } else {
        let mut scan = lean::imports::scan_imports(&lean_dir)?;
        // `gate::run` copies `lean/` without `Apc/Challenge.lean`, so remove it
        // from the import graph before computing `Apc.Solution`'s closure.
        scan.graph.remove("Apc.Challenge");
        scan.rel_paths.remove("Apc.Challenge");
        let sol_target = ["Apc.Solution".to_string()];
        let closure = lean::imports::closure_of(&scan.graph, &sol_target);
        hasher.update(b"sol_closure:\n");
        for m in closure {
            hasher.update(m.as_bytes());
            hasher.update(b"\0");
            if let Some(rel) = scan.rel_paths.get(&m) {
                let p = lean_dir.join(rel);
                if let Ok(bytes) = fs::read(&p) {
                    hasher.update((bytes.len() as u64).to_le_bytes());
                    hasher.update(&bytes);
                }
            } else {
                hasher.update(b"<missing>");
            }
            hasher.update(b"\n");
        }
    }

    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    Ok(hex)
}

pub fn run(ctx: &Ctx) -> Result<Verdict> {
    run_with_opts(ctx, false)
}

pub fn run_fresh(ctx: &Ctx) -> Result<Verdict> {
    run_with_opts(ctx, true)
}

pub fn run_with_opts(ctx: &Ctx, fresh: bool) -> Result<Verdict> {
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

    let cache_root = ctx.state_dir.join("gate_cache");
    let _gate_lock = if fresh {
        None
    } else {
        Some(acquire_gate_lock(&cache_root)?)
    };

    // Temp work directory inside .build/gate for tree copies (and fresh build dirs when `fresh`).
    let gate_base = ctx.campaign.build_gate_dir();
    fs::create_dir_all(&gate_base).map_err(|e| VdashError::io_path(&gate_base, e))?;
    let tmp = TempDir::new_in(&gate_base).map_err(|e| VdashError::io_path(&gate_base, e))?;

    let need_chal_build = !ctx.cfg.policy.comparator_cmd.trim().is_empty();
    let tc = lean::toolchain(&ctx.cfg)?;
    let host_lp = lean::lean_path(ctx, Path::new(""))?;
    let anchor_manifest_hash = jsonio::read_json::<anchor::Anchor>(anchor::anchor_path(ctx))
        .map(|a| a.manifest_hash)
        .unwrap_or_default();
    let chal_key = sha256_hex_bytes(
        format!("chal-v1\0{anchor_manifest_hash}\0{tc}\0{host_lp}").as_bytes(),
    );
    let chal_fps_name = format!("chal_fps_{chal_key}.json");
    let chal_fps_path = cache_root.join(&chal_fps_name);

    let cached_chal_fps = if !fresh && chal_fps_path.is_file() {
        jsonio::read_json::<fingerprint::Fingerprints>(&chal_fps_path).ok()
    } else {
        None
    };

    // Step 1a + 2a: Challenge tree, build, and fingerprint (reusing state_dir cache when !fresh).
    let chal_tree = tmp.path().join("chal_tree");
    let chal_build = if fresh {
        tmp.path().join("chal_build")
    } else {
        cache_root.join("chal_build")
    };

    let chal_fps = match (cached_chal_fps, need_chal_build) {
        (Some(fps), false) => fps,
        (maybe_fps, _) => {
            fs::create_dir_all(&chal_build).map_err(|e| VdashError::io_path(&chal_build, e))?;
            util::copy_tree(&ctx.campaign.heldout_tree(), &chal_tree, &|_| false)?;
            let chal_targets = if need_chal_build {
                None
            } else {
                Some(vec!["Apc.Challenge".to_string()])
            };
            let chal_rep = lean::build::build(ctx, &chal_tree, &chal_build, chal_targets.as_deref())
                .map_err(|e| VdashError::new(5, format!("challenge build failed: {}", e.message)))?;
            if !chal_rep.ok {
                return Err(VdashError::new(
                    5,
                    "held-out challenge tree failed to compile".to_string(),
                ));
            }
            if !fresh && need_chal_build {
                let keep: BTreeSet<String> = chal_rep.results.keys().cloned().collect();
                prune_stale_build_files(&chal_build, &keep);
            }
            if let Some(fps) = maybe_fps {
                fps
            } else {
                let fps = fingerprint::targets(ctx, &chal_tree, &chal_build).map_err(|e| {
                    VdashError::new(5, format!("challenge fingerprint failed: {}", e.message))
                })?;
                if !fresh {
                    prune_old_chal_fps(&cache_root, &chal_fps_name);
                    let _ = jsonio::write_atomic(&chal_fps_path, &fps);
                }
                fps
            }
        }
    };
    let target_names: Vec<String> = chal_fps.keys().cloned().collect();

    // Step 1b + 2b: Copy solution tree (without Apc/Challenge.lean) and build Apc.Solution closure.
    let sol_tree = tmp.path().join("sol_tree");
    let sol_build = if fresh {
        tmp.path().join("sol_build")
    } else {
        cache_root.join("sol_build")
    };
    fs::create_dir_all(&sol_build).map_err(|e| VdashError::io_path(&sol_build, e))?;
    util::copy_tree(&ctx.campaign.lean_dir(), &sol_tree, &|rel| {
        rel == Path::new("Apc/Challenge.lean")
    })?;

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
    let (sol_rep, sol_keys) =
        match lean::build::build_with_keys(ctx, &sol_tree, &sol_build, Some(&sol_target)) {
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
    if !fresh {
        prune_stale_build_files(&sol_build, &sol_closure);
    }

    let heldout_tree = ctx.campaign.heldout_tree();
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
            let held_file = heldout_tree.join(&rel);
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

    // Step 4: kernel_recheck via leanchecker in parallel across `max_parallel_lean` slots,
    // skipping modules whose `(cache_key, olean_sha256)` already passed in `state_dir`.
    if ctx.cfg.policy.kernel_recheck {
        let sol_lp = lean::lean_path(ctx, &sol_build)?;
        let mut to_check: Vec<(String, Option<(PathBuf, String)>)> = Vec::new();
        for m in &untrusted_mods {
            if !fresh {
                if let Some(m_key) = sol_keys.get(m) {
                    let olean_path = module_olean_path(&sol_build, m);
                    let marker_path = olean_lc_marker_path(&olean_path);
                    if let Some(expected) = expected_lc_marker(&olean_path, m_key) {
                        if fs::read_to_string(&marker_path).ok().as_deref() == Some(expected.as_str()) {
                            continue;
                        }
                        to_check.push((m.clone(), Some((marker_path, expected))));
                        continue;
                    }
                }
            }
            to_check.push((m.clone(), None));
        }

        if !to_check.is_empty() {
            let sol_lp_ref = sol_lp.as_str();
            let lc_results: Vec<Result<std::result::Result<(), (bool, String)>>> =
                std::thread::scope(|s| {
                    let handles: Vec<_> = to_check
                        .iter()
                        .map(|(m, _)| {
                            s.spawn(move || {
                                let _slot = lean::slots::acquire(ctx)?;
                                let mut lc_cmd = lean::command(ctx, "leanchecker")?;
                                lc_cmd.env("LEAN_PATH", sol_lp_ref).arg(m);
                                let out = proc::run(
                                    &mut lc_cmd,
                                    None,
                                    Duration::from_secs(ctx.cfg.budget.lean_timeout_s),
                                )?;
                                if out.timed_out || out.status != Some(0) {
                                    Ok(Err((out.timed_out, out.stderr_lossy().trim().to_string())))
                                } else {
                                    Ok(Ok(()))
                                }
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .unwrap_or_else(|_| Err(VdashError::new(5, "leanchecker thread panicked")))
                        })
                        .collect()
                });

            let mut first_fail: Option<(String, bool, String)> = None;
            for ((m, marker), res) in to_check.into_iter().zip(lc_results) {
                match res? {
                    Ok(()) => {
                        if let Some((marker_path, expected)) = marker {
                            let _ = fs::write(&marker_path, expected);
                        }
                    }
                    Err((timed_out, stderr)) => {
                        if first_fail.is_none() {
                            first_fail = Some((m, timed_out, stderr));
                        }
                    }
                }
            }

            if let Some((m, timed_out, stderr)) = first_fail {
                return Ok(reject_all(
                    target_names,
                    ReasonCode::KernelRecheckFailed,
                    serde_json::json!({
                        "module": m,
                        "timed_out": timed_out,
                        "stderr": stderr,
                    }),
                    true,
                    vec!["kernel_recheck_failed".to_string()],
                ));
            }
        }
    }

    // Step 5: Fingerprint solution for the challenge target names (root Apc.Solution),
    // reusing `sol_fps.json` when `Apc.Solution`'s build key and `target_names` match.
    let sol_fps_path = cache_root.join("sol_fps.json");
    let sol_fps = if !fresh {
        let cached = sol_keys.get("Apc.Solution").and_then(|sk| {
            jsonio::read_json::<CachedSolFps>(&sol_fps_path)
                .ok()
                .filter(|c| c.version == 1 && &c.sol_key == sk && c.target_names == target_names)
                .map(|c| c.fps)
        });
        match cached {
            Some(fps) => fps,
            None => {
                let fps =
                    fingerprint::closure(ctx, &sol_tree, &sol_build, "Apc.Solution", &target_names)?;
                if let Some(sk) = sol_keys.get("Apc.Solution") {
                    let cached_entry = CachedSolFps {
                        version: 1,
                        sol_key: sk.clone(),
                        target_names: target_names.clone(),
                        fps: fps.clone(),
                    };
                    let _ = jsonio::write_atomic(&sol_fps_path, &cached_entry);
                }
                fps
            }
        }
    } else {
        fingerprint::closure(ctx, &sol_tree, &sol_build, "Apc.Solution", &target_names)?
    };

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
    gate_cmd_opts(ctx, json, false)
}

pub fn gate_cmd_opts(ctx: &Ctx, json: bool, fresh: bool) -> Result<(String, u8)> {
    let verdict = run_with_opts(ctx, fresh)?;
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

    #[test]
    fn input_digest_ignores_unimported_work_file_and_detects_closure_and_policy_changes() {
        let tmp = TempDir::new().unwrap();
        let (mut ctx, _, _, _, _, _) = mk_gate_ctx(&tmp);
        fs::create_dir_all(ctx.campaign.proof_work_dir()).unwrap();
        let sol = ctx.campaign.solution_file();
        fs::write(&sol, "import Apc.Spec.Defs\n").unwrap();
        let d0 = input_digest(&ctx).unwrap();

        // Editing an unimported Proof/Work file does not change Apc.Solution's closure digest.
        let scratch = ctx.campaign.proof_work_dir().join("Scratch.lean");
        fs::write(&scratch, "def helper : Nat := 1\n").unwrap();
        assert_eq!(input_digest(&ctx).unwrap(), d0);

        // Importing it changes the digest, and editing it afterwards also changes the digest.
        fs::write(&sol, "import Apc.Spec.Defs\nimport Apc.Proof.Work.Scratch\n").unwrap();
        let d1 = input_digest(&ctx).unwrap();
        assert_ne!(d1, d0);
        fs::write(&scratch, "def helper : Nat := 2\n").unwrap();
        let d2 = input_digest(&ctx).unwrap();
        assert_ne!(d2, d1);

        // Changing policy changes the digest.
        ctx.cfg.policy.kernel_recheck = false;
        let d3 = input_digest(&ctx).unwrap();
        assert_ne!(d3, d2);
    }
}
