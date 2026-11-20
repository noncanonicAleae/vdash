use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::{self, BackendKind, Config, Role};
use crate::error::{VdashError, Result};
use crate::jsonio;
use crate::proc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Campaign {
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Ctx {
    pub campaign: Campaign,
    pub cfg: Config,
    pub state_dir: PathBuf,
    pub exe: PathBuf,
    /// The user's model catalog cache (`models.json`, ROADMAP M8-08), shared by
    /// every campaign.
    pub models_cache: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetState {
    pub accepted: bool,
    pub reasons: Vec<String>,
    pub since_round: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalState {
    pub status: String,
    pub lean_name: String,
    pub file: String,
    pub lead: Option<String>,
    pub verified_round: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeadState {
    pub goal_ids: Vec<String>,
    pub summary: String,
    pub last_round: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendUsage {
    pub calls: u64,
    pub cost_usd: f64,
    pub seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    pub phase: String,
    pub round: u32,
    pub stage: String,
    pub targets: BTreeMap<String, TargetState>,
    pub goals: BTreeMap<String, GoalState>,
    pub leads: BTreeMap<String, LeadState>,
    pub round_data: serde_json::Value,
    pub pending_guidance: Vec<String>,
    pub module_times: BTreeMap<String, f64>,
    pub usage: BTreeMap<String, BTreeMap<String, BackendUsage>>,
}

impl State {
    pub fn new_init() -> Self {
        Self {
            version: 1,
            phase: "init".to_string(),
            round: 0,
            stage: "idle".to_string(),
            targets: BTreeMap::new(),
            goals: BTreeMap::new(),
            leads: BTreeMap::new(),
            round_data: serde_json::json!({}),
            pending_guidance: Vec::new(),
            module_times: BTreeMap::new(),
            usage: BTreeMap::new(),
        }
    }
}

impl Campaign {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("campaign.toml")
    }

    pub fn sources_dir(&self) -> PathBuf {
        self.root.join("sources")
    }

    pub fn brief_file(&self) -> PathBuf {
        self.root.join("sources/BRIEF.md")
    }

    pub fn strategy_file(&self) -> PathBuf {
        self.root.join("STRATEGY.md")
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    pub fn spec_dir(&self) -> PathBuf {
        self.root.join("spec")
    }

    pub fn departures_file(&self) -> PathBuf {
        self.root.join("spec/DEPARTURES.md")
    }

    pub fn review_file(&self, n: u32) -> PathBuf {
        self.root.join(format!("spec/review-{n}.md"))
    }

    pub fn lean_dir(&self) -> PathBuf {
        self.root.join("lean")
    }

    pub fn apc_dir(&self) -> PathBuf {
        self.root.join("lean/Apc")
    }

    pub fn lean_spec_dir(&self) -> PathBuf {
        self.root.join("lean/Apc/Spec")
    }

    pub fn challenge_file(&self) -> PathBuf {
        self.root.join("lean/Apc/Challenge.lean")
    }

    pub fn solution_file(&self) -> PathBuf {
        self.root.join("lean/Apc/Solution.lean")
    }

    pub fn proof_dir(&self) -> PathBuf {
        self.root.join("lean/Apc/Proof")
    }

    pub fn proof_lib_dir(&self) -> PathBuf {
        self.root.join("lean/Apc/Proof/Lib")
    }

    pub fn proof_work_dir(&self) -> PathBuf {
        self.root.join("lean/Apc/Proof/Work")
    }

    pub fn heldout_dir(&self) -> PathBuf {
        self.root.join(".heldout")
    }

    pub fn heldout_tree(&self) -> PathBuf {
        self.root.join(".heldout/lean")
    }

    pub fn build_dir(&self) -> PathBuf {
        self.root.join(".build")
    }

    pub fn build_work_dir(&self) -> PathBuf {
        self.root.join(".build/work")
    }

    pub fn build_gate_dir(&self) -> PathBuf {
        self.root.join(".build/gate")
    }

    pub fn build_slots_dir(&self) -> PathBuf {
        self.root.join(".build/slots")
    }

    pub fn lean_path_cache(&self) -> PathBuf {
        self.root.join(".build/lean_path.json")
    }

    pub fn plan_file(&self) -> PathBuf {
        self.root.join("plan.json")
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn bulletin_file(&self) -> PathBuf {
        self.root.join("bulletin.md")
    }

    pub fn leads_dir(&self) -> PathBuf {
        self.root.join("leads")
    }

    pub fn lead_notes_file(&self, id: &str) -> PathBuf {
        self.root.join(format!("leads/{id}.md"))
    }

    pub fn amendments_dir(&self) -> PathBuf {
        self.root.join("amendments")
    }

    pub fn escalations_dir(&self) -> PathBuf {
        self.root.join("escalations")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn events_file(&self) -> PathBuf {
        self.root.join("logs/events.jsonl")
    }

    pub fn status_log_file(&self) -> PathBuf {
        self.root.join("logs/status.md")
    }

    pub fn engine_out_file(&self) -> PathBuf {
        self.root.join("logs/engine.out")
    }

    pub fn round_logs_dir(&self, round: u32) -> PathBuf {
        self.root.join(format!("logs/{round}"))
    }

    pub fn module_name(&self, path: impl AsRef<Path>) -> Result<String> {
        let p = path.as_ref();
        let rel = if p.is_absolute() {
            p.strip_prefix(&self.root).map_err(|_| {
                VdashError::new(
                    2,
                    format!("path '{}' is outside campaign", p.display()),
                )
            })?
        } else {
            p
        };
        module_name(rel)
    }

    pub fn module_path(&self, name: &str) -> PathBuf {
        module_path(name)
    }

    pub fn trusted_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let challenge = self.challenge_file();
        if challenge.is_file() {
            files.push(PathBuf::from("lean/Apc/Challenge.lean"));
        }
        let spec_dir = self.lean_spec_dir();
        if spec_dir.is_dir() {
            for entry in WalkDir::new(&spec_dir)
                .follow_links(false)
                .into_iter()
                .flatten()
            {
                if entry.file_type().is_file() {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("lean") {
                        if let Ok(rel) = p.strip_prefix(&self.root) {
                            files.push(rel.to_path_buf());
                        }
                    }
                }
            }
        }
        files.sort();
        files
    }
}

pub fn module_name(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    let under_lean = path.strip_prefix("lean").map_err(|_| {
        VdashError::new(
            2,
            format!("path '{}' is outside lean/", path.display()),
        )
    })?;
    if under_lean.extension().and_then(|e| e.to_str()) != Some("lean") {
        return Err(VdashError::new(
            2,
            format!("path '{}' is not a .lean file", path.display()),
        ));
    }
    let without_ext = under_lean.with_extension("");
    let mut parts = Vec::new();
    for comp in without_ext.components() {
        match comp {
            Component::Normal(os) => {
                let s = os.to_str().ok_or_else(|| {
                    VdashError::new(2, format!("non-utf8 path '{}'", path.display()))
                })?;
                if s.is_empty() {
                    return Err(VdashError::new(
                        2,
                        format!("invalid module path '{}'", path.display()),
                    ));
                }
                parts.push(s);
            }
            _ => {
                return Err(VdashError::new(
                    2,
                    format!("invalid path component in '{}'", path.display()),
                ))
            }
        }
    }
    if parts.is_empty() {
        return Err(VdashError::new(
            2,
            format!("empty module name for '{}'", path.display()),
        ));
    }
    Ok(parts.join("."))
}

pub fn module_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from("lean");
    for part in name.split('.') {
        p.push(part);
    }
    p.set_extension("lean");
    p
}

pub fn find_campaign(
    explicit: Option<&Path>,
    env_value: Option<&str>,
    cwd: &Path,
) -> Result<Campaign> {
    if let Some(dir) = explicit {
        if dir.join("campaign.toml").is_file() {
            return Ok(Campaign::new(dir));
        }
        return Err(VdashError::new(
            2,
            format!("no campaign.toml found in {}", dir.display()),
        ));
    }
    if let Some(env_val) = env_value {
        if !env_val.is_empty() {
            let dir = PathBuf::from(env_val);
            if dir.join("campaign.toml").is_file() {
                return Ok(Campaign::new(dir));
            }
            return Err(VdashError::new(
                2,
                format!("no campaign.toml found in {}", dir.display()),
            ));
        }
    }
    let mut cur: &Path = cwd;
    loop {
        if cur.join("campaign.toml").is_file() {
            return Ok(Campaign::new(cur));
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => {
                return Err(VdashError::new(
                    2,
                    "no campaign found (pass -C <dir>, set VDASH_CAMPAIGN, or run inside a campaign)",
                ))
            }
        }
    }
}

fn generate_campaign_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| VdashError::new(5, format!("getrandom failed: {e}")))?;
    let mut s = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    Ok(s)
}

pub const ISORT_SPEC_IMPL: &str = include_str!("../examples/isort/lean/Apc/Spec/Impl.lean");
pub const ISORT_SPEC_DEFS: &str = include_str!("../examples/isort/lean/Apc/Spec/Defs.lean");
pub const ISORT_SPEC_MUTANTS: &str = include_str!("../examples/isort/lean/Apc/Spec/Mutants.lean");
pub const ISORT_SPEC_TESTS: &str = include_str!("../examples/isort/lean/Apc/Spec/Tests.lean");
pub const ISORT_CHALLENGE: &str = include_str!("../examples/isort/lean/Apc/Challenge.lean");
pub const ISORT_REFERENCE_SOLUTION: &str =
    include_str!("../examples/isort/reference/Solution.lean");

pub fn init_campaign(
    dir: impl AsRef<Path>,
    host: impl AsRef<Path>,
    example: Option<&str>,
    available_backends: &[BackendKind],
) -> Result<Option<String>> {
    let dir = dir.as_ref();
    let host = host.as_ref();

    let is_isort = match example {
        None => false,
        Some("isort") => true,
        Some(ex) => return Err(VdashError::new(2, format!("unknown example '{ex}'"))),
    };

    if !host.is_dir() {
        return Err(VdashError::new(
            2,
            format!("host_project '{}' is not a directory", host.display()),
        ));
    }
    if !host.join("lean-toolchain").is_file() {
        return Err(VdashError::new(
            2,
            format!(
                "host_project '{}' does not contain lean-toolchain",
                host.display()
            ),
        ));
    }
    let abs_host = fs::canonicalize(host).unwrap_or_else(|_| host.to_path_buf());

    if dir.exists() {
        if !dir.is_dir() {
            return Err(VdashError::new(
                2,
                format!("'{}' exists and is not a directory", dir.display()),
            ));
        }
        let mut entries = fs::read_dir(dir).map_err(|e| VdashError::io_path(dir, e))?;
        if entries.next().is_some() {
            return Err(VdashError::new(
                2,
                format!("directory '{}' is not empty", dir.display()),
            ));
        }
    }

    let camp = Campaign::new(dir);
    for d in [
        camp.sources_dir(),
        camp.skills_dir(),
        camp.spec_dir(),
        camp.lean_spec_dir(),
        camp.proof_lib_dir(),
        camp.proof_work_dir(),
        camp.heldout_tree(),
        camp.build_work_dir(),
        camp.build_gate_dir(),
        camp.build_slots_dir(),
        camp.leads_dir(),
        camp.amendments_dir(),
        camp.escalations_dir(),
        camp.logs_dir(),
    ] {
        fs::create_dir_all(&d).map_err(|e| VdashError::io_path(&d, e))?;
    }

    if is_isort {
        for (rel, content) in [
            ("lean/Apc/Spec/Impl.lean", ISORT_SPEC_IMPL),
            ("lean/Apc/Spec/Defs.lean", ISORT_SPEC_DEFS),
            ("lean/Apc/Spec/Mutants.lean", ISORT_SPEC_MUTANTS),
            ("lean/Apc/Spec/Tests.lean", ISORT_SPEC_TESTS),
            ("lean/Apc/Challenge.lean", ISORT_CHALLENGE),
        ] {
            let p = camp.root.join(rel);
            fs::write(&p, content).map_err(|e| VdashError::io_path(&p, e))?;
        }
    }

    let id = generate_campaign_id()?;
    let mut toml_text = format!(
        "id = \"{id}\"\nhost_project = \"{}\"\n",
        abs_host.display().to_string().replace('\\', "\\\\").replace('"', "\\\"")
    );
    if is_isort {
        toml_text.push_str("use_mathlib = false\n");
    }

    let single_backend = match (
        available_backends.contains(&BackendKind::Claude),
        available_backends.contains(&BackendKind::Gemini),
    ) {
        (true, false) => Some(BackendKind::Claude),
        (false, true) => Some(BackendKind::Gemini),
        _ => None,
    };

    let notice = if let Some(b) = single_backend {
        toml_text.push_str("\n[roles]\n");
        for role in Role::ALL {
            toml_text.push_str(&format!("{role} = \"{b}\"\n"));
        }
        Some(format!(
            "only backend '{b}' found; assigned every role in [roles] to '{b}'"
        ))
    } else {
        None
    };

    fs::write(camp.config_file(), toml_text)
        .map_err(|e| VdashError::io_path(camp.config_file(), e))?;

    let state = State::new_init();
    jsonio::write_atomic(camp.state_file(), &state)?;

    let gitignore_path = camp.root.join(".gitignore");
    fs::write(&gitignore_path, ".build/\nlogs/\n")
        .map_err(|e| VdashError::io_path(&gitignore_path, e))?;

    let mut git_cmd = Command::new("git");
    git_cmd.args(["init", "-q"]).current_dir(&camp.root);
    let out = proc::run(&mut git_cmd, None, Duration::from_secs(10))?;
    if out.timed_out || out.status != Some(0) {
        return Err(VdashError::new(
            5,
            format!("git init failed: {}", out.stderr_lossy().trim()),
        ));
    }

    Ok(notice)
}

pub fn doctor(cfg: &Config) -> (String, bool) {
    use std::collections::BTreeSet;

    let mut lines = Vec::new();
    let mut all_ok = true;
    let timeout = Duration::from_secs(10);

    // 1. git
    let mut git_cmd = Command::new("git");
    git_cmd.arg("--version");
    match proc::run(&mut git_cmd, None, timeout) {
        Ok(out) if !out.timed_out && out.status == Some(0) => {
            lines.push(format!("git: ok ({})", out.stdout_lossy().trim()));
        }
        _ => {
            all_ok = false;
            lines.push("git: FAIL (install git and ensure it is on PATH)".to_string());
        }
    }

    // 2. lean under pinned toolchain
    let tc_path = cfg.host_project.join("lean-toolchain");
    let toolchain = fs::read_to_string(&tc_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let expected_ver = toolchain
        .split(':')
        .next_back()
        .unwrap_or(&toolchain)
        .trim_start_matches('v');

    if toolchain.is_empty() {
        all_ok = false;
        lines.push(format!(
            "lean: FAIL (empty or unreadable {})",
            tc_path.display()
        ));
    } else {
        let mut lean_cmd = Command::new("lean");
        lean_cmd
            .arg("--version")
            .env("ELAN_TOOLCHAIN", &toolchain);
        match proc::run(&mut lean_cmd, None, timeout) {
            Ok(out)
                if !out.timed_out
                    && out.status == Some(0)
                    && out.stdout_lossy().contains(expected_ver) =>
            {
                lines.push(format!(
                    "lean ({toolchain}): ok ({})",
                    out.stdout_lossy().lines().next().unwrap_or("").trim()
                ));
            }
            Ok(out) => {
                all_ok = false;
                let msg = out.stderr_lossy();
                let hint = if msg.trim().is_empty() {
                    out.stdout_lossy().trim().to_string()
                } else {
                    msg.trim().to_string()
                };
                lines.push(format!(
                    "lean ({toolchain}): FAIL (expected version '{expected_ver}'; got '{hint}')"
                ));
            }
            Err(e) => {
                all_ok = false;
                lines.push(format!(
                    "lean ({toolchain}): FAIL ({}; ensure elan/lean is on PATH)",
                    e.message
                ));
            }
        }
    }

    // 3. lake env printenv LEAN_PATH when use_mathlib
    if cfg.use_mathlib {
        let mut lake_cmd = Command::new("lake");
        lake_cmd
            .args(["env", "printenv", "LEAN_PATH"])
            .current_dir(&cfg.host_project)
            .env("ELAN_TOOLCHAIN", &toolchain);
        match proc::run(&mut lake_cmd, None, timeout) {
            Ok(out)
                if !out.timed_out
                    && out.status == Some(0)
                    && !out.stdout_lossy().trim().is_empty() =>
            {
                lines.push("lake (LEAN_PATH): ok".to_string());
            }
            _ => {
                all_ok = false;
                lines.push(format!(
                    "lake (LEAN_PATH): FAIL (run 'lake build' in {})",
                    cfg.host_project.display()
                ));
            }
        }
    }

    // 4. leanchecker when kernel_recheck (F7: leanchecker has no --version/--help and
    // replays all modules when invoked without args; check $(lean --print-prefix)/bin/leanchecker)
    if cfg.policy.kernel_recheck {
        let mut pfx_cmd = Command::new("lean");
        pfx_cmd
            .arg("--print-prefix")
            .env("ELAN_TOOLCHAIN", &toolchain);
        let lc_ok = match proc::run(&mut pfx_cmd, None, Duration::from_secs(5)) {
            Ok(out) if !out.timed_out && out.status == Some(0) => {
                let prefix = out.stdout_lossy().trim().to_string();
                !prefix.is_empty() && PathBuf::from(prefix).join("bin/leanchecker").is_file()
            }
            _ => false,
        };
        if lc_ok {
            lines.push("leanchecker: ok".to_string());
        } else {
            all_ok = false;
            lines.push(
                "leanchecker: FAIL (ensure leanchecker is available in the pinned toolchain)"
                    .to_string(),
            );
        }
    }

    // 5. Backends used by roles
    let mut used_backends = BTreeSet::new();
    for entries in cfg.roles.values() {
        for e in entries {
            used_backends.insert(e.backend);
        }
    }

    for kind in used_backends {
        let bcfg = cfg.backend_cfg(kind);
        let prog = &bcfg.command[0];
        let extra = &bcfg.command[1..];
        let mut vcmd = Command::new(prog);
        vcmd.args(extra).arg("--version");
        match proc::run(&mut vcmd, None, timeout) {
            Ok(out) if !out.timed_out && out.status == Some(0) => match kind {
                BackendKind::Claude => {
                    lines.push(format!("{kind} ({prog}): ok (login: not checked)"));
                }
                BackendKind::Gemini => {
                    let mut mcmd = Command::new(prog);
                    mcmd.args(extra).arg("models");
                    if cfg.use_subscription {
                        mcmd.env_remove("GEMINI_API_KEY")
                            .env_remove("GOOGLE_API_KEY");
                    }
                    match proc::run(&mut mcmd, None, timeout) {
                        Ok(mout) if !mout.timed_out && mout.status == Some(0) => {
                            lines.push(format!("{kind} ({prog}): ok (logged in)"));
                        }
                        _ => {
                            all_ok = false;
                            lines.push(format!(
                                "{kind} ({prog}): FAIL ('{prog} models' failed; log in to {prog})"
                            ));
                        }
                    }
                }
            },
            _ => {
                all_ok = false;
                lines.push(format!(
                    "{kind} ({prog}): FAIL (command '{prog}' failed; check installation or [backends.{kind}])"
                ));
            }
        }
    }

    let mut report = lines.join("\n");
    report.push('\n');
    (report, all_ok)
}

impl Ctx {
    pub fn for_test(campaign_dir: impl AsRef<Path>, state_dir: impl AsRef<Path>) -> Result<Self> {
        let campaign = Campaign::new(campaign_dir.as_ref());
        let cfg = config::load(&campaign.root)?;
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("vdash"));
        Ok(Self {
            campaign,
            cfg,
            state_dir: state_dir.as_ref().to_path_buf(),
            exe,
            models_cache: state_dir.as_ref().join("models.json"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn module_name_and_path_round_trip() {
        let camp = Campaign::new("/tmp/my_camp");
        let p = camp.module_path("Apc.Spec.Impl");
        assert_eq!(p, PathBuf::from("lean/Apc/Spec/Impl.lean"));
        assert_eq!(camp.module_name(&p).unwrap(), "Apc.Spec.Impl");
        assert_eq!(
            camp.module_name("/tmp/my_camp/lean/Apc/Spec/Impl.lean")
                .unwrap(),
            "Apc.Spec.Impl"
        );

        let err = camp.module_name("sources/BRIEF.md").unwrap_err();
        assert_eq!(err.code, 2);
        let err = camp.module_name("/other/dir/Foo.lean").unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn trusted_files_sorted_on_sample_tree() {
        let tmp = TempDir::new().unwrap();
        let camp = Campaign::new(tmp.path());
        fs::create_dir_all(tmp.path().join("lean/Apc/Spec/Sub")).unwrap();
        fs::create_dir_all(tmp.path().join("lean/Apc/Proof")).unwrap();
        fs::write(tmp.path().join("lean/Apc/Challenge.lean"), "").unwrap();
        fs::write(tmp.path().join("lean/Apc/Spec/Impl.lean"), "").unwrap();
        fs::write(tmp.path().join("lean/Apc/Spec/Defs.lean"), "").unwrap();
        fs::write(tmp.path().join("lean/Apc/Spec/Sub/Extra.lean"), "").unwrap();
        fs::write(tmp.path().join("lean/Apc/Spec/notes.txt"), "").unwrap();
        fs::write(tmp.path().join("lean/Apc/Solution.lean"), "").unwrap();
        fs::write(tmp.path().join("lean/Apc/Proof/Work.lean"), "").unwrap();

        let trusted = camp.trusted_files();
        assert_eq!(
            trusted,
            vec![
                PathBuf::from("lean/Apc/Challenge.lean"),
                PathBuf::from("lean/Apc/Spec/Defs.lean"),
                PathBuf::from("lean/Apc/Spec/Impl.lean"),
                PathBuf::from("lean/Apc/Spec/Sub/Extra.lean"),
            ]
        );
    }

    #[test]
    fn find_campaign_lookup_rules() {
        let tmp = TempDir::new().unwrap();
        let c1 = tmp.path().join("c1");
        let c2 = tmp.path().join("c2");
        let c3 = tmp.path().join("c3");
        let sub = c3.join("lean/Apc/Spec");
        fs::create_dir_all(&c1).unwrap();
        fs::create_dir_all(&c2).unwrap();
        fs::create_dir_all(&sub).unwrap();
        fs::write(c1.join("campaign.toml"), "").unwrap();
        fs::write(c2.join("campaign.toml"), "").unwrap();
        fs::write(c3.join("campaign.toml"), "").unwrap();

        // 1. Explicit -C wins over env and cwd
        let found = find_campaign(Some(&c1), Some(c2.to_str().unwrap()), &sub).unwrap();
        assert_eq!(found.root, c1);

        // 2. Env wins over cwd when explicit is None
        let found = find_campaign(None, Some(c2.to_str().unwrap()), &sub).unwrap();
        assert_eq!(found.root, c2);

        // 3. Walk up from cwd
        let found = find_campaign(None, None, &sub).unwrap();
        assert_eq!(found.root, c3);

        // 4. Outside any campaign -> code 2
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let err = find_campaign(None, None, &outside).unwrap_err();
        assert_eq!(err.code, 2);
    }

    #[test]
    fn init_with_single_backend_assigns_all_roles() {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();

        let camp_dir = tmp.path().join("single_backend_camp");
        let notice = init_campaign(&camp_dir, &host, None, &[BackendKind::Claude])
            .unwrap()
            .expect("expected single-backend notice");
        assert!(notice.contains("claude"));

        let cfg = config::load(&camp_dir).unwrap();
        for role in Role::ALL {
            assert_eq!(cfg.entries_for(role)[0].backend, BackendKind::Claude);
            assert_eq!(cfg.entries_for(role).len(), 1);
        }
    }
}
