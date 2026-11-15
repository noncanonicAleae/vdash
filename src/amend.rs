//! Amendments and escalations (ROADMAP M6-01, M6-02, M6-03; DESIGN §10):
//! `propose`, `process`, `apply`, `approve_cmd`, `reject_cmd`, and `list_cmd`.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agents::{self, Outcome, Request};
use crate::anchor;
use crate::backend::pool::Pool;
use crate::backend::Profile;
use crate::config::{AmendmentPolicy, Role};
use crate::error::{VdashError, Result};
use crate::events::{self, Control};
use crate::fingerprint;
use crate::lean;
use crate::paths::Ctx;
use crate::schemas::Reply;
use crate::util;

const META_START: &str = "<!-- apc-amendment-json\n";
const META_END: &str = "\n-->";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AmendOutcome {
    /// Amendment numbers applied to the spec this round.
    pub applied: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub file: String,
    pub old_text: String,
    pub new_text: String,
    pub justification: String,
    pub evidence_kind: String,
    #[serde(default)]
    pub evidence_file: Option<String>,
    #[serde(default)]
    pub evidence_lean_name: Option<String>,
    pub alternatives_tried: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmendmentRecord {
    pub n: u32,
    pub slug: String,
    pub status: String,
    pub approved_by: String,
    pub file: String,
    pub old_text: String,
    pub new_text: String,
    pub justification: String,
    pub evidence_kind: String,
    pub evidence_file: Option<String>,
    pub evidence_lean_name: Option<String>,
    pub alternatives_tried: String,
    pub evidence_verified: bool,
    pub evidence_statement: Option<String>,
    pub critic_verdict: String,
    pub touches_trusted_assumptions: bool,
    pub closer_to_code: bool,
    pub critic_reason: String,
}

impl AmendmentRecord {
    pub fn rel_path(&self) -> String {
        format!("amendments/{:03}-{}.md", self.n, self.slug)
    }

    pub fn escalation_rel_path(&self) -> String {
        format!("escalations/{:03}-{}.md", self.n, self.slug)
    }

    pub fn to_markdown(&self) -> String {
        let meta = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        let mut md = String::new();
        md.push_str(&format!("# Amendment {:03}: {}\n\n", self.n, self.slug));
        md.push_str(&format!("- **Status**: `{}`\n", self.status));
        md.push_str(&format!("- **Critic verdict**: `{}`\n", self.critic_verdict));
        md.push_str(&format!(
            "- **Touches trusted assumptions**: `{}`\n",
            self.touches_trusted_assumptions
        ));
        md.push_str(&format!("- **Closer to code**: `{}`\n", self.closer_to_code));
        md.push_str(&format!("- **Approved by**: `{}`\n", self.approved_by));
        md.push_str(&format!("- **File**: `{}`\n\n", self.file));
        md.push_str("## Justification\n\n");
        md.push_str(self.justification.trim());
        md.push_str("\n\n## Alternatives Tried\n\n");
        md.push_str(self.alternatives_tried.trim());
        md.push_str("\n\n## Proposed Edit\n\n### Old Text\n```lean\n");
        md.push_str(self.old_text.trim_end());
        md.push_str("\n```\n\n### New Text\n```lean\n");
        md.push_str(self.new_text.trim_end());
        md.push_str("\n```\n\n## Evidence\n\n");
        md.push_str(&format!("- **Kind**: `{}`\n", self.evidence_kind));
        md.push_str(&format!("- **Verified**: `{}`\n", self.evidence_verified));
        if let Some(stmt) = &self.evidence_statement {
            md.push_str(&format!("- **Statement**:\n```lean\n{}\n```\n", stmt.trim()));
        }
        md.push_str("\n## Critic Reason\n\n");
        md.push_str(self.critic_reason.trim());
        md.push_str("\n\n");
        md.push_str(META_START);
        md.push_str(&meta);
        md.push_str(META_END);
        md.push('\n');
        md
    }

    pub fn from_markdown(text: &str) -> Option<Self> {
        let start = text.find(META_START)? + META_START.len();
        let rest = &text[start..];
        let end = rest.find(META_END)?;
        serde_json::from_str(&rest[..end]).ok()
    }
}

fn make_slug(file: &str, justification: &str) -> String {
    let stem = std::path::Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("spec");
    let raw = format!("{stem}-{}", justification.lines().next().unwrap_or("edit"));
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= 32 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "amend".to_string()
    } else {
        trimmed
    }
}

pub fn list_records(ctx: &Ctx) -> Result<Vec<AmendmentRecord>> {
    let dir = ctx.campaign.amendments_dir();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| VdashError::io_path(&dir, e))? {
        let entry = entry.map_err(|e| VdashError::io_path(&dir, e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&path) {
            if let Some(rec) = AmendmentRecord::from_markdown(&text) {
                records.push(rec);
            }
        }
    }
    records.sort_by_key(|r| r.n);
    Ok(records)
}

pub fn load_record(ctx: &Ctx, n: u32) -> Result<AmendmentRecord> {
    let records = list_records(ctx)?;
    records
        .into_iter()
        .find(|r| r.n == n)
        .ok_or_else(|| VdashError::new(2, format!("amendment {:03} not found", n)))
}

fn save_record(ctx: &Ctx, rec: &AmendmentRecord) -> Result<PathBuf> {
    let dir = ctx.campaign.amendments_dir();
    fs::create_dir_all(&dir).map_err(|e| VdashError::io_path(&dir, e))?;
    let path = ctx.campaign.root.join(rec.rel_path());
    fs::write(&path, rec.to_markdown()).map_err(|e| VdashError::io_path(&path, e))?;
    Ok(path)
}

fn save_escalation(ctx: &Ctx, rec: &AmendmentRecord) -> Result<PathBuf> {
    let dir = ctx.campaign.escalations_dir();
    fs::create_dir_all(&dir).map_err(|e| VdashError::io_path(&dir, e))?;
    let path = ctx.campaign.root.join(rec.escalation_rel_path());
    let mut md = String::new();
    md.push_str(&format!("# Escalation {:03}: {}\n\n", rec.n, rec.slug));
    md.push_str(&format!("- **Amendment**: `{}`\n", rec.rel_path()));
    md.push_str(&format!("- **Target file**: `{}`\n", rec.file));
    md.push_str(&format!("- **Critic reason**: {}\n\n", rec.critic_reason));
    md.push_str("## Justification\n\n");
    md.push_str(rec.justification.trim());
    md.push_str("\n\n## Proposed Edit\n\n### Old Text\n```lean\n");
    md.push_str(rec.old_text.trim_end());
    md.push_str("\n```\n\n### New Text\n```lean\n");
    md.push_str(rec.new_text.trim_end());
    md.push_str("\n```\n");
    fs::write(&path, md).map_err(|e| VdashError::io_path(&path, e))?;
    Ok(path)
}

fn next_amendment_number(ctx: &Ctx) -> Result<u32> {
    let records = list_records(ctx)?;
    Ok(records.last().map(|r| r.n + 1).unwrap_or(1))
}

fn collect_trusted_spec_inline(ctx: &Ctx) -> String {
    let mut out = String::new();
    let dep = ctx.campaign.departures_file();
    if let Ok(text) = fs::read_to_string(&dep) {
        out.push_str("### spec/DEPARTURES.md\n");
        out.push_str(&text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    for rel in ctx.campaign.trusted_files() {
        let full = ctx.campaign.root.join(&rel);
        if let Ok(text) = fs::read_to_string(&full) {
            out.push_str(&format!("### {}\n", rel.display()));
            out.push_str(&text);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
    }
    out
}

fn verify_disproof_evidence(
    ctx: &Ctx,
    evidence_file: Option<&str>,
    evidence_lean_name: Option<&str>,
) -> Option<String> {
    let ev_file = evidence_file?.trim();
    let ev_name = evidence_lean_name?.trim();
    if ev_file.is_empty() || ev_name.is_empty() {
        return None;
    }
    let full = ctx.campaign.root.join(ev_file);
    if !full.is_file() {
        return None;
    }
    let mod_name = ctx.campaign.module_name(ev_file).ok()?;
    let tree = ctx.campaign.lean_dir();
    let build_dir = ctx.campaign.build_work_dir();
    let rep = lean::build::build(ctx, &tree, &build_dir, Some(std::slice::from_ref(&mod_name))).ok()?;
    if !rep.ok {
        return None;
    }
    let names = vec![ev_name.to_string()];
    let ax = fingerprint::axioms(ctx, &tree, &build_dir, &mod_name, &names).ok()?;
    let info = ax.get(ev_name)?;
    if !info.exists
        || info.kind != "theorem"
        || !info.axioms.iter().all(|a| fingerprint::is_standard_axiom(a))
    {
        return None;
    }
    match info.statement.as_deref() {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => Some(format!("theorem {ev_name}")),
    }
}

/// Evaluate and record one amendment proposal (M6-02).
/// Returns `(record, applied_now)`.
pub fn propose(
    ctx: &Ctx,
    pool: &Pool,
    round: u32,
    raw_proposal: &Value,
) -> Result<(AmendmentRecord, bool)> {
    let prop: Proposal = serde_json::from_value(raw_proposal.clone())
        .map_err(|e| VdashError::new(2, format!("invalid amendment proposal JSON: {e}")))?;

    let n = next_amendment_number(ctx)?;
    let slug = make_slug(&prop.file, &prop.justification);

    let mut rec = AmendmentRecord {
        n,
        slug,
        status: "rejected".to_string(),
        approved_by: "none".to_string(),
        file: prop.file.clone(),
        old_text: prop.old_text.clone(),
        new_text: prop.new_text.clone(),
        justification: prop.justification.clone(),
        evidence_kind: prop.evidence_kind.clone(),
        evidence_file: prop.evidence_file.clone(),
        evidence_lean_name: prop.evidence_lean_name.clone(),
        alternatives_tried: prop.alternatives_tried.clone(),
        evidence_verified: false,
        evidence_statement: None,
        critic_verdict: "reject".to_string(),
        touches_trusted_assumptions: false,
        closer_to_code: false,
        critic_reason: String::new(),
    };

    // Early rejections without calling the critic:
    // 1. `file` is not a trusted file
    let trusted = ctx.campaign.trusted_files();
    let prop_rel = PathBuf::from(&prop.file);
    if !trusted.contains(&prop_rel) {
        rec.critic_reason = format!("'{}' is not a trusted specification file", prop.file);
        save_record(ctx, &rec)?;
        events::emit(
            ctx,
            round,
            "amendment",
            json!({ "n": n, "status": rec.status }),
        );
        return Ok((rec, false));
    }

    // 2. `old_text` must occur exactly once in `file`
    let full_file = ctx.campaign.root.join(&prop_rel);
    let current_text = fs::read_to_string(&full_file).unwrap_or_default();
    if prop.old_text.is_empty() || current_text.matches(&prop.old_text).count() != 1 {
        rec.critic_reason = "old_text does not occur exactly once in target file".to_string();
        save_record(ctx, &rec)?;
        events::emit(
            ctx,
            round,
            "amendment",
            json!({ "n": n, "status": rec.status }),
        );
        return Ok((rec, false));
    }

    // 3. `new_text` adds more than 15 lines
    let old_lines = prop.old_text.lines().count();
    let new_lines = prop.new_text.lines().count();
    if new_lines > old_lines + 15 {
        rec.critic_reason = format!(
            "new_text adds more than 15 lines ({old_lines} -> {new_lines} lines)"
        );
        save_record(ctx, &rec)?;
        events::emit(
            ctx,
            round,
            "amendment",
            json!({ "n": n, "status": rec.status }),
        );
        return Ok((rec, false));
    }

    // 4. `evidence_kind: disproof` -> verify `evidence_lean_name` with `fingerprint::axioms`
    if prop.evidence_kind == "disproof" {
        match verify_disproof_evidence(
            ctx,
            prop.evidence_file.as_deref(),
            prop.evidence_lean_name.as_deref(),
        ) {
            Some(stmt) => {
                rec.evidence_verified = true;
                rec.evidence_statement = Some(stmt);
            }
            None => {
                rec.critic_reason = "evidence not machine-checked".to_string();
                save_record(ctx, &rec)?;
                events::emit(
                    ctx,
                    round,
                    "amendment",
                    json!({ "n": n, "status": rec.status }),
                );
                return Ok((rec, false));
            }
        }
    }

    // 5. Critic call (Profile::None) with ONLY trusted files inlined, before/after edit,
    // justification, alternatives_tried, and the verified evidence statement (not its proof).
    let trusted_spec = collect_trusted_spec_inline(ctx);
    let proposed_edit = format!(
        "Target file: {}\n<before>\n{}\n</before>\n<after>\n{}\n</after>",
        prop.file, prop.old_text, prop.new_text
    );
    let justification_sec = format!(
        "Justification:\n{}\n\nAlternatives tried:\n{}",
        prop.justification, prop.alternatives_tried
    );
    let evidence_sec = match (&prop.evidence_kind[..], &rec.evidence_statement) {
        ("disproof", Some(stmt)) => format!(
            "Evidence kind: machine-checked disproof ({})\nVerified statement:\n{}",
            prop.evidence_lean_name.as_deref().unwrap_or(""),
            stmt
        ),
        _ => format!("Evidence kind: {}", prop.evidence_kind),
    };

    let req = Request::new(
        Role::Critic,
        Reply::Critic,
        "Evaluate this proposed specification amendment against the criteria in your skill.",
    )
    .section("trusted_spec", trusted_spec)
    .section("proposed_edit", proposed_edit)
    .section("justification", justification_sec)
    .section("evidence", evidence_sec)
    .label(format!("amend-{n:03}"))
    .profile(Profile::None);

    let (_, outcome) = agents::run(ctx, pool, round, req)?;
    match outcome {
        Outcome::Ok(val) => {
            rec.critic_verdict = val
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("reject")
                .to_string();
            rec.touches_trusted_assumptions = val
                .get("touches_trusted_assumptions")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            rec.closer_to_code = val
                .get("closer_to_code")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.critic_reason = val
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        Outcome::Failed(err) => {
            rec.critic_verdict = "reject".to_string();
            rec.critic_reason = format!("critic call failed: {err}");
        }
    }

    match rec.critic_verdict.as_str() {
        "escalate" => {
            rec.status = "escalated".to_string();
            save_record(ctx, &rec)?;
            save_escalation(ctx, &rec)?;
            events::emit(
                ctx,
                round,
                "amendment",
                json!({ "n": n, "status": rec.status }),
            );
            events::emit(ctx, round, "escalation", json!({ "n": n }));
            Ok((rec, false))
        }
        "accept" => {
            let auto_apply = ctx.cfg.policy.amendment_policy == AmendmentPolicy::AutoNeutral
                && !rec.touches_trusted_assumptions
                && rec.closer_to_code;
            rec.status = "pending".to_string();
            save_record(ctx, &rec)?;
            if auto_apply {
                match apply(ctx, n, "auto_neutral") {
                    Ok(()) => {
                        let applied_rec = load_record(ctx, n)?;
                        Ok((applied_rec, true))
                    }
                    Err(e) => {
                        rec.status = "rejected".to_string();
                        rec.critic_reason =
                            format!("{} (auto_neutral apply failed: {})", rec.critic_reason, e.message);
                        save_record(ctx, &rec)?;
                        events::emit(
                            ctx,
                            round,
                            "amendment",
                            json!({ "n": n, "status": rec.status }),
                        );
                        Ok((rec, false))
                    }
                }
            } else {
                events::emit(
                    ctx,
                    round,
                    "amendment",
                    json!({ "n": n, "status": rec.status }),
                );
                Ok((rec, false))
            }
        }
        _ => {
            rec.status = "rejected".to_string();
            save_record(ctx, &rec)?;
            events::emit(
                ctx,
                round,
                "amendment",
                json!({ "n": n, "status": rec.status }),
            );
            Ok((rec, false))
        }
    }
}

/// Apply an amendment to the work tree and `.heldout/`, verify the build, extend the
/// anchor, reset targets and affected goals, and commit (M6-03).
pub fn apply(ctx: &Ctx, n: u32, approved_by: &str) -> Result<()> {
    let problems = anchor::verify(ctx);
    if !problems.is_empty() {
        return Err(VdashError::new(
            3,
            format!("anchor verification failed: {}", problems.join("; ")),
        ));
    }

    let mut rec = load_record(ctx, n)?;
    // A human may also approve an escalated proposal: escalation hands them the decision.
    if !matches!(rec.status.as_str(), "pending" | "approved" | "escalated") {
        return Err(VdashError::new(
            2,
            format!(
                "amendment {:03} cannot be applied (status is '{}', expected 'pending')",
                n, rec.status
            ),
        ));
    }

    let work_file = ctx.campaign.root.join(&rec.file);
    let held_file = ctx.campaign.heldout_dir().join(&rec.file);

    let orig_work = fs::read_to_string(&work_file).map_err(|e| VdashError::io_path(&work_file, e))?;
    let orig_held = fs::read_to_string(&held_file).map_err(|e| VdashError::io_path(&held_file, e))?;

    if orig_work.matches(&rec.old_text).count() != 1 {
        return Err(VdashError::new(
            1,
            format!(
                "amendment {:03}: old_text does not occur exactly once in work tree '{}'",
                n, rec.file
            ),
        ));
    }
    if orig_held.matches(&rec.old_text).count() != 1 {
        return Err(VdashError::new(
            1,
            format!(
                "amendment {:03}: old_text does not occur exactly once in held-out '{}'",
                n, rec.file
            ),
        ));
    }

    let new_work = orig_work.replacen(&rec.old_text, &rec.new_text, 1);
    let new_held = orig_held.replacen(&rec.old_text, &rec.new_text, 1);

    // Apply to work tree and temporarily-writable held-out copy
    fs::write(&work_file, &new_work).map_err(|e| VdashError::io_path(&work_file, e))?;
    util::set_readonly(&held_file, false)?;
    if let Err(e) = fs::write(&held_file, &new_held) {
        let _ = fs::write(&work_file, &orig_work);
        let _ = util::set_readonly(&held_file, true);
        return Err(VdashError::io_path(&held_file, e));
    }
    util::set_readonly(&held_file, true)?;

    // Rebuild the trusted tree; roll back both files if the build fails or introduces sorry in Spec/**
    let rollback = |work_file: &PathBuf,
                    held_file: &PathBuf,
                    orig_work: &str,
                    orig_held: &str| {
        let _ = fs::write(work_file, orig_work);
        let _ = util::set_readonly(held_file, false);
        let _ = fs::write(held_file, orig_held);
        let _ = util::set_readonly(held_file, true);
    };

    let trusted_rels = ctx.campaign.trusted_files();
    let mut trusted_mods = Vec::with_capacity(trusted_rels.len());
    for rel in &trusted_rels {
        trusted_mods.push(ctx.campaign.module_name(rel)?);
    }

    let tree = ctx.campaign.lean_dir();
    let build_dir = ctx.campaign.build_work_dir();
    match lean::build::build(ctx, &tree, &build_dir, Some(&trusted_mods)) {
        Ok(rep) => {
            let spec_has_sorry = rep.results.iter().any(|(m, mr)| {
                m != "Apc.Challenge"
                    && mr.diagnostics.iter().any(|d| {
                        d.kind == "hasSorry"
                            || d.text.contains("declaration uses 'sorry'")
                            || d.text.contains("declaration uses `sorry`")
                    })
            });
            if !rep.ok || spec_has_sorry {
                rollback(&work_file, &held_file, &orig_work, &orig_held);
                return Err(VdashError::new(
                    1,
                    format!(
                        "amendment {:03} broke the trusted specification build; rolled back",
                        n
                    ),
                ));
            }
        }
        Err(e) => {
            rollback(&work_file, &held_file, &orig_work, &orig_held);
            return Err(VdashError::new(
                1,
                format!(
                    "amendment {:03} failed to build trusted tree ({}); rolled back",
                    n, e.message
                ),
            ));
        }
    }

    // Update record on disk before anchor::extend hashes it
    rec.status = "applied".to_string();
    rec.approved_by = approved_by.to_string();
    save_record(ctx, &rec)?;

    let rel_record = rec.rel_path();
    anchor::extend(ctx, n, &rel_record, approved_by)?;

    // Determine which goals depend on the edited file
    let edited_mod = ctx.campaign.module_name(&rec.file).ok();
    let import_graph = lean::imports::import_graph(&tree).unwrap_or_default();

    let round = util::load_state(&ctx.campaign)
        .map(|s| s.round)
        .unwrap_or(0);

    util::update_state(&ctx.campaign, |s| {
        for t in s.targets.values_mut() {
            t.accepted = false;
            t.reasons = vec!["missing".to_string()];
            t.since_round = None;
        }
        for g in s.goals.values_mut() {
            let affected = match (&edited_mod, ctx.campaign.module_name(&g.file).ok()) {
                (Some(em), _) if em == "Apc.Challenge" => true,
                (Some(em), Some(gm)) => {
                    let closure = lean::imports::closure_of(&import_graph, std::slice::from_ref(&gm));
                    closure.contains(em) || !ctx.campaign.root.join(&g.file).is_file()
                }
                _ => true,
            };
            if affected {
                g.status = "open".to_string();
                g.verified_round = None;
            }
        }
    })?;

    util::commit_all(
        &ctx.campaign,
        &format!("amend: {:03} {}", n, rec.slug),
    )?;

    events::emit(
        ctx,
        round,
        "amendment",
        json!({ "n": n, "status": "applied" }),
    );
    Ok(())
}

/// Process this round's queued proposals and the user's decisions (M6-02).
pub fn process(
    ctx: &Ctx,
    pool: &Pool,
    round: u32,
    proposals: Vec<Value>,
    decisions: Vec<(u32, bool)>,
) -> Result<AmendOutcome> {
    let mut applied = Vec::new();

    for raw in &proposals {
        let (rec, did_apply) = propose(ctx, pool, round, raw)?;
        if did_apply {
            applied.push(rec.n);
        }
    }

    for (n, approve) in decisions {
        let Ok(mut rec) = load_record(ctx, n) else {
            continue;
        };
        if rec.status != "pending" && rec.status != "escalated" {
            continue;
        }
        if approve {
            if apply(ctx, n, "user").is_ok() {
                applied.push(n);
            }
        } else {
            rec.status = "rejected".to_string();
            rec.approved_by = "user".to_string();
            save_record(ctx, &rec)?;
            events::emit(
                ctx,
                round,
                "amendment",
                json!({ "n": n, "status": "rejected" }),
            );
        }
    }

    Ok(AmendOutcome { applied })
}

/// CLI `vdash amend approve <N>` (M6-03).
pub fn approve_cmd(ctx: &Ctx, n: u32) -> Result<String> {
    let rec = load_record(ctx, n)?;
    if rec.status != "pending" && rec.status != "escalated" {
        return Err(VdashError::new(
            2,
            format!(
                "amendment {:03} is '{}' (only pending or escalated amendments can be approved)",
                n, rec.status
            ),
        ));
    }
    if events::engine_running(ctx).is_some() {
        events::send(ctx, &Control::AmendDecision { n, approve: true })?;
        Ok(format!(
            "vdash: approval for amendment {:03} queued for the running engine\n",
            n
        ))
    } else {
        apply(ctx, n, "user")?;
        Ok(format!("vdash: amendment {:03} ({}) applied\n", n, rec.slug))
    }
}

/// CLI `vdash amend reject <N>` (M6-03).
pub fn reject_cmd(ctx: &Ctx, n: u32) -> Result<String> {
    let mut rec = load_record(ctx, n)?;
    if rec.status != "pending" {
        return Err(VdashError::new(
            2,
            format!(
                "amendment {:03} is '{}' (only 'pending' amendments can be rejected)",
                n, rec.status
            ),
        ));
    }
    if events::engine_running(ctx).is_some() {
        events::send(ctx, &Control::AmendDecision { n, approve: false })?;
        Ok(format!(
            "vdash: rejection for amendment {:03} queued for the running engine\n",
            n
        ))
    } else {
        rec.status = "rejected".to_string();
        rec.approved_by = "user".to_string();
        save_record(ctx, &rec)?;
        let round = util::load_state(&ctx.campaign)
            .map(|s| s.round)
            .unwrap_or(0);
        events::emit(
            ctx,
            round,
            "amendment",
            json!({ "n": n, "status": "rejected" }),
        );
        Ok(format!("vdash: amendment {:03} ({}) rejected\n", n, rec.slug))
    }
}

/// CLI `vdash amend propose <file.json>`: send one proposal (ROADMAP §3.7 shape) to
/// the critic now, as a human or a script would. Refused while an engine runs, so
/// it never races the engine's own amend stage.
pub fn propose_cmd(ctx: &Ctx, pool: &Pool, file: &std::path::Path) -> Result<String> {
    if let Some(pid) = events::engine_running(ctx) {
        return Err(VdashError::new(
            2,
            format!("an engine is running (pid {pid}); stop it first, or let the team propose amendments"),
        ));
    }
    let text = fs::read_to_string(file).map_err(|e| VdashError::io_path(file, e))?;
    let raw: Value = serde_json::from_str(&text)
        .map_err(|e| VdashError::new(2, format!("{}: invalid JSON: {e}", file.display())))?;
    let round = util::load_state(&ctx.campaign).map(|s| s.round).unwrap_or(0);
    let (rec, applied) = propose(ctx, pool, round, &raw)?;
    let mut out = format!(
        "amendment {:03} ({}): {} · critic verdict {}{}\n",
        rec.n,
        rec.slug,
        rec.status,
        rec.critic_verdict,
        if applied { " · applied" } else { "" }
    );
    if !rec.critic_reason.trim().is_empty() {
        out.push_str(&format!("reason: {}\n", rec.critic_reason.trim()));
    }
    if rec.status == "pending" {
        out.push_str(&format!("approve with `vdash amend approve {}` or reject with `vdash amend reject {}`\n", rec.n, rec.n));
    }
    Ok(out)
}

/// CLI `vdash amend list` (M6-03): number, status, file, verdict, approver, first line of justification.
pub fn list_cmd(ctx: &Ctx) -> Result<String> {
    let records = list_records(ctx)?;
    if records.is_empty() {
        return Ok("No amendments.\n".to_string());
    }
    let mut out = String::new();
    for r in records {
        let first_line = r.justification.lines().next().unwrap_or("").trim();
        out.push_str(&format!(
            "{:03}  {:<10}  {:<28}  verdict={:<8}  by={:<12}  {}\n",
            r.n, r.status, r.file, r.critic_verdict, r.approved_by, first_line
        ));
    }
    Ok(out)
}
