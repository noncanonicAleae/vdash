use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustix::fs::{flock, FlockOperation};
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
    /// Reruns after an infrastructure error (M3-09).
    #[serde(default)]
    pub retries: u32,
}

/// An error caused by the build machinery rather than by the Lean source (M3-09): a
/// dependency's olean vanished or is unreadable, or Lean was killed by a signal.
pub fn is_infra_text(text: &str) -> bool {
    let t = text.to_lowercase();
    (t.contains("object file") && t.contains("does not exist"))
        || (t.contains(".olean") && (t.contains("failed to read file") || t.contains("invalid header") || t.contains("incompatible header")))
        || t.starts_with("lean exited with status none")
}

/// Does module result `r` fail with infrastructure errors only?
pub fn infra_failure(r: &ModuleResult) -> bool {
    if r.ok {
        return false;
    }
    let errs: Vec<&Diagnostic> = r.diagnostics.iter().filter(|d| d.severity == "error").collect();
    !errs.is_empty() && errs.iter().all(|d| is_infra_text(&d.text))
}

/// Is a `vdash check` report failing only because of infrastructure errors? Lines
/// `dependency M failed` follow from another error and are neutral.
pub fn report_is_infra(report: &str) -> bool {
    let errs: Vec<&str> = report.lines().filter(|l| l.contains(": error: ")).collect();
    let mut any = false;
    for l in errs {
        let text = l.split_once(": error: ").map(|x| x.1).unwrap_or(l);
        if is_infra_text(text) {
            any = true;
        } else if !(text.starts_with("dependency ") && text.ends_with(" failed")) {
            return false;
        }
    }
    any
}

/// Reruns of a build that hit an infrastructure error.
pub const INFRA_RETRIES: u32 = 2;

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

fn olean_lock_path(olean: &Path) -> PathBuf {
    let mut os = olean.as_os_str().to_os_string();
    os.push(".lock");
    PathBuf::from(os)
}

/// A fresh output path next to `olean`, unique to this process and call.
fn olean_tmp_path(olean: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut os = olean.as_os_str().to_os_string();
    os.push(format!(".{}-{n}.tmp", std::process::id()));
    PathBuf::from(os)
}

/// Exclusive lock on one module's cache entry, held across the cache check and the
/// compile. Several builds share one build directory (parallel workers' checks, in
/// one process or several); without it they compiled the same module at once and
/// deleted each other's oleans while a dependent was reading them.
struct ModuleLock {
    file: fs::File,
}

impl Drop for ModuleLock {
    fn drop(&mut self) {
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

fn lock_module(olean: &Path) -> Result<ModuleLock> {
    if let Some(parent) = olean.parent() {
        fs::create_dir_all(parent).map_err(|e| VdashError::io_path(parent, e))?;
    }
    let lock_path = olean_lock_path(olean);
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| VdashError::io_path(&lock_path, e))?;
    flock(&file, FlockOperation::LockExclusive)
        .map_err(|e| VdashError::new(5, format!("cannot lock {}: {e}", lock_path.display())))?;
    Ok(ModuleLock { file })
}

/// Mark a module's cache entry invalid without deleting its olean. Another build
/// may be compiling a dependent against that olean right now (it saw the module
/// succeed); deleting it made that compile fail with "object file … does not
/// exist". A keyless olean is never a cache hit, and no build compiles against a
/// module that failed in the same build, so the stale file is never used.
fn invalidate_cache_entry(olean: &Path) {
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

    // Held until return. Lock order is module lock, then slot; a slot holder never
    // waits for a module lock, so the two cannot deadlock.
    let _lock = lock_module(&olean_path)?;
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

    // Compile to a private path and rename into place, so a concurrent reader
    // sees either the old olean or the complete new one, never a gap.
    let tmp_path = olean_tmp_path(&olean_path);
    let mut cmd = lean::command(ctx, "lean")?;
    cmd.env("LEAN_PATH", lp)
        .arg("--json")
        .arg("-R")
        .arg(tree)
        .arg("-o")
        .arg(&tmp_path)
        .arg(&src_path);

    let timeout_s = ctx.cfg.budget.lean_timeout_s;
    let start = Instant::now();
    let run = proc::run(&mut cmd, None, Duration::from_secs(timeout_s));
    let seconds = start.elapsed().as_secs_f64();
    let out = match run {
        Ok(out) => out,
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
    };

    if out.timed_out {
        let _ = fs::remove_file(&tmp_path);
        invalidate_cache_entry(&olean_path);
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
    let ok = out.status == Some(0) && !has_error && tmp_path.is_file();

    if ok {
        // Drop the old key first: a crash between the rename and the key write
        // leaves a keyless olean, which is rebuilt next time.
        invalidate_cache_entry(&olean_path);
        if let Err(e) = fs::rename(&tmp_path, &olean_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(VdashError::io_path(&olean_path, e));
        }
        let cd = CachedDiags {
            version: 1,
            diagnostics: diags.clone(),
        };
        jsonio::write_atomic(&diag_path, &cd)?;
        fs::write(&key_path, format!("{key}\n")).map_err(|e| VdashError::io_path(&key_path, e))?;
    } else {
        let _ = fs::remove_file(&tmp_path);
        invalidate_cache_entry(&olean_path);
    }

    Ok(ModuleResult {
        module: module.to_string(),
        ok,
        cached: false,
        seconds,
        diagnostics: diags,
    })
}

fn scope_graph_for_targets(
    tree: &Path,
    scan: imports::ImportScan,
    targets: Option<&[String]>,
) -> Result<BTreeMap<String, Vec<String>>> {
    if let Some(ts) = targets {
        for t in ts {
            if !scan.graph.contains_key(t) {
                return Err(VdashError::new(
                    1,
                    format!("target module '{t}' does not exist in {}", tree.display()),
                ));
            }
        }
        let needed = imports::closure_of(&scan.graph, ts);
        let missing_msgs: Vec<String> = scan
            .missing
            .iter()
            .filter(|(m, _, _)| needed.contains(m))
            .map(|(_, dep, rel)| format!("{} imports missing module {dep}", rel.display()))
            .collect();
        if !missing_msgs.is_empty() {
            return Err(VdashError::new(1, missing_msgs.join("\n")));
        }
        Ok(scan
            .graph
            .into_iter()
            .filter(|(k, _)| needed.contains(k))
            .collect())
    } else {
        if !scan.missing.is_empty() {
            let missing_msgs: Vec<String> = scan
                .missing
                .iter()
                .map(|(_, dep, rel)| format!("{} imports missing module {dep}", rel.display()))
                .collect();
            return Err(VdashError::new(1, missing_msgs.join("\n")));
        }
        Ok(scan.graph)
    }
}

/// Build `targets` (or everything) in `tree` into `build_dir`. A build where some
/// module failed with an infrastructure error only is rerun, up to
/// [`INFRA_RETRIES`] times; cached modules stay cached, so a rerun rebuilds only
/// what is missing (M3-09).
pub fn build(
    ctx: &Ctx,
    tree: &Path,
    build_dir: &Path,
    targets: Option<&[String]>,
) -> Result<BuildReport> {
    build_with_keys(ctx, tree, build_dir, targets).map(|(rep, _)| rep)
}

/// Same as [`build`], also returning each built module's cache key.
pub fn build_with_keys(
    ctx: &Ctx,
    tree: &Path,
    build_dir: &Path,
    targets: Option<&[String]>,
) -> Result<(BuildReport, BTreeMap<String, String>)> {
    let (mut rep, mut keys) = build_once(ctx, tree, build_dir, targets)?;
    while rep.retries < INFRA_RETRIES && rep.results.values().any(infra_failure) {
        let retries = rep.retries + 1;
        std::thread::sleep(Duration::from_millis(300));
        let next = build_once(ctx, tree, build_dir, targets)?;
        rep = next.0;
        keys = next.1;
        rep.retries = retries;
    }
    Ok((rep, keys))
}

fn build_once(
    ctx: &Ctx,
    tree: &Path,
    build_dir: &Path,
    targets: Option<&[String]>,
) -> Result<(BuildReport, BTreeMap<String, String>)> {
    let scan = imports::scan_imports_pkg(tree, ctx.pkg())?;
    let graph = scope_graph_for_targets(tree, scan, targets)?;

    // Validates acyclicity and keeps the exact import cycle diagnostic.
    let _levels = imports::topo_levels(&graph)?;
    let tc = lean::toolchain(&ctx.cfg)?;
    let lp = lean::lean_path(ctx, build_dir)?;

    let mut results: BTreeMap<String, ModuleResult> = BTreeMap::new();
    let mut module_keys: BTreeMap<String, String> = BTreeMap::new();
    let mut root_failure: BTreeMap<String, String> = BTreeMap::new();

    if graph.is_empty() {
        return Ok((BuildReport { results, ok: true, retries: 0 }, module_keys));
    }

    let mut rdeps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut remaining_deps: BTreeMap<String, usize> = BTreeMap::new();
    let mut ready: std::collections::VecDeque<String> = std::collections::VecDeque::new();

    for (m, deps) in &graph {
        rdeps.entry(m.clone()).or_default();
        let mut count = 0usize;
        for d in deps {
            if graph.contains_key(d) {
                count += 1;
                rdeps.entry(d.clone()).or_default().push(m.clone());
            }
        }
        remaining_deps.insert(m.clone(), count);
        if count == 0 {
            ready.push_back(m.clone());
        }
    }

    let mut first_err: Option<VdashError> = None;
    let lp_ref = lp.as_str();

    std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::channel::<Result<ModuleResult>>();
        let mut in_flight = 0usize;

        while (!ready.is_empty() && first_err.is_none()) || in_flight > 0 {
            while first_err.is_none() {
                let Some(m) = ready.pop_front() else {
                    break;
                };
                let deps = graph.get(&m).map(Vec::as_slice).unwrap_or(&[]);
                let failed_dep = deps
                    .iter()
                    .find(|d| results.get(*d).map(|r| !r.ok).unwrap_or(true));

                if let Some(fd) = failed_dep {
                    let root_failed = root_failure
                        .get(fd)
                        .cloned()
                        .unwrap_or_else(|| fd.clone());
                    let olean_path = module_olean_path(build_dir, &m);
                    match lock_module(&olean_path) {
                        Ok(_lock) => invalidate_cache_entry(&olean_path),
                        Err(e) => {
                            first_err = Some(e);
                            break;
                        }
                    }
                    let default_rel = format!("lean/{}.lean", module_rel_stem(&m).display());
                    root_failure.insert(m.clone(), root_failed.clone());
                    results.insert(
                        m.clone(),
                        ModuleResult {
                            module: m.clone(),
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
                    if let Some(children) = rdeps.get(&m) {
                        for child in children {
                            if let Some(rem) = remaining_deps.get_mut(child) {
                                *rem = rem.saturating_sub(1);
                                if *rem == 0 {
                                    ready.push_back(child.clone());
                                }
                            }
                        }
                    }
                } else {
                    let src_path = module_src_path(tree, &m);
                    let src_bytes = match fs::read(&src_path) {
                        Ok(b) => b,
                        Err(e) => {
                            first_err = Some(VdashError::io_path(&src_path, e));
                            break;
                        }
                    };
                    let key = compute_cache_key(&src_bytes, deps, &module_keys, &tc, &lp);
                    module_keys.insert(m.clone(), key.clone());
                    let tx_c = tx.clone();
                    s.spawn(move || {
                        struct PanicNotify(Option<std::sync::mpsc::Sender<Result<ModuleResult>>>);
                        impl Drop for PanicNotify {
                            fn drop(&mut self) {
                                if let Some(tx) = self.0.take() {
                                    let _ = tx.send(Err(VdashError::new(5, "worker thread panicked")));
                                }
                            }
                        }
                        let mut guard = PanicNotify(Some(tx_c));
                        let res = compile_single_module(ctx, tree, build_dir, lp_ref, &m, &key);
                        if let Some(tx) = guard.0.take() {
                            let _ = tx.send(res);
                        }
                    });
                    in_flight += 1;
                }
            }

            if in_flight > 0 {
                match rx.recv() {
                    Ok(Ok(mr)) => {
                        in_flight -= 1;
                        let m_name = mr.module.clone();
                        if !mr.ok {
                            root_failure.insert(m_name.clone(), m_name.clone());
                            module_keys.remove(&m_name);
                        }
                        results.insert(m_name.clone(), mr);
                        if let Some(children) = rdeps.get(&m_name) {
                            for child in children {
                                if let Some(rem) = remaining_deps.get_mut(child) {
                                    *rem = rem.saturating_sub(1);
                                    if *rem == 0 {
                                        ready.push_back(child.clone());
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        in_flight -= 1;
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                    Err(_) => {
                        in_flight -= 1;
                        if first_err.is_none() {
                            first_err = Some(VdashError::new(5, "worker thread panicked"));
                        }
                    }
                }
            }
        }
    });

    if let Some(e) = first_err {
        return Err(e);
    }

    let ok = results.values().all(|r| r.ok);
    Ok((BuildReport { results, ok, retries: 0 }, module_keys))
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
    let (out, ok, _) = check_file_report(ctx, file_arg, cwd)?;
    Ok((out, ok))
}

pub fn check_file_report(ctx: &Ctx, file_arg: &Path, cwd: &Path) -> Result<(String, bool, BuildReport)> {
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
    let ok = rep.ok;
    Ok((out, ok, rep))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn diag(sev: &str, text: &str) -> Diagnostic {
        Diagnostic { file: "lean/Apc/X.lean".into(), line: 1, col: 0, severity: sev.into(), kind: String::new(), text: text.into() }
    }

    #[test]
    fn infra_classifier() {
        let missing = "object file '/c/.build/work/Apc/Proof/Lib/QSpec.olean' of module Apc.Proof.Lib.QSpec does not exist";
        assert!(is_infra_text(missing));
        assert!(is_infra_text("failed to read file '/c/.build/work/Apc/A.olean', invalid header"));
        assert!(is_infra_text("lean exited with status None"));
        assert!(!is_infra_text("unknown identifier 'foo'"));
        assert!(!is_infra_text("No goals to be solved"));
        let r = |ok, d: Vec<Diagnostic>| ModuleResult { module: "Apc.X".into(), ok, cached: false, seconds: 0.0, diagnostics: d };
        assert!(infra_failure(&r(false, vec![diag("error", missing), diag("warning", "declaration uses 'sorry'")])));
        assert!(!infra_failure(&r(false, vec![diag("error", missing), diag("error", "type mismatch")])));
        assert!(!infra_failure(&r(true, vec![])));
        assert!(!infra_failure(&r(false, vec![])));
        // The report the quartic workers got (round 6).
        let report = format!(
            "lean/Apc/Proof/Work/AndOr.lean:1:1: error: dependency Apc.Proof.Work.Separation failed\nlean/Apc/Proof/Work/Separation.lean:1:0: error: {missing}\nFAILED (2 errors)\n"
        );
        assert!(report_is_infra(&report));
        assert!(!report_is_infra("lean/Apc/X.lean:99:4: error: No goals to be solved\nFAILED (1 errors)\n"));
        assert!(!report_is_infra(&format!("{report}lean/Apc/Y.lean:3:1: error: unknown constant\n")));
        assert!(!report_is_infra("OK\n"));
    }

    #[test]
    fn scope_graph_ignores_missing_imports_outside_target_closure() {
        let tmp = TempDir::new().unwrap();
        let tree = tmp.path();
        fs::create_dir_all(tree.join("Apc/Proof/Work")).unwrap();
        fs::write(tree.join("Apc/Proof/Work/Good.lean"), "-- clean\n").unwrap();
        fs::write(
            tree.join("Apc/Proof/Work/Bad.lean"),
            "import Apc.Proof.Work.DoesNotExist\n",
        )
        .unwrap();

        let scan = imports::scan_imports(tree).unwrap();
        // Full build still rejects Bad.lean's missing import.
        let full_err = scope_graph_for_targets(tree, scan.clone(), None).unwrap_err();
        assert_eq!(full_err.code, 1);
        assert!(full_err.message.contains("DoesNotExist"));

        // Building Good.lean ignores Bad.lean's missing import completely.
        let good_graph =
            scope_graph_for_targets(tree, scan.clone(), Some(&["Apc.Proof.Work.Good".into()]))
                .unwrap();
        assert_eq!(good_graph.len(), 1);
        assert!(good_graph.contains_key("Apc.Proof.Work.Good"));

        // Building Bad.lean reports Bad.lean's own missing import.
        let bad_err =
            scope_graph_for_targets(tree, scan, Some(&["Apc.Proof.Work.Bad".into()])).unwrap_err();
        assert_eq!(bad_err.code, 1);
        assert!(bad_err.message.contains("DoesNotExist"));
    }
}
