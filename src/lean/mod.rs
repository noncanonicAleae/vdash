pub mod build;
pub mod imports;
pub mod slots;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{VdashError, Result};
use crate::jsonio;
use crate::paths::Ctx;
use crate::proc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeanPathCache {
    version: u32,
    #[serde(default)]
    host_project: String,
    manifest_mtime_ns: u128,
    lean_path: String,
}

pub fn toolchain(cfg: &Config) -> Result<String> {
    let tc_path = cfg.host_project.join("lean-toolchain");
    let raw = fs::read_to_string(&tc_path).map_err(|e| {
        VdashError::new(
            2,
            format!("cannot read {}: {e}", tc_path.display()),
        )
    })?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err(VdashError::new(
            2,
            format!("empty lean-toolchain in {}", tc_path.display()),
        ));
    }
    Ok(trimmed)
}

pub fn command(ctx: &Ctx, program: &str) -> Result<Command> {
    let tc = toolchain(&ctx.cfg)?;
    let mut cmd = Command::new(program);
    cmd.env("ELAN_TOOLCHAIN", tc);
    Ok(cmd)
}

fn manifest_mtime_ns(host_project: &Path) -> u128 {
    let p = host_project.join("lake-manifest.json");
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub fn lean_path(ctx: &Ctx, build_dir: &Path) -> Result<String> {
    let build_str = build_dir.display().to_string();
    if !ctx.cfg.use_mathlib {
        return Ok(build_str);
    }

    let host_key = ctx.cfg.host_project.display().to_string();
    let mtime = manifest_mtime_ns(&ctx.cfg.host_project);
    let cache_path = ctx.campaign.lean_path_cache();
    if cache_path.is_file() {
        if let Ok(cached) = jsonio::read_json::<LeanPathCache>(&cache_path) {
            if cached.version == 1
                && cached.host_project == host_key
                && cached.manifest_mtime_ns == mtime
            {
                return Ok(format!("{build_str}:{}", cached.lean_path));
            }
        }
    }

    let mut cmd = command(ctx, "lake")?;
    cmd.args(["env", "printenv", "LEAN_PATH"])
        .current_dir(&ctx.cfg.host_project);
    let out = proc::run(&mut cmd, None, Duration::from_secs(30))?;
    if out.timed_out || out.status != Some(0) {
        let stderr = out.stderr_lossy();
        let detail = if stderr.trim().is_empty() {
            out.stdout_lossy().trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        return Err(VdashError::new(
            2,
            format!("lake env printenv LEAN_PATH failed: {detail}"),
        ));
    }

    let host_lp = out.stdout_lossy().trim().to_string();
    let cache = LeanPathCache {
        version: 1,
        host_project: host_key,
        manifest_mtime_ns: mtime,
        lean_path: host_lp.clone(),
    };
    jsonio::write_atomic(&cache_path, &cache)?;

    Ok(format!("{build_str}:{host_lp}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::paths;
    use tempfile::TempDir;

    #[test]
    fn lean_path_cache_respects_host_project_and_mtime() {
        let tmp = TempDir::new().unwrap();
        let host1 = tmp.path().join("host1");
        let host2 = tmp.path().join("host2");
        fs::create_dir_all(&host1).unwrap();
        fs::create_dir_all(&host2).unwrap();
        fs::write(host1.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
        fs::write(host2.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();

        let camp = tmp.path().join("camp");
        paths::init_campaign(&camp, &host1, None, &[BackendKind::Claude]).unwrap();
        let mut ctx = Ctx::for_test(&camp, tmp.path().join("state")).unwrap();
        let build_dir = ctx.campaign.build_work_dir();
        fs::create_dir_all(build_dir.parent().unwrap()).unwrap();

        let cache = LeanPathCache {
            version: 1,
            host_project: host1.display().to_string(),
            manifest_mtime_ns: 0,
            lean_path: "/mock/mathlib".to_string(),
        };
        jsonio::write_atomic(ctx.campaign.lean_path_cache(), &cache).unwrap();

        let lp = lean_path(&ctx, &build_dir).unwrap();
        assert_eq!(lp, format!("{}:/mock/mathlib", build_dir.display()));

        // Switching host_project invalidates the cache even when both have mtime 0,
        // forcing a fresh `lake env printenv LEAN_PATH` lookup. Without lake that errors;
        // with lake it succeeds with the real path. Either way the cached entry is not used.
        ctx.cfg.host_project = host2;
        if let Ok(lp) = lean_path(&ctx, &build_dir) {
            assert!(!lp.contains("/mock/mathlib"), "stale cache entry reused: {lp}");
        }
    }
}
