use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{VdashError, Result};
use crate::jsonio;
use crate::lean::{self, imports, slots};
use crate::paths::Ctx;
use crate::proc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub severity: String,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleResult {
    pub module: String,
    pub ok: bool,
    pub cached: bool,
    pub seconds: f64,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildReport {
    pub results: BTreeMap<String, ModuleResult>,
    pub ok: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedDiags {
    version: u32,
    diagnostics: Vec<Diagnostic>,
}

fn module_rel_stem(module: &str) -> PathBuf {
    let mut p = PathBuf::new();
    for part in module.split('.') {
        p.push(part);
    }
    p
}

fn module_src_path(tree: &Path, module: &str) -> PathBuf {
    let mut p = tree.join(module_rel_stem(module));
    p.set_extension("lean");
    p
}

fn module_olean_path(build_dir: &Path, module: &str) -> PathBuf {
    let mut p = build_dir.join(module_rel_stem(module));
    p.set_extension("olean");
    p
}

fn olean_key_path(olean: &Path) -> PathBuf {
    let mut os = olean.as_os_str().to_os_string();
    os.push(".key");
    PathBuf::from(os)
}

fn olean_diag_path(olean: &Path) -> PathBuf {
    let mut os = olean.as_os_str().to_os_string();
    os.push(".diag.json");
    PathBuf::from(os)
}

fn remove_olean_artifacts(olean: &Path) {
    let _ = fs::remove_file(olean);
    let _ = fs::remove_file(olean_key_path(olean));
    let _ = fs::remove_file(olean_diag_path(olean));
}

fn normalize_diag_file(raw: &str, tree: &Path, camp_root: &Path, default_rel: &str) -> String {
    if raw.is_empty() {
        return default_rel.to_string();
    }
    let p = Path::new(raw);
    if let Ok(rel) = p.strip_prefix(tree) {
        return format!("lean/{}", rel.display());
    }
    if let Ok(rel) = p.strip_prefix(camp_root) {
        return rel.display().to_string();
    }
    raw.to_string()
}

fn compute_cache_key(
    src_bytes: &[u8],
    deps: &[String],
    module_keys: &BTreeMap<String, String>,
    tc: &str,
    lp: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"src:");
    hasher.update((src_bytes.len() as u64).to_le_bytes());
    hasher.update(src_bytes);

    let mut sorted_deps = deps.to_vec();
    sorted_deps.sort();
    for dep in sorted_deps {
        let dk = module_keys.get(&dep).map(String::as_str).unwrap_or("");
        hasher.update(b"dep:");
        hasher.update(dep.as_bytes());
        hasher.update(b":");
        hasher.update(dk.as_bytes());
        hasher.update(b"\n");
    }

    hasher.update(b"tc:");
    hasher.update(tc.as_bytes());
    hasher.update(b"\nlp:");
    hasher.update(lp.as_bytes());
    hasher.update(b"\n");

    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

fn parse_lean_output(
    stdout: &str,
    stderr: &str,
    status: Option<i32>,
    tree: &Path,
    camp_root: &Path,
    default_rel: &str,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) if v.is_object() => {
                let raw_file = v.get("fileName").and_then(|x| x.as_str()).unwrap_or("");
                let file = normalize_diag_file(raw_file, tree, camp_root, default_rel);
                let line_num = v
                    .get("pos")
                    .and_then(|p| p.get("line"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(1) as u32;
                let col_num = v
                    .get("pos")
                    .and_then(|p| p.get("column"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(1) as u32;
                let severity = v
                    .get("severity")
                    .and_then(|x| x.as_str())
                    .unwrap_or("information")
                    .to_string();
                let kind = v
                    .get("kind")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let text = v
                    .get("data")
                    .and_then(|x| x.as_str())
                    .unwrap_or(trimmed)
                    .to_string();
                diags.push(Diagnostic {
                    file,
                    line: line_num,
                    col: col_num,
                    severity,
                    kind,
                    text,
                });
            }
            _ => {
                diags.push(Diagnostic {
                    file: default_rel.to_string(),
                    line: 1,
                    col: 1,
                    severity: "information".to_string(),
                    kind: String::new(),
                    text: trimmed.to_string(),
                });
            }
        }
    }

    let has_error = diags.iter().any(|d| d.severity == "error");
    if status != Some(0) && !has_error {
        let msg = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("lean exited with status {status:?}")
        };
        diags.push(Diagnostic {
            file: default_rel.to_string(),
            line: 1,
            col: 1,
            severity: "error".to_string(),
            kind: String::new(),
            text: msg,
        });
    }

    diags
}

fn compile_single_module(
    ctx: &Ctx,
    tree: &Path,
    build_dir: &Path,
    lp: &str,
    module: &str,
    key: &str,
) -> Result<ModuleResult> {
    let src_path = module_src_path(tree, module);
    let olean_path = module_olean_path(build_dir, module);
    let key_path = olean_key_path(&olean_path);
    let diag_path = olean_diag_path(&olean_path);
    let default_rel = format!("lean/{}.lean", module_rel_stem(module).display());

    if olean_path.is_file() && key_path.is_file() {
        if let Ok(stored_key) = fs::read_to_string(&key_path) {
            if stored_key.trim() == key {
                let cached_diags = if diag_path.is_file() {
                    jsonio::read_json::<CachedDiags>(&diag_path)
                        .map(|c| c.diagnostics)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                return Ok(ModuleResult {
                    module: module.to_string(),
                    ok: true,
                    cached: true,
                    seconds: 0.0,
                    diagnostics: cached_diags,
                });
            }
        }
    }

    let _slot = slots::acquire(ctx)?;
    if let Some(parent) = olean_path.parent() {
        fs::create_dir_all(parent).map_err(|e| VdashError::io_path(parent, e))?;
    }
    remove_olean_artifacts(&olean_path);

    let mut cmd = lean::command(ctx, "lean")?;
    cmd.env("LEAN_PATH", lp)
        .arg("--json")
        .arg("-R")
        .arg(tree)
        .arg("-o")
        .arg(&olean_path)
        .arg(&src_path);

    let timeout_s = ctx.cfg.budget.lean_timeout_s;
    let start = Instant::now();
    let out = proc::run(&mut cmd, None, Duration::from_secs(timeout_s))?;
    let seconds = start.elapsed().as_secs_f64();

    if out.timed_out {
        remove_olean_artifacts(&olean_path);
        return Ok(ModuleResult {
            module: module.to_string(),
            ok: false,
            cached: false,
            seconds,
            diagnostics: vec![Diagnostic {
                file: default_rel,
                line: 1,
                col: 1,
                severity: "error".to_string(),
                kind: "timeout".to_string(),
                text: format!("timed out after {timeout_s}s"),
            }],
        });
    }

    let diags = parse_lean_output(
        &out.stdout_lossy(),
        &out.stderr_lossy(),
        out.status,
        tree,
        &ctx.campaign.root,
        &default_rel,
    );
    let has_error = diags.iter().any(|d| d.severity == "error");
    let ok = out.status == Some(0) && !has_error && olean_path.is_file();

    if ok {
        fs::write(&key_path, format!("{key}\n")).map_err(|e| VdashError::io_path(&key_path, e))?;
        let cd = CachedDiags {
            version: 1,
            diagnostics: diags.clone(),
        };
        jsonio::write_atomic(&diag_path, &cd)?;
    } else {
        remove_olean_artifacts(&olean_path);
    }

    Ok(ModuleResult {
        module: module.to_string(),
        ok,
        cached: false,
        seconds,
        diagnostics: diags,
    })
}

pub fn build(
    ctx: &Ctx,
    tree: &Path,
    build_dir: &Path,
    targets: Option<&[String]>,
) -> Result<BuildReport> {
    let full_graph = imports::import_graph(tree)?;
    let graph = if let Some(ts) = targets {
        for t in ts {
            if !full_graph.contains_key(t) {
                return Err(VdashError::new(
                    1,
                    format!("target module '{t}' does not exist in {}", tree.display()),
                ));
            }
        }
        let needed = imports::closure_of(&full_graph, ts);
        full_graph
            .into_iter()
            .filter(|(k, _)| needed.contains(k))
            .collect()
    } else {
        full_graph
    };

    let levels = imports::topo_levels(&graph)?;
    let tc = lean::toolchain(&ctx.cfg)?;
    let lp = lean::lean_path(ctx, build_dir)?;

    let mut results: BTreeMap<String, ModuleResult> = BTreeMap::new();
    let mut module_keys: BTreeMap<String, String> = BTreeMap::new();
    let mut root_failure: BTreeMap<String, String> = BTreeMap::new();

    for level in levels {
        let mut to_compile: Vec<(String, String)> = Vec::new();

        for m in level {
            let deps = graph.get(&m).cloned().unwrap_or_default();
            let failed_dep = deps
                .iter()
                .find(|d| results.get(*d).map(|r| !r.ok).unwrap_or(true));

            if let Some(fd) = failed_dep {
                let root_failed = root_failure
                    .get(fd)
                    .cloned()
                    .unwrap_or_else(|| fd.clone());
                let olean_path = module_olean_path(build_dir, &m);
                remove_olean_artifacts(&olean_path);
                let default_rel = format!("lean/{}.lean", module_rel_stem(&m).display());
                root_failure.insert(m.clone(), root_failed.clone());
                results.insert(
                    m.clone(),
                    ModuleResult {
                        module: m,
                        ok: false,
                        cached: false,
                        seconds: 0.0,
                        diagnostics: vec![Diagnostic {
                            file: default_rel,
                            line: 1,
                            col: 1,
                            severity: "error".to_string(),
                            kind: "depFailed".to_string(),
                            text: format!("dependency {root_failed} failed"),
                        }],
                    },
                );
            } else {
                let src_path = module_src_path(tree, &m);
                let src_bytes =
                    fs::read(&src_path).map_err(|e| VdashError::io_path(&src_path, e))?;
                let key = compute_cache_key(&src_bytes, &deps, &module_keys, &tc, &lp);
                module_keys.insert(m.clone(), key.clone());
                to_compile.push((m, key));
            }
        }

        if !to_compile.is_empty() {
            let level_results: Vec<Result<ModuleResult>> = std::thread::scope(|s| {
                let mut handles = Vec::with_capacity(to_compile.len());
                for (m, key) in &to_compile {
                    let lp_ref = lp.as_str();
                    handles.push(s.spawn(move || {
                        compile_single_module(ctx, tree, build_dir, lp_ref, m, key)
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err(VdashError::new(5, "worker thread panicked")))
                    })
                    .collect()
            });

            for res in level_results {
                let mr = res?;
                if !mr.ok {
                    root_failure.insert(mr.module.clone(), mr.module.clone());
                }
                results.insert(mr.module.clone(), mr);
            }
        }
    }

    let ok = results.values().all(|r| r.ok);
    Ok(BuildReport { results, ok })
}

pub fn truncate_text(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let prefix: String = text.chars().take(max_chars).collect();
    format!("{prefix}… [{} more characters]", total - max_chars)
}

pub fn format_capped_diagnostics(diags: &[Diagnostic]) -> Vec<String> {
    let mut ordered: Vec<&Diagnostic> = Vec::with_capacity(diags.len());
    for d in diags.iter().filter(|d| d.severity == "error") {
        ordered.push(d);
    }
    for d in diags.iter().filter(|d| d.severity == "warning") {
        ordered.push(d);
    }
    for d in diags
        .iter()
        .filter(|d| d.severity != "error" && d.severity != "warning")
    {
        ordered.push(d);
    }

    let mut lines = Vec::new();
    for d in ordered.iter().take(60) {
        let msg = truncate_text(&d.text, 1500);
        lines.push(format!(
            "{}:{}:{}: {}: {}",
            d.file, d.line, d.col, d.severity, msg
        ));
    }
    if ordered.len() > 60 {
        lines.push(format!("… [{} more diagnostics]", ordered.len() - 60));
    }
    lines
}

pub fn check_file(ctx: &Ctx, file_arg: &Path, cwd: &Path) -> Result<(String, bool)> {
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

    let rep = build(
        ctx,
        &ctx.campaign.lean_dir(),
        &ctx.campaign.build_work_dir(),
        Some(std::slice::from_ref(&mod_name)),
    )?;

    let target_res = rep.results.get(&mod_name);
    let diags: Vec<Diagnostic> = match target_res {
        Some(tr) if tr.ok || tr.diagnostics.iter().all(|d| d.kind != "depFailed") => {
            tr.diagnostics.clone()
        }
        _ => {
            let mut all = Vec::new();
            for r in rep.results.values() {
                all.extend(r.diagnostics.clone());
            }
            all
        }
    };

    let mut lines = format_capped_diagnostics(&diags);
    if !rep.ok {
        let err_count = diags
            .iter()
            .filter(|d| d.severity == "error")
            .count()
            .max(1);
        lines.push(format!("FAILED ({err_count} errors)"));
    } else {
        let sorry_count = diags
            .iter()
            .filter(|d| {
                d.kind == "hasSorry" || (d.severity == "warning" && d.text.contains("sorry"))
            })
            .count();
        if sorry_count > 0 {
            lines.push(format!("OK ({sorry_count} sorry)"));
        } else {
            lines.push("OK".to_string());
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');
    Ok((out, rep.ok))
}
