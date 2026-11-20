use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::error::{VdashError, Result};
use crate::jsonio;
use crate::paths::Ctx;
use crate::util;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentEntry {
    pub n: u32,
    pub record: String,
    pub record_sha256: String,
    pub manifest_hash_after: String,
    pub approved_by: String,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    pub version: u32,
    pub campaign_id: String,
    #[serde(default)]
    pub campaign_path: String,
    pub baseline_commit: String,
    pub manifest: BTreeMap<String, String>,
    pub manifest_hash: String,
    pub amendments: Vec<AmendmentEntry>,
}

pub fn anchor_path(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("anchor.json")
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

fn canonical_str(p: &Path) -> String {
    fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .display()
        .to_string()
}

pub fn collect_heldout_files(heldout_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !heldout_dir.is_dir() {
        return files;
    }
    for entry in WalkDir::new(heldout_dir)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_file() {
            if let Ok(rel) = entry.path().strip_prefix(heldout_dir) {
                files.push(rel.to_path_buf());
            }
        }
    }
    files.sort();
    files
}

pub fn manifest(root: &Path, files: &[PathBuf]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for rel in files {
        let full = root.join(rel);
        let bytes = fs::read(&full).map_err(|e| VdashError::io_path(&full, e))?;
        out.insert(rel.display().to_string(), sha256_bytes(&bytes));
    }
    Ok(out)
}

pub fn manifest_hash(manifest: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (rel, sha) in manifest {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(sha.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

pub fn create(ctx: &Ctx, baseline_commit: &str) -> Result<Anchor> {
    let apath = anchor_path(ctx);
    if apath.exists() {
        return Err(VdashError::new(
            2,
            format!(
                "anchor already exists at {} (delete it deliberately to re-approve)",
                apath.display()
            ),
        ));
    }

    let heldout_dir = ctx.campaign.heldout_dir();
    let held_files = collect_heldout_files(&heldout_dir);
    let m = if !held_files.is_empty() {
        manifest(&heldout_dir, &held_files)?
    } else {
        let trusted = ctx.trusted_files();
        manifest(&ctx.campaign.root, &trusted)?
    };
    let mh = manifest_hash(&m);

    let anc = Anchor {
        version: 1,
        campaign_id: ctx.cfg.id.clone(),
        campaign_path: canonical_str(&ctx.campaign.root),
        baseline_commit: baseline_commit.to_string(),
        manifest: m,
        manifest_hash: mh,
        amendments: Vec::new(),
    };
    jsonio::write_atomic(&apath, &anc)?;
    Ok(anc)
}

pub fn verify(ctx: &Ctx) -> Vec<String> {
    let mut problems = Vec::new();
    let apath = anchor_path(ctx);
    if !apath.is_file() {
        problems.push(format!("anchor missing at {}", apath.display()));
        return problems;
    }

    let anc: Anchor = match jsonio::read_json(&apath) {
        Ok(a) => a,
        Err(e) => {
            problems.push(format!("invalid anchor.json: {}", e.message));
            return problems;
        }
    };

    if anc.version != 1 {
        problems.push(format!("unsupported anchor version {}", anc.version));
    }
    if anc.campaign_id != ctx.cfg.id {
        problems.push(format!(
            "campaign_id differs: anchor has '{}', config has '{}'",
            anc.campaign_id, ctx.cfg.id
        ));
    }

    let computed_mh = manifest_hash(&anc.manifest);
    if computed_mh != anc.manifest_hash {
        problems.push("anchor manifest_hash does not match anchor manifest".to_string());
    }

    let heldout_dir = ctx.campaign.heldout_dir();
    let actual_files = collect_heldout_files(&heldout_dir);
    let actual_set: BTreeSet<String> = actual_files
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    for (rel, expected_sha) in &anc.manifest {
        if !actual_set.contains(rel) {
            problems.push(format!("file removed from .heldout/: {rel}"));
        } else {
            let full = heldout_dir.join(rel);
            match fs::read(&full) {
                Ok(bytes) => {
                    let got_sha = sha256_bytes(&bytes);
                    if &got_sha != expected_sha {
                        problems.push(format!("file changed in .heldout/: {rel}"));
                    }
                }
                Err(e) => {
                    problems.push(format!("cannot read .heldout/{rel}: {e}"));
                }
            }
        }
    }

    for rel in &actual_set {
        if !anc.manifest.contains_key(rel) {
            problems.push(format!("file added in .heldout/: {rel}"));
        }
    }

    for am in &anc.amendments {
        let rec_path = ctx.campaign.root.join(&am.record);
        match fs::read(&rec_path) {
            Ok(bytes) => {
                let got_sha = sha256_bytes(&bytes);
                if got_sha != am.record_sha256 {
                    problems.push(format!(
                        "amendment record sha256 mismatch for {}",
                        am.record
                    ));
                }
            }
            Err(_) => {
                problems.push(format!("amendment record missing: {}", am.record));
            }
        }
    }

    if let Some(last) = anc.amendments.last() {
        if last.manifest_hash_after != anc.manifest_hash {
            problems.push(
                "last amendment manifest_hash_after does not match anchor manifest_hash"
                    .to_string(),
            );
        }
    }

    problems
}

pub fn extend(ctx: &Ctx, n: u32, record: &str, approved_by: &str) -> Result<Anchor> {
    let apath = anchor_path(ctx);
    let mut anc: Anchor = jsonio::read_json(&apath)?;
    if anc.version != 1 {
        return Err(VdashError::new(
            2,
            format!("unsupported anchor version {}", anc.version),
        ));
    }

    let rec_path = ctx.campaign.root.join(record);
    let rec_bytes = fs::read(&rec_path).map_err(|e| VdashError::io_path(&rec_path, e))?;
    let record_sha256 = sha256_bytes(&rec_bytes);

    let heldout_dir = ctx.campaign.heldout_dir();
    let held_files = collect_heldout_files(&heldout_dir);
    let new_manifest = manifest(&heldout_dir, &held_files)?;
    let new_mh = manifest_hash(&new_manifest);

    anc.manifest = new_manifest;
    anc.manifest_hash = new_mh.clone();
    anc.amendments.push(AmendmentEntry {
        n,
        record: record.to_string(),
        record_sha256,
        manifest_hash_after: new_mh,
        approved_by: approved_by.to_string(),
        at: util::now_iso(),
    });

    jsonio::write_atomic(&apath, &anc)?;
    Ok(anc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::paths;
    use std::fs;
    use tempfile::TempDir;

    fn setup_heldout_campaign(tmp: &TempDir) -> Ctx {
        let host = tmp.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();

        let camp_dir = tmp.path().join("camp");
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        paths::init_campaign(&camp_dir, &host, Some("isort"), &[BackendKind::Claude]).unwrap();
        let ctx = Ctx::for_test(&camp_dir, &state_dir).unwrap();

        for rel in ctx.campaign.trusted_files() {
            let src = ctx.campaign.root.join(&rel);
            let dst = ctx.campaign.heldout_dir().join(&rel);
            fs::create_dir_all(dst.parent().unwrap()).unwrap();
            fs::copy(src, dst).unwrap();
        }
        ctx
    }

    #[test]
    fn create_verify_tamper_overwrite_and_extend() {
        let tmp = TempDir::new().unwrap();
        let ctx = setup_heldout_campaign(&tmp);

        // 0. Before create -> verify reports anchor missing
        let probs = verify(&ctx);
        assert!(!probs.is_empty());
        assert!(probs.iter().any(|p| p.contains("missing")));

        // 1. Create -> verify is clean
        let anc = create(&ctx, "deadbeef1234567890").unwrap();
        assert_eq!(anc.version, 1);
        assert_eq!(anc.manifest.len(), 5);
        assert!(
            verify(&ctx).is_empty(),
            "expected clean verify, got {:?}",
            verify(&ctx)
        );

        // 2. Overwrite is refused with code 2
        let err = create(&ctx, "othercommit").unwrap_err();
        assert_eq!(err.code, 2);

        // 3. File changed in .heldout/
        let impl_held = ctx.campaign.heldout_dir().join("lean/Apc/Spec/Impl.lean");
        let orig_impl = fs::read_to_string(&impl_held).unwrap();
        fs::write(&impl_held, "-- tampered\n").unwrap();
        let probs = verify(&ctx);
        assert!(probs
            .iter()
            .any(|p| p.contains("changed") && p.contains("Impl.lean")));
        fs::write(&impl_held, &orig_impl).unwrap();
        assert!(verify(&ctx).is_empty());

        // 4. File added in .heldout/
        let extra_held = ctx.campaign.heldout_dir().join("lean/Apc/Spec/Extra.lean");
        fs::write(&extra_held, "-- extra\n").unwrap();
        let probs = verify(&ctx);
        assert!(probs
            .iter()
            .any(|p| p.contains("added") && p.contains("Extra.lean")));
        fs::remove_file(&extra_held).unwrap();
        assert!(verify(&ctx).is_empty());

        // 5. File removed from .heldout/
        fs::remove_file(&impl_held).unwrap();
        let probs = verify(&ctx);
        assert!(probs
            .iter()
            .any(|p| p.contains("removed") && p.contains("Impl.lean")));
        fs::write(&impl_held, &orig_impl).unwrap();
        assert!(verify(&ctx).is_empty());

        // 6. Moving the campaign directory still verifies cleanly, while campaign_id mismatch fails
        let moved_dir = tmp.path().join("moved_camp");
        fs::rename(&ctx.campaign.root, &moved_dir).unwrap();
        let mut moved_ctx = ctx.clone();
        moved_ctx.campaign = paths::Campaign::new(&moved_dir);
        assert!(
            verify(&moved_ctx).is_empty(),
            "expected clean verify after moving campaign, got {:?}",
            verify(&moved_ctx)
        );
        fs::rename(&moved_dir, &ctx.campaign.root).unwrap();

        let mut wrong_id_ctx = ctx.clone();
        wrong_id_ctx.cfg.id = "different_id".to_string();
        let probs = verify(&wrong_id_ctx);
        assert!(probs.iter().any(|p| p.contains("campaign_id")));

        // 7. extend with amendment record, then tamper with amendment record
        let rec_rel = "amendments/001-fix.md";
        let rec_path = ctx.campaign.root.join(rec_rel);
        fs::write(&rec_path, "# Amendment 1\n").unwrap();
        fs::write(&impl_held, format!("{orig_impl}\n-- amended\n")).unwrap();
        let anc2 = extend(&ctx, 1, rec_rel, "user").unwrap();
        assert_eq!(anc2.amendments.len(), 1);
        assert!(verify(&ctx).is_empty());

        fs::write(&rec_path, "# Tampered amendment 1\n").unwrap();
        let probs = verify(&ctx);
        assert!(probs.iter().any(|p| p.contains("amendment")));
    }
}
