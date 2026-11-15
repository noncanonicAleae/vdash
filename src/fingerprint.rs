use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{VdashError, Result};
use crate::lean::{self, slots};
use crate::paths::Ctx;
use crate::proc;

pub const FINGERPRINT_LEAN: &str = include_str!("../lean_src/Fingerprint.lean");
const MAX_OUTPUT_BYTES: usize = 50 * 1024 * 1024;
static DRIVER_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureEntry {
    pub module: String,
    pub ext: bool,
    pub ser: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetFingerprint {
    pub kind: String,
    pub module: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub axioms: Vec<String>,
    pub closure: BTreeMap<String, ClosureEntry>,
}

pub type Fingerprints = BTreeMap<String, TargetFingerprint>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclInfo {
    pub exists: bool,
    pub kind: String,
    pub axioms: Vec<String>,
    pub statement: Option<String>,
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

fn run_driver(
    ctx: &Ctx,
    tree: &Path,
    build_dir: &Path,
    root_module: &str,
    mode: &str,
    names: &[String],
) -> Result<String> {
    let lp = lean::lean_path(ctx, build_dir)?;
    let driver_dir = build_dir.join("_apc");
    fs::create_dir_all(&driver_dir).map_err(|e| VdashError::io_path(&driver_dir, e))?;

    let quoted_names: Vec<String> = names
        .iter()
        .map(|n| serde_json::to_string(n).unwrap_or_else(|_| format!("\"{n}\"")))
        .collect();
    let names_list = format!("[{}]", quoted_names.join(", "));

    let driver_src = format!(
        "import Lean\nimport {root_module}\n\n{FINGERPRINT_LEAN}\n#eval ApcTool.main \"{mode}\" {names_list}\n"
    );

    let pid = std::process::id();
    let seq = DRIVER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let driver_path = driver_dir.join(format!("Driver_{pid}_{seq}.lean"));
    fs::write(&driver_path, &driver_src).map_err(|e| VdashError::io_path(&driver_path, e))?;

    let _slot = slots::acquire(ctx)?;
    let mut cmd = lean::command(ctx, "lean")?;
    cmd.env("LEAN_PATH", &lp)
        .arg("-R")
        .arg(tree)
        .arg(&driver_path);

    let run_res = proc::run(
        &mut cmd,
        None,
        Duration::from_secs(ctx.cfg.budget.lean_timeout_s),
    );
    let _ = fs::remove_file(&driver_path);
    let out = run_res?;

    if out.stdout.len().saturating_add(out.stderr.len()) > MAX_OUTPUT_BYTES {
        return Err(VdashError::new(
            1,
            "fingerprint script output exceeded 50 MB limit",
        ));
    }

    let stdout = out.stdout_lossy();
    let stderr = out.stderr_lossy();
    if out.timed_out || out.status != Some(0) {
        return Err(VdashError::new(
            1,
            format!(
                "fingerprint driver failed (status {:?}):\n{}\n{}",
                out.status,
                stdout.trim(),
                stderr.trim()
            ),
        ));
    }

    let mut fp_lines = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("APCFP "))
        .collect::<Vec<_>>();
    if fp_lines.len() != 1 {
        return Err(VdashError::new(
            1,
            format!(
                "expected exactly one APCFP line from driver, got {}:\n{}\n{}",
                fp_lines.len(),
                stdout.trim(),
                stderr.trim()
            ),
        ));
    }

    Ok(fp_lines.pop().unwrap().to_string())
}

fn hash_fingerprints(mut fps: Fingerprints) -> Fingerprints {
    for tfp in fps.values_mut() {
        tfp.ty = sha256_hex(&tfp.ty);
        for ce in tfp.closure.values_mut() {
            if let Some(raw_ser) = &ce.ser {
                ce.ser = Some(sha256_hex(raw_ser));
            }
        }
    }
    fps
}

pub fn targets(ctx: &Ctx, tree: &Path, build_dir: &Path) -> Result<Fingerprints> {
    let raw_json = run_driver(ctx, tree, build_dir, "Apc.Challenge", "targets", &[])?;
    let fps: Fingerprints = serde_json::from_str(&raw_json).map_err(|e| {
        VdashError::new(
            1,
            format!("invalid JSON from fingerprint driver (targets): {e}"),
        )
    })?;
    Ok(hash_fingerprints(fps))
}

pub fn closure(
    ctx: &Ctx,
    tree: &Path,
    build_dir: &Path,
    root_module: &str,
    names: &[String],
) -> Result<Fingerprints> {
    let raw_json = run_driver(ctx, tree, build_dir, root_module, "closure", names)?;
    let fps: Fingerprints = serde_json::from_str(&raw_json).map_err(|e| {
        VdashError::new(
            1,
            format!("invalid JSON from fingerprint driver (closure): {e}"),
        )
    })?;
    Ok(hash_fingerprints(fps))
}

pub fn axioms(
    ctx: &Ctx,
    tree: &Path,
    build_dir: &Path,
    root_module: &str,
    names: &[String],
) -> Result<BTreeMap<String, DeclInfo>> {
    let raw_json = run_driver(ctx, tree, build_dir, root_module, "axioms", names)?;
    serde_json::from_str(&raw_json).map_err(|e| {
        VdashError::new(
            1,
            format!("invalid JSON from fingerprint driver (axioms): {e}"),
        )
    })
}

pub fn is_standard_axiom(ax: &str) -> bool {
    matches!(ax, "propext" | "Quot.sound" | "Classical.choice")
}

pub fn axioms_cmd(
    ctx: &Ctx,
    file_arg: &Path,
    names: &[String],
    cwd: &Path,
) -> Result<(String, bool)> {
    let resolved = if file_arg.is_absolute() {
        file_arg.to_path_buf()
    } else if ctx.campaign.root.join(file_arg).exists() {
        ctx.campaign.root.join(file_arg)
    } else {
        cwd.join(file_arg)
    };

    let mod_name = ctx.campaign.module_name(&resolved)?;
    let src_path = ctx.campaign.root.join(ctx.campaign.module_path(&mod_name));
    if !src_path.is_file() {
        return Err(VdashError::new(
            2,
            format!("missing file {}", src_path.display()),
        ));
    }

    let tree = ctx.campaign.lean_dir();
    let build_dir = ctx.campaign.build_work_dir();
    let rep = lean::build::build(ctx, &tree, &build_dir, Some(std::slice::from_ref(&mod_name)))?;
    if !rep.ok {
        let mut all_diags = Vec::new();
        for r in rep.results.values() {
            all_diags.extend(r.diagnostics.clone());
        }
        let mut lines = lean::build::format_capped_diagnostics(&all_diags);
        lines.push("FAILED".to_string());
        let mut out = lines.join("\n");
        out.push('\n');
        return Ok((out, false));
    }

    let map = axioms(ctx, &tree, &build_dir, &mod_name, names)?;
    let mut all_ok = true;
    let mut lines = Vec::new();

    for (idx, name) in names.iter().enumerate() {
        let line = match map.get(name) {
            Some(info) if info.exists => {
                if info.kind != "theorem" {
                    all_ok = false;
                    format!("{name}: {} (not a theorem)", info.kind)
                } else {
                    if !info.axioms.iter().all(|a| is_standard_axiom(a)) {
                        all_ok = false;
                    }
                    let ax_str = if info.axioms.is_empty() {
                        "none".to_string()
                    } else {
                        info.axioms.join(", ")
                    };
                    format!("{name}: theorem, axioms: {ax_str}")
                }
            }
            _ => {
                all_ok = false;
                format!("{name}: MISSING")
            }
        };
        if idx < 60 {
            lines.push(lean::build::truncate_text(&line, 1500));
        }
    }
    if names.len() > 60 {
        lines.push(format!("… [{} more diagnostics]", names.len() - 60));
    }

    let mut out = lines.join("\n");
    out.push('\n');
    Ok((out, all_ok))
}

pub fn tcb_cmd(ctx: &Ctx) -> Result<String> {
    let tree = ctx.campaign.lean_dir();
    let build_dir = ctx.campaign.build_work_dir();
    let target_mod = "Apc.Challenge".to_string();
    let rep = lean::build::build(ctx, &tree, &build_dir, Some(std::slice::from_ref(&target_mod)))?;
    if !rep.ok {
        return Err(VdashError::new(1, "failed to build Apc.Challenge for tcb"));
    }

    let fps = targets(ctx, &tree, &build_dir)?;
    let mut lines = Vec::new();

    for (tname, tfp) in &fps {
        lines.push(format!("target {tname} ({}):", tfp.module));
        let mut by_mod: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut ext_ns: BTreeMap<String, usize> = BTreeMap::new();

        for (cname, ce) in &tfp.closure {
            if ce.ext {
                let top_ns = cname.split('.').next().unwrap_or(cname).to_string();
                *ext_ns.entry(top_ns).or_insert(0) += 1;
            } else {
                by_mod.entry(ce.module.clone()).or_default().push(cname.clone());
            }
        }

        for (m, mut cs) in by_mod {
            cs.sort();
            lines.push(format!("  {m}: {}", cs.join(", ")));
        }

        if !ext_ns.is_empty() {
            let ext_parts: Vec<String> = ext_ns
                .into_iter()
                .map(|(ns, cnt)| format!("{ns} ({cnt})"))
                .collect();
            lines.push(format!("  external: {}", ext_parts.join(", ")));
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}
