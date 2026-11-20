//! Spec phase orchestration (ROADMAP M5-02, M5-03; DESIGN §5):
//! `draft`, `revise`, `review_only`, and `converse`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::agents::{self, Outcome, Request};
use crate::backend::pool::Pool;
use crate::backend::Profile;
use crate::config::{BackendKind, Role};
use crate::error::{VdashError, Result};
use crate::events;
use crate::lean;
use crate::paths::Ctx;
use crate::policy::{self, Scope};
use crate::schemas::Reply;
use crate::util;

pub const LENSES: [&str; 4] = ["soundness", "strength", "fidelity", "legibility"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: String,
    pub where_: String,
    pub issue: String,
    pub suggestion: String,
}

impl Finding {
    fn severity_rank(&self) -> u8 {
        match self.severity.as_str() {
            "blocker" => 0,
            "major" => 1,
            "minor" => 2,
            _ => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LensReview {
    pub lens: String,
    pub verdict: String,
    pub findings: Vec<Finding>,
    pub failure: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpecReport {
    pub review_num: u32,
    pub review_path: PathBuf,
    pub writer_notes: String,
    pub writer_failure: Option<String>,
    pub build_ok: bool,
    pub build_summary: String,
    pub reviews: Vec<LensReview>,
    pub blockers: usize,
    pub majors: usize,
    pub minors: usize,
    pub warnings: Vec<String>,
    pub summary: String,
}

/// `own_lock`: the caller (the spec loop) holds the engine lock itself.
fn check_preconditions(ctx: &Ctx, require_brief: bool, own_lock: bool) -> Result<()> {
    let state = util::load_state(&ctx.campaign)?;
    if state.phase != "init" && state.phase != "spec" {
        return Err(VdashError::new(
            2,
            format!(
                "spec commands require phase 'init' or 'spec' (current phase is '{}')",
                state.phase
            ),
        ));
    }
    if let Some(pid) = events::engine_running(ctx).filter(|_| !own_lock) {
        return Err(VdashError::new(
            2,
            format!("an engine is currently running (pid {pid})"),
        ));
    }
    if require_brief {
        let brief = ctx.campaign.sources_dir().join("BRIEF.md");
        if !brief.is_file() {
            return Err(VdashError::new(
                2,
                format!("missing required brief file '{}'", brief.display()),
            ));
        }
    }
    Ok(())
}

/// Check whether all `reviewer` entries use the exact same backend family as all
/// `spec_writer` entries.
pub fn cross_family_warning(ctx: &Ctx) -> Option<String> {
    let sw: BTreeSet<BackendKind> = ctx
        .cfg
        .entries_for(Role::SpecWriter)
        .iter()
        .map(|e| e.backend)
        .collect();
    let rev: BTreeSet<BackendKind> = ctx
        .cfg
        .entries_for(Role::Reviewer)
        .iter()
        .map(|e| e.backend)
        .collect();
    if !sw.is_empty() && sw.len() == 1 && sw == rev {
        let b = sw.iter().next().copied().unwrap_or(BackendKind::Claude);
        Some(format!(
            "warning: spec_writer and reviewer both use only '{b}' (no cross-family review)"
        ))
    } else {
        None
    }
}

fn latest_review_file(ctx: &Ctx) -> Option<(u32, PathBuf)> {
    let dir = ctx.campaign.spec_dir();
    let rd = fs::read_dir(&dir).ok()?;
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in rd.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(rest) = name.strip_prefix("review-") {
            if let Some(num_str) = rest.strip_suffix(".md") {
                if let Ok(n) = num_str.parse::<u32>() {
                    if best.as_ref().map(|(b, _)| n > *b).unwrap_or(true) {
                        best = Some((n, path));
                    }
                }
            }
        }
    }
    best
}

fn next_review_number(ctx: &Ctx) -> u32 {
    latest_review_file(ctx).map(|(n, _)| n + 1).unwrap_or(1)
}

fn collect_spec_context(ctx: &Ctx) -> String {
    let mut out = String::new();
    let brief = ctx.campaign.sources_dir().join("BRIEF.md");
    if let Ok(text) = fs::read_to_string(&brief) {
        out.push_str("### sources/BRIEF.md\n");
        out.push_str(&text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    let dep = ctx.campaign.departures_file();
    if let Ok(text) = fs::read_to_string(&dep) {
        out.push_str("### spec/DEPARTURES.md\n");
        out.push_str(&text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    for rel in ctx.trusted_files() {
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

fn build_trusted_tree(ctx: &Ctx) -> (bool, String) {
    let trusted_files = ctx.trusted_files();
    if trusted_files.is_empty() {
        return (false, "FAILED: no trusted Lean files found".to_string());
    }
    let mut modules = Vec::new();
    for rel in &trusted_files {
        if let Ok(m) = ctx.campaign.module_name(rel) {
            modules.push(m);
        }
    }
    if modules.is_empty() {
        return (false, "FAILED: no trusted modules found".to_string());
    }
    match lean::build::build(
        ctx,
        &ctx.campaign.lean_dir(),
        &ctx.campaign.build_work_dir(),
        Some(&modules),
    ) {
        Ok(res) => {
            let mut diags = Vec::new();
            for m in res.results.values() {
                diags.extend(m.diagnostics.clone());
            }
            let formatted = lean::build::format_capped_diagnostics(&diags).join("\n");
            if res.ok {
                let sorry_count = diags
                    .iter()
                    .filter(|d| {
                        d.kind == "hasSorry" || d.text.contains("declaration uses 'sorry'") || d.text.contains("declaration uses `sorry`")
                    })
                    .count();
                let header = if sorry_count > 0 {
                    format!("OK ({sorry_count} sorry)")
                } else {
                    "OK".to_string()
                };
                if formatted.is_empty() {
                    (true, header)
                } else {
                    (true, format!("{header}\n{formatted}"))
                }
            } else {
                let err_count = diags
                    .iter()
                    .filter(|d| d.severity == "error")
                    .count()
                    .max(1);
                if formatted.is_empty() {
                    (false, format!("FAILED ({err_count} errors)"))
                } else {
                    (false, format!("FAILED ({err_count} errors)\n{formatted}"))
                }
            }
        }
        Err(e) => (
            false,
            format!("UNAVAILABLE (could not run lean build: {})", e.message),
        ),
    }
}

fn parse_lens_review(lens: &str, outcome: Outcome) -> LensReview {
    match outcome {
        Outcome::Ok(val) => {
            let verdict = val
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("revise")
                .to_string();
            let mut findings = Vec::new();
            if let Some(arr) = val.get("findings").and_then(Value::as_array) {
                for item in arr {
                    let severity = item
                        .get("severity")
                        .and_then(Value::as_str)
                        .unwrap_or("minor")
                        .to_string();
                    let where_ = item
                        .get("where")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let issue = item
                        .get("issue")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let suggestion = item
                        .get("suggestion")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    findings.push(Finding {
                        severity,
                        where_,
                        issue,
                        suggestion,
                    });
                }
            }
            findings.sort_by_key(|f| f.severity_rank());
            LensReview {
                lens: lens.to_string(),
                verdict,
                findings,
                failure: None,
            }
        }
        Outcome::Failed(err) => LensReview {
            lens: lens.to_string(),
            verdict: "failed".to_string(),
            findings: Vec::new(),
            failure: Some(err),
        },
    }
}

fn run_reviewers_and_write_report(
    ctx: &Ctx,
    pool: &Pool,
    writer_notes: String,
    writer_failure: Option<String>,
    mut warnings: Vec<String>,
) -> Result<SpecReport> {
    let (build_ok, build_summary) = build_trusted_tree(ctx);
    let spec_context = collect_spec_context(ctx);
    let rev_snap = policy::snapshot(ctx);

    let reviews: Vec<LensReview> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        for lens in LENSES {
            let build_ref = &build_summary;
            let spec_ref = &spec_context;
            handles.push(s.spawn(move || -> Result<LensReview> {
                let req = Request::new(
                    Role::Reviewer,
                    Reply::Reviewer,
                    format!("Review the draft Lean specification through the '{lens}' lens."),
                )
                .section("lens", lens)
                .section("build_diagnostics", build_ref.as_str())
                .section("spec_files", spec_ref.as_str())
                .label(lens)
                .profile(Profile::Readonly);
                let (_, outcome) = agents::run(ctx, pool, 0, req)?;
                let rev = parse_lens_review(lens, outcome);
                // Emit a live note event so the app transcript displays each lens as it arrives (M5-03).
                let mut msg = format!("reviewer ({lens}): {}", rev.verdict);
                for f in rev
                    .findings
                    .iter()
                    .filter(|f| f.severity == "blocker" || f.severity == "major")
                {
                    msg.push_str(&format!("\n  [{}] {}: {}", f.severity, f.where_, f.issue));
                }
                events::emit(ctx, 0, "note", json!({ "text": msg }));
                Ok(rev)
            }));
        }
        let mut out = Vec::with_capacity(LENSES.len());
        for h in handles {
            match h.join() {
                Ok(Ok(r)) => out.push(r),
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(VdashError::new(5, "reviewer thread panicked")),
            }
        }
        Ok(out)
    })?;

    let rev_violations = policy::enforce(ctx, &rev_snap, Scope::Spec, &BTreeSet::new())?;
    if !rev_violations.is_empty() {
        policy::report(ctx, 0, &rev_violations);
        for v in &rev_violations {
            warnings.push(format!(
                "warning: reverted unauthorized reviewer edit ({}) to {}",
                v.change, v.path
            ));
        }
    }

    let mut blockers = 0usize;
    let mut majors = 0usize;
    let mut minors = 0usize;
    for r in &reviews {
        for f in &r.findings {
            match f.severity.as_str() {
                "blocker" => blockers += 1,
                "major" => majors += 1,
                _ => minors += 1,
            }
        }
    }

    let review_num = next_review_number(ctx);
    let review_path = ctx
        .campaign
        .spec_dir()
        .join(format!("review-{review_num}.md"));
    if let Some(parent) = review_path.parent() {
        fs::create_dir_all(parent).map_err(|e| VdashError::io_path(parent, e))?;
    }

    let mut md = String::new();
    md.push_str(&format!("# Spec Review {review_num}\n\n"));
    md.push_str("## Build Status\n\n```\n");
    md.push_str(build_summary.trim_end());
    md.push_str("\n```\n\n");

    for r in &reviews {
        md.push_str(&format!("## Lens: {} ({})\n\n", r.lens, r.verdict));
        if let Some(fail) = &r.failure {
            md.push_str(&format!("- **reviewer failed**: {fail}\n\n"));
            continue;
        }
        if r.findings.is_empty() {
            md.push_str("No findings.\n\n");
            continue;
        }
        for f in &r.findings {
            md.push_str(&format!(
                "- **[{}]** `{}`: {}\n  - *Suggestion:* {}\n",
                f.severity, f.where_, f.issue, f.suggestion
            ));
        }
        md.push('\n');
    }

    fs::write(&review_path, &md).map_err(|e| VdashError::io_path(&review_path, e))?;

    util::update_state(&ctx.campaign, |s| {
        s.phase = "spec".to_string();
    })?;

    let mut summary_lines = Vec::new();
    for w in &warnings {
        summary_lines.push(w.clone());
    }
    if !writer_notes.is_empty() {
        summary_lines.push(format!("spec_writer: {writer_notes}"));
    }
    if let Some(wf) = &writer_failure {
        summary_lines.push(format!("spec_writer failed: {wf}"));
    }
    summary_lines.push(format!(
        "spec/review-{review_num}.md: {blockers} blocker, {majors} major, {minors} minor"
    ));
    let failed: Vec<&str> = reviews.iter().filter(|r| r.failure.is_some()).map(|r| r.lens.as_str()).collect();
    if !failed.is_empty() {
        summary_lines.push(format!(
            "{} of {} reviewers failed ({}); the review is incomplete. Rerun them with `vdash spec review` (or `/review`) before approving.",
            failed.len(),
            reviews.len(),
            failed.join(", ")
        ));
    } else if blockers == 0 {
        summary_lines.push(
            "No blockers reported. Run `vdash approve` (or `/approve`) when ready to lock the spec."
                .to_string(),
        );
    }
    let summary = summary_lines.join("\n") + "\n";

    Ok(SpecReport {
        review_num,
        review_path,
        writer_notes,
        writer_failure,
        build_ok,
        build_summary,
        reviews,
        blockers,
        majors,
        minors,
        warnings,
        summary,
    })
}

fn run_spec_writer(ctx: &Ctx, pool: &Pool, feedback: Option<&str>, own_lock: bool) -> Result<SpecReport> {
    check_preconditions(ctx, true, own_lock)?;

    let mut warnings = Vec::new();
    if let Some(w) = cross_family_warning(ctx) {
        eprintln!("vdash: {w}");
        events::emit(ctx, 0, "note", json!({ "text": w }));
        warnings.push(w);
    }

    let brief_path = ctx.campaign.sources_dir().join("BRIEF.md");
    let brief_text =
        fs::read_to_string(&brief_path).map_err(|e| VdashError::io_path(&brief_path, e))?;

    let snap = policy::snapshot(ctx);

    let spec_pfx = ctx.spec_rel_prefix();
    let chal_rel = ctx.challenge_rel();
    let mut req = if feedback.is_some() {
        Request::new(
            Role::SpecWriter,
            Reply::SpecWriter,
            format!(
                "Revise the Lean 4 specification in {spec_pfx}**, {chal_rel}, and spec/DEPARTURES.md based on the latest review and user feedback."
            ),
        )
        .label("revise")
    } else {
        Request::new(
            Role::SpecWriter,
            Reply::SpecWriter,
            format!(
                "Draft the initial Lean 4 specification in {spec_pfx}**, {chal_rel}, and spec/DEPARTURES.md from sources/BRIEF.md."
            ),
        )
        .label("draft")
    };
    req = req
        .section("sources_brief", brief_text)
        .profile(Profile::Lean);

    if let Some(fb) = feedback {
        if let Some((_, rev_path)) = latest_review_file(ctx) {
            if let Ok(rev_text) = fs::read_to_string(&rev_path) {
                req = req.section("latest_review", rev_text);
            }
        }
        req = req.section("user_feedback", fb.to_string());
    }

    let (_, writer_outcome) = agents::run(ctx, pool, 0, req)?;

    let allowed = BTreeSet::from([
        spec_pfx,
        chal_rel,
        "spec/DEPARTURES.md".to_string(),
    ]);
    let violations = policy::enforce(ctx, &snap, Scope::Spec, &allowed)?;
    if !violations.is_empty() {
        policy::report(ctx, 0, &violations);
        for v in &violations {
            warnings.push(format!(
                "warning: reverted out-of-scope spec_writer edit ({}) to {}",
                v.change, v.path
            ));
        }
    }

    let (writer_notes, writer_failure) = match writer_outcome {
        Outcome::Ok(val) => {
            let notes = val
                .get("notes")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !notes.is_empty() {
                events::emit(
                    ctx,
                    0,
                    "note",
                    json!({ "text": format!("spec_writer: {notes}") }),
                );
            }
            (notes, None)
        }
        Outcome::Failed(f) => {
            events::emit(
                ctx,
                0,
                "note",
                json!({ "text": format!("spec_writer failed: {f}") }),
            );
            (String::new(), Some(f))
        }
    };

    run_reviewers_and_write_report(ctx, pool, writer_notes, writer_failure, warnings)
}

/// `vdash spec draft`: run `spec_writer` on `sources/BRIEF.md`, build the trusted tree,
/// run the 4 reviewers in parallel, and write `spec/review-<N>.md`.
pub fn draft(ctx: &Ctx, pool: &Pool) -> Result<SpecReport> {
    run_spec_writer(ctx, pool, None, false)
}

/// `vdash spec revise "<feedback>"`: run `spec_writer` with `<user_feedback>` and the
/// latest `spec/review-<N>.md`, build the trusted tree, run the 4 reviewers in parallel,
/// and write the next `spec/review-<N>.md`.
pub fn revise(ctx: &Ctx, pool: &Pool, feedback: &str) -> Result<SpecReport> {
    run_spec_writer(ctx, pool, Some(feedback), false)
}

/// Rerun only the 4 reviewers (for `/review` in the app) and write `spec/review-<N>.md`.
pub fn review_only(ctx: &Ctx, pool: &Pool) -> Result<SpecReport> {
    review_inner(ctx, pool, false)
}

fn review_inner(ctx: &Ctx, pool: &Pool, own_lock: bool) -> Result<SpecReport> {
    check_preconditions(ctx, false, own_lock)?;
    let mut warnings = Vec::new();
    if let Some(w) = cross_family_warning(ctx) {
        events::emit(ctx, 0, "note", json!({ "text": w }));
        warnings.push(w);
    }
    run_reviewers_and_write_report(ctx, pool, String::new(), None, warnings)
}

/// The first step of a spec loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirstStep {
    Draft,
    Revise(String),
    Review,
}

/// Why a spec loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopEnd {
    /// Every lens said `ok` and the tree did not fail to build.
    Converged,
    /// The user said it is good enough (`stop`).
    Stopped,
    /// `[budget] spec_max_rounds` rounds ran.
    Budget,
    /// The spec-writer or a reviewer failed, so the review cannot be judged.
    Failed,
}

/// True when every lens said `ok`, no call failed, and the trusted tree did not
/// fail to build (an unavailable Lean does not hold the loop up).
pub fn converged(r: &SpecReport) -> bool {
    !r.build_summary.starts_with("FAILED")
        && r.writer_failure.is_none()
        && !r.reviews.is_empty()
        && r.reviews.iter().all(|l| l.failure.is_none() && l.verdict == "ok")
}

/// Control messages received between rounds.
#[derive(Default)]
struct Inbox {
    guidance: Vec<String>,
    stop: bool,
}

/// Drain the control inbox; while paused, block (polling every second) until
/// `resume` or `stop`.
fn take_controls(ctx: &mut Ctx, pool: &Pool, round: u32, inbox: &mut Inbox) {
    let mut paused = false;
    loop {
        for c in events::drain(ctx) {
            match c {
                events::Control::Guidance { text } => {
                    events::emit(ctx, round, "note", json!({"text": "guidance queued for the spec-writer's next revision"}));
                    inbox.guidance.push(text);
                }
                events::Control::Stop => {
                    events::emit(ctx, round, "note", json!({"text": "the user says the spec is good enough; the spec loop stops"}));
                    inbox.stop = true;
                }
                events::Control::Pause => {
                    if !paused {
                        events::emit(ctx, round, "paused", json!({}));
                    }
                    paused = true;
                }
                events::Control::Resume => {
                    if paused {
                        events::emit(ctx, round, "resumed", json!({}));
                    }
                    paused = false;
                }
                events::Control::SetRole { role, entries } => {
                    let parsed = role.parse::<Role>().and_then(|r| {
                        let es = entries.iter().map(|e| e.parse()).collect::<Result<Vec<crate::config::RoleEntry>>>()?;
                        crate::config::set_role(&ctx.campaign.root, r, &es)?;
                        pool.set_role(r, es);
                        Ok(())
                    });
                    let text = match parsed {
                        Ok(()) => format!("role {role} now uses {}", entries.join(", ")),
                        Err(e) => format!("could not set role {role}: {}", e.message),
                    };
                    if let Ok(cfg) = crate::config::load(&ctx.campaign.root) {
                        ctx.cfg = cfg;
                    }
                    events::emit(ctx, round, "note", json!({"text": text}));
                }
                events::Control::AmendDecision { .. } => {}
                // Escalations come from the proof phase; close the files, and keep a
                // proof-phase answer out of the spec writer's guidance.
                events::Control::ResolveEscalation { ns, answer } => {
                    let text = match crate::escalation::resolve(ctx, round, &ns, &answer, "user") {
                        Ok(r) => format!("escalation(s) {:?} closed", r.closed),
                        Err(e) => format!("could not resolve escalation: {}", e.message),
                    };
                    events::emit(ctx, round, "note", json!({"text": text}));
                }
            }
        }
        if !paused || inbox.stop {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// The spec loop: run `first`, then keep revising while any reviewer asks for
/// changes, until every lens says `ok`, the user stops it, a call fails, or
/// `[budget] spec_max_rounds` rounds have run. Holds the engine lock, so the
/// user's `stop`, `pause`, `/model` and guidance arrive through the control inbox;
/// guidance received during a round goes to the spec-writer in the next one (and
/// triggers one even if the reviewers were satisfied). Returns the last report.
pub fn iterate(ctx: &Ctx, pool: &Pool, first: FirstStep) -> Result<(SpecReport, LoopEnd)> {
    check_preconditions(ctx, !matches!(first, FirstStep::Review), false)?;
    let _lock = events::EngineLock::acquire(ctx)?;
    let mut ctx = ctx.clone();
    // Other messages left over from an earlier run are not meant for this loop;
    // guidance typed while the loop was starting is.
    let mut carried: Vec<String> = events::drain(&ctx)
        .into_iter()
        .filter_map(|c| if let events::Control::Guidance { text } = c { Some(text) } else { None })
        .collect();
    let max = ctx.cfg.budget.spec_max_rounds;
    let mut step = first;
    let mut round = 1u32;
    loop {
        events::emit(&ctx, 0, "note", json!({"text": format!("spec round {round} of at most {max}")}));
        let report = match &step {
            FirstStep::Draft => run_spec_writer(&ctx, pool, None, true)?,
            FirstStep::Revise(fb) => run_spec_writer(&ctx, pool, Some(fb), true)?,
            FirstStep::Review => review_inner(&ctx, pool, true)?,
        };
        events::emit(&ctx, 0, "note", json!({"text": report.summary.trim_end()}));
        let mut inbox = Inbox { guidance: std::mem::take(&mut carried), stop: false };
        take_controls(&mut ctx, pool, 0, &mut inbox);
        let end = if inbox.stop {
            Some(LoopEnd::Stopped)
        } else if report.writer_failure.is_some() || report.reviews.iter().any(|l| l.failure.is_some()) {
            Some(LoopEnd::Failed)
        } else if converged(&report) && inbox.guidance.is_empty() {
            Some(LoopEnd::Converged)
        } else if round >= max {
            Some(LoopEnd::Budget)
        } else {
            None
        };
        if let Some(end) = end {
            let text = match end {
                LoopEnd::Converged => format!("spec loop done: all four reviewers say ok (review-{})", report.review_num),
                LoopEnd::Stopped => format!("spec loop stopped by the user after review-{}", report.review_num),
                LoopEnd::Budget => format!(
                    "spec loop stopped after {round} rounds ([budget] spec_max_rounds); reviewers still ask for changes. Type feedback or /spec to continue, or /approve"
                ),
                LoopEnd::Failed => "spec loop stopped: a spec-writer or reviewer call failed, so the review is incomplete. Rerun with /review".to_string(),
            };
            events::emit(&ctx, 0, "note", json!({"text": text}));
            let mut report = report;
            report.summary.push_str(&text);
            report.summary.push('\n');
            return Ok((report, end));
        }
        let fb = if inbox.guidance.is_empty() {
            format!(
                "No new message from the user this round. Address the reviewers' findings in spec/review-{}.md (in <latest_review>).",
                report.review_num
            )
        } else {
            inbox.guidance.join("\n\n")
        };
        step = FirstStep::Revise(fb);
        round += 1;
    }
}

/// Conversational spec turn (M5-03): in phase `init` (or when no review exists yet),
/// runs `draft` (creating `sources/BRIEF.md` from `message` if missing); otherwise
/// runs `revise(ctx, pool, message)`.
pub fn converse(ctx: &Ctx, pool: &Pool, message: &str) -> Result<SpecReport> {
    let state = util::load_state(&ctx.campaign)?;
    if state.phase != "init" && state.phase != "spec" {
        return Err(VdashError::new(
            2,
            format!(
                "spec conversation requires phase 'init' or 'spec' (current phase is '{}')",
                state.phase
            ),
        ));
    }
    let brief_path = ctx.campaign.sources_dir().join("BRIEF.md");
    if state.phase == "init" && latest_review_file(ctx).is_none() {
        if !brief_path.is_file() {
            if let Some(parent) = brief_path.parent() {
                fs::create_dir_all(parent).map_err(|e| VdashError::io_path(parent, e))?;
            }
            fs::write(&brief_path, format!("{}\n", message.trim_end()))
                .map_err(|e| VdashError::io_path(&brief_path, e))?;
        }
        draft(ctx, pool)
    } else {
        if !brief_path.is_file() {
            if let Some(parent) = brief_path.parent() {
                fs::create_dir_all(parent).map_err(|e| VdashError::io_path(parent, e))?;
            }
            fs::write(&brief_path, format!("{}\n", message.trim_end()))
                .map_err(|e| VdashError::io_path(&brief_path, e))?;
        }
        revise(ctx, pool, message)
    }
}
