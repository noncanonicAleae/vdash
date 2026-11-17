//! Escalations: questions and potential bugs for the human (ROADMAP M6-04, M6-05;
//! DESIGN §10). Each one is a Markdown file `escalations/NNN-<slug>.md`, written by
//! the engine from a team lead's summary or from an amendment the critic escalated.
//!
//! The file is the record. Its header list carries `- status: <s>` (`open`,
//! `resolved`, `applied`, `rejected`); a file without that line is `open`. The
//! human closes a lead escalation with `resolve` (optionally answering it, which
//! also becomes guidance); an amendment's escalation closes when that amendment is
//! applied or rejected.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde_json::json;

use crate::error::{Result, VdashError};
use crate::events;
use crate::paths::Ctx;
use crate::util;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalation {
    pub n: u32,
    /// Path relative to the campaign root.
    pub rel: String,
    pub round: Option<u32>,
    pub from: String,
    pub status: String,
    /// `amendments/NNN-<slug>.md` when the critic escalated an amendment.
    pub amendment: Option<String>,
    pub summary: String,
}

impl Escalation {
    pub fn is_open(&self) -> bool {
        self.status == "open"
    }

    /// The amendment number, for an amendment-backed escalation.
    pub fn amendment_n(&self) -> Option<u32> {
        let a = self.amendment.as_deref()?;
        let name = a.rsplit('/').next()?;
        name.split('-').next()?.parse().ok()
    }

    /// `#N (round r, from X): summary`, as shown to people and the orchestrator.
    pub fn line(&self) -> String {
        let round = self.round.map(|r| format!("round {r}, ")).unwrap_or_default();
        format!("#{} ({round}from {}): {}", self.n, self.from, self.summary)
    }
}

fn prefix_number(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || !name[digits.len()..].starts_with('-') {
        return None;
    }
    digits.parse().ok()
}

/// The next escalation number: one more than the largest `NNN-` prefix in the
/// directory. Never a file count, so a gap or a deleted file cannot cause a reuse.
pub fn next_number(ctx: &Ctx) -> u32 {
    fs::read_dir(ctx.campaign.escalations_dir())
        .map(|r| r.flatten().filter_map(|e| prefix_number(&e.file_name().to_string_lossy())).max().unwrap_or(0))
        .unwrap_or(0)
        + 1
}

pub fn slug(text: &str) -> String {
    let first: String = text.lines().next().unwrap_or("issue").chars().take(40).collect();
    let mut s: String = first.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    if s.is_empty() {
        s = "issue".into();
    }
    s
}

fn header_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    for line in text.lines() {
        let l = line.trim();
        let Some(rest) = l.strip_prefix("- ") else { continue };
        let rest = rest.trim_start_matches("**");
        let lower = rest.to_ascii_lowercase();
        if lower.starts_with(key) {
            let after = &rest[key.len()..];
            let after = after.trim_start_matches("**");
            if let Some(v) = after.strip_prefix(':') {
                return Some(v.trim().trim_start_matches("**").trim().trim_matches('`'));
            }
        }
    }
    None
}

fn summary_of(text: &str) -> String {
    // The first line of the body: not the title, not a header item.
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("- "))
        .unwrap_or("")
        .to_string()
}

/// Parse one escalation file.
pub fn parse(rel: &str, text: &str) -> Option<Escalation> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let n = prefix_number(name)?;
    let amendment = header_value(text, "amendment").map(str::to_string);
    let status = header_value(text, "status").map(|s| s.to_ascii_lowercase()).unwrap_or_else(|| "open".into());
    let round = header_value(text, "round").and_then(|r| r.parse().ok());
    let from = header_value(text, "from").map(str::to_string).unwrap_or_else(|| if amendment.is_some() { "critic".into() } else { "?".into() });
    let summary = if amendment.is_some() {
        let reason = header_value(text, "critic reason").unwrap_or("").to_string();
        let a = amendment.as_deref().unwrap_or("");
        format!("escalated amendment {a}: {reason}")
    } else {
        summary_of(text)
    };
    Some(Escalation { n, rel: rel.to_string(), round, from, status, amendment, summary })
}

/// Every escalation, by number.
pub fn list(ctx: &Ctx) -> Vec<Escalation> {
    let dir = ctx.campaign.escalations_dir();
    let mut out: Vec<Escalation> = fs::read_dir(&dir)
        .map(|r| {
            r.flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let text = fs::read_to_string(e.path()).ok()?;
                    parse(&format!("escalations/{name}"), &text)
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|e| e.n);
    out
}

/// The file of escalation `n`, relative to the campaign root.
pub fn rel_of(ctx: &Ctx, n: u32) -> Option<String> {
    list(ctx).into_iter().find(|e| e.n == n).map(|e| e.rel)
}

pub fn open(ctx: &Ctx) -> Vec<Escalation> {
    list(ctx).into_iter().filter(Escalation::is_open).collect()
}

fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(|w| w.to_lowercase())
        .collect()
}

/// Jaccard similarity of the two texts' word sets (M6-05).
pub fn similarity(a: &str, b: &str) -> f64 {
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    let inter = wa.intersection(&wb).count() as f64;
    let union = wa.union(&wb).count() as f64;
    inter / union
}

pub const DUPLICATE_THRESHOLD: f64 = 0.6;

fn body_of(text: &str) -> String {
    // Everything after the header list, without resolutions.
    let mut out = String::new();
    let mut started = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("## Resolution") {
            break;
        }
        if !started {
            if l.is_empty() || l.starts_with('#') || l.starts_with("- ") {
                continue;
            }
            started = true;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The open escalation `text` duplicates, if any.
pub fn duplicate_of(ctx: &Ctx, text: &str) -> Option<u32> {
    for e in open(ctx) {
        if e.amendment.is_some() {
            continue;
        }
        let Ok(cur) = fs::read_to_string(ctx.campaign.root.join(&e.rel)) else { continue };
        if similarity(&body_of(&cur), text) >= DUPLICATE_THRESHOLD {
            return Some(e.n);
        }
    }
    None
}

/// Write a team lead's escalation, unless it repeats an open one (M6-05). Returns
/// the new number, or `Err(n)` with the open escalation it duplicates.
pub fn write_lead(ctx: &Ctx, round: u32, who: &str, text: &str) -> Result<std::result::Result<u32, u32>> {
    let text = text.trim();
    if let Some(dup) = duplicate_of(ctx, text) {
        events::emit(ctx, round, "note", json!({"text": format!("{who}: escalation skipped, it repeats open escalation #{dup}")}));
        return Ok(Err(dup));
    }
    let dir = ctx.campaign.escalations_dir();
    fs::create_dir_all(&dir).map_err(|e| VdashError::io_path(&dir, e))?;
    let n = next_number(ctx);
    let rel = format!("escalations/{n:03}-{}.md", slug(text));
    let body = format!(
        "# Escalation {n}\n\n- round: {round}\n- from: {who}\n- at: {}\n- status: open\n\n{text}\n",
        util::now_iso()
    );
    let p = ctx.campaign.root.join(&rel);
    fs::write(&p, body).map_err(|e| VdashError::io_path(&p, e))?;
    crate::engine::record_engine_file(ctx, &rel);
    let first: String = text.lines().next().unwrap_or("").chars().take(200).collect();
    events::emit(ctx, round, "escalation", json!({"n": n, "text": first, "from": who}));
    Ok(Ok(n))
}

/// Write the escalation for an amendment the critic escalated. Returns its path.
pub fn write_amendment(ctx: &Ctx, amendment_rel: &str, slug_text: &str, file: &str, reason: &str, body: &str) -> Result<(u32, String)> {
    let dir = ctx.campaign.escalations_dir();
    fs::create_dir_all(&dir).map_err(|e| VdashError::io_path(&dir, e))?;
    let n = next_number(ctx);
    let rel = format!("escalations/{n:03}-{slug_text}.md");
    let md = format!(
        "# Escalation {n:03}: {slug_text}\n\n- **Amendment**: `{amendment_rel}`\n- **Target file**: `{file}`\n- **Critic reason**: {}\n- status: open\n\n{body}",
        reason.replace('\n', " ")
    );
    let p = ctx.campaign.root.join(&rel);
    fs::write(&p, md).map_err(|e| VdashError::io_path(&p, e))?;
    crate::engine::record_engine_file(ctx, &rel);
    Ok((n, rel))
}

fn set_status(text: &str, status: &str) -> String {
    // Replace the status line of the first header list, or add one at its end.
    let mut out = String::new();
    let mut done = false;
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let l = line.trim();
        if !done && l.starts_with("- ") {
            let low = l.trim_start_matches("- ").trim_start_matches("**").to_ascii_lowercase();
            if low.starts_with("status") {
                out.push_str(&format!("- status: {status}\n"));
                done = true;
                continue;
            }
            let next_is_item = lines.get(i + 1).is_some_and(|n| n.trim().starts_with("- "));
            out.push_str(line);
            out.push('\n');
            if !next_is_item {
                out.push_str(&format!("- status: {status}\n"));
                done = true;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !done {
        out = format!("- status: {status}\n\n{out}");
    }
    out
}

/// Set the status of escalation file `rel` and append a note (no-op if missing).
pub fn close_file(ctx: &Ctx, rel: &str, status: &str, by: &str, note: &str) -> Result<()> {
    let p = ctx.campaign.root.join(rel);
    let Ok(text) = fs::read_to_string(&p) else { return Ok(()) };
    let mut new = set_status(&text, status);
    if !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&format!("\n## Resolution ({}, by {by})\n\n{}\n", util::now_iso(), note.trim()));
    fs::write(&p, new).map_err(|e| VdashError::io_path(&p, e))?;
    crate::engine::record_engine_file(ctx, rel);
    Ok(())
}

/// Close an amendment's escalation when the amendment is applied or rejected.
pub fn close_for_amendment(ctx: &Ctx, escalation_rel: &str, status: &str, by: &str) -> Result<()> {
    if escalation_rel.is_empty() || !ctx.campaign.root.join(escalation_rel).is_file() {
        return Ok(());
    }
    let note = match status {
        "applied" => "The amendment was applied, knowingly accepting a weaker guarantee.",
        _ => "The amendment was rejected.",
    };
    close_file(ctx, escalation_rel, status, by, note)
}

/// Parse `3`, `1-15`, `1-3,7,9-10` into numbers.
pub fn parse_numbers(spec: &str) -> Result<Vec<u32>> {
    let bad = || VdashError::new(2, format!("'{spec}' is not an escalation number or range (e.g. 23, 16-22, 1-3,7)"));
    let mut out = BTreeSet::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let part = part.trim_start_matches('#');
        if let Some((a, b)) = part.split_once('-') {
            let (a, b): (u32, u32) = (a.trim().parse().map_err(|_| bad())?, b.trim().trim_start_matches('#').parse().map_err(|_| bad())?);
            if a > b || b - a > 10_000 {
                return Err(bad());
            }
            out.extend(a..=b);
        } else {
            out.insert(part.parse().map_err(|_| bad())?);
        }
    }
    if out.is_empty() {
        return Err(bad());
    }
    Ok(out.into_iter().collect())
}

/// What a `resolve` did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub closed: Vec<u32>,
    /// The files changed, relative to the campaign root.
    pub rels: Vec<String>,
    /// Guidance to queue for the orchestrator (empty without an answer).
    pub guidance: String,
}

/// Check that every number names an open lead escalation (M6-04).
pub fn validate(ctx: &Ctx, ns: &[u32]) -> Result<Vec<Escalation>> {
    let all = list(ctx);
    let mut out = Vec::new();
    for n in ns {
        let Some(e) = all.iter().find(|e| e.n == *n) else {
            return Err(VdashError::new(2, format!("there is no escalation #{n}")));
        };
        if let Some(a) = e.amendment_n() {
            return Err(VdashError::new(
                2,
                format!(
                    "escalation #{n} is amendment {a:03}'s escalated proposal: decide it with `/amend approve {a} --accept-weakening` or `/amend reject {a}`"
                ),
            ));
        }
        if !e.is_open() {
            return Err(VdashError::new(2, format!("escalation #{n} is already {}", e.status)));
        }
        out.push(e.clone());
    }
    Ok(out)
}

/// Mark lead escalations resolved, with an optional answer (M6-04). The caller
/// queues `guidance` when it is not empty.
pub fn resolve(ctx: &Ctx, round: u32, ns: &[u32], answer: &str, by: &str) -> Result<Resolved> {
    let es = validate(ctx, ns)?;
    let answer = answer.trim();
    let note = if answer.is_empty() { "Dismissed without an answer.".to_string() } else { answer.to_string() };
    let mut res = Resolved::default();
    for e in &es {
        close_file(ctx, &e.rel, "resolved", by, &note)?;
        events::emit(ctx, round, "escalation_resolved", json!({"n": e.n, "answered": !answer.is_empty()}));
        res.closed.push(e.n);
        res.rels.push(e.rel.clone());
    }
    if !answer.is_empty() {
        let about: Vec<String> = es
            .iter()
            .map(|e| {
                let s: String = e.summary.chars().take(240).collect();
                let round = e.round.map(|r| format!(", round {r}")).unwrap_or_default();
                format!("#{} ({}{round}: {s})", e.n, e.from)
            })
            .collect();
        let label = if es.len() == 1 { "escalation" } else { "escalations" };
        res.guidance = format!("Answer to {label} {}: {answer}", about.join("; "));
    }
    Ok(res)
}

/// `vdash escalations [--all]` and `/escalations [all]`.
pub fn list_text(ctx: &Ctx, all: bool) -> String {
    let es = list(ctx);
    let shown: Vec<&Escalation> = es.iter().filter(|e| all || e.is_open()).collect();
    if shown.is_empty() {
        let closed = es.len();
        return if closed == 0 {
            "No escalations.\n".into()
        } else {
            format!("No open escalations ({closed} closed; `escalations --all` lists them).\n")
        };
    }
    let mut out = String::new();
    for e in &shown {
        let status = if all { format!("{:<9} ", e.status) } else { String::new() };
        let mut s: String = e.summary.chars().take(160).collect();
        if e.summary.chars().count() > 160 {
            s.push('…');
        }
        let round = e.round.map(|r| format!("r{r}")).unwrap_or_else(|| "-".into());
        out.push_str(&format!("#{:<3} {status}{round:<4} {:<7} {s}\n", e.n, e.from));
    }
    let open = es.iter().filter(|e| e.is_open()).count();
    out.push_str(&format!(
        "{open} open. Answer one with `resolve <N> <answer>` (the answer becomes guidance), dismiss with `resolve <N>`; ranges like 1-15 work. Files: escalations/\n"
    ));
    out
}

/// `vdash resolve` and `/resolve` (M6-04). With an engine running the decision is
/// queued for its next checkpoint (editing the files mid-round would be reverted);
/// otherwise the files are closed now and an answer is queued as guidance for the
/// next engine start.
pub fn resolve_cmd(ctx: &Ctx, spec: &str, answer: &str) -> Result<String> {
    let ns = parse_numbers(spec)?;
    validate(ctx, &ns)?;
    let list = ns.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ");
    let answer = answer.trim();
    if events::engine_running(ctx).is_some() {
        events::send(ctx, &events::Control::ResolveEscalation { ns, answer: answer.to_string() })?;
        let what = if answer.is_empty() { "dismissal" } else { "answer" };
        return Ok(format!("{what} for escalation {list} queued for the running engine\n"));
    }
    let round = util::load_state(&ctx.campaign).map(|s| s.round).unwrap_or(0);
    let r = resolve(ctx, round, &ns, answer, "user")?;
    if r.guidance.is_empty() {
        Ok(format!("escalation {list} dismissed\n"))
    } else {
        events::send(ctx, &events::Control::Guidance { text: r.guidance })?;
        Ok(format!("escalation {list} resolved; the answer is queued as guidance for the orchestrator\n"))
    }
}

/// Read escalation `n`'s file, for `/escalations <n>`.
pub fn show(ctx: &Ctx, n: u32) -> Result<String> {
    let e = list(ctx).into_iter().find(|e| e.n == n).ok_or_else(|| VdashError::new(2, format!("there is no escalation #{n}")))?;
    let p = ctx.campaign.root.join(&e.rel);
    fs::read_to_string(&p).map_err(|er| VdashError::io_path(Path::new(&p), er))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use crate::paths;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Ctx) {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
        let camp = tmp.path().join("camp");
        paths::init_campaign(&camp, &host, None, &[BackendKind::Claude]).unwrap();
        let ctx = Ctx::for_test(&camp, tmp.path().join("state")).unwrap();
        (tmp, ctx)
    }

    // The quartic-query-separation format, before status lines existed.
    const OLD_LEAD: &str = "# Escalation 23\n\n- round: 6\n- from: L2\n- at: 2026-11-17T15:25:27.38957185Z\n\nBoth separation lemmas (L_sep_rand_lower, L_sep_cheatsheet) are blocked on paper results that are only cited as hypotheses. The human should decide whether to accept hypothesis-carrying lemmas for arXiv:2008.07003 and arXiv:1511.01937, or whether the target statements need changing.\n";
    const OLD_24: &str = "Possible mismatch: the cheat-sheet goal's sqrt(m) quantum bound and the paper's bound may differ (worker reports sqrt(k m^{3/2}) and a 1/log loss). Needs checking against arXiv:1511.01937 before the goal is used.";

    #[test]
    fn parses_old_files_as_open() {
        let e = parse("escalations/023-both_separation_lemmas__l_sep_rand_lower.md", OLD_LEAD).unwrap();
        assert_eq!(e.n, 23);
        assert_eq!(e.round, Some(6));
        assert_eq!(e.from, "L2");
        assert!(e.is_open());
        assert!(e.summary.starts_with("Both separation lemmas"));
        assert_eq!(e.amendment_n(), None);
    }

    #[test]
    fn numbering_skips_gaps_and_counts_amendment_escalations() {
        let (_t, ctx) = setup();
        let dir = ctx.campaign.escalations_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("002-a.md"), "# Escalation 2\n\nx\n").unwrap();
        fs::write(dir.join("007-b.md"), "# Escalation 7\n\ny\n").unwrap();
        fs::write(dir.join("notes.txt"), "z").unwrap();
        // A file count would say 4 and collide with nothing here but reuse numbers later.
        assert_eq!(next_number(&ctx), 8);
        let (n, rel) = write_amendment(&ctx, "amendments/003-fix.md", "fix", "lean/Apc/Spec/Defs.lean", "weakens it", "## Justification\n\nj\n").unwrap();
        assert_eq!(n, 8);
        let e = parse(&rel, &fs::read_to_string(ctx.campaign.root.join(&rel)).unwrap()).unwrap();
        assert_eq!(e.amendment_n(), Some(3));
        assert_eq!(e.from, "critic");
        assert!(e.is_open());
        assert_eq!(write_lead(&ctx, 1, "L1", "Is the spec of foo too weak?").unwrap(), Ok(9));
    }

    #[test]
    fn resolve_with_and_without_answer() {
        let (_t, ctx) = setup();
        write_lead(&ctx, 2, "L1", "Should the target take the hypothesis from the cited paper?").unwrap().unwrap();
        write_lead(&ctx, 2, "L2", "The bound in goal G differs from the paper's bound by a log factor.").unwrap().unwrap();
        write_lead(&ctx, 3, "L3", "Is lemma H meant to be stated for all n or only n > 0?").unwrap().unwrap();

        let r = resolve(&ctx, 3, &[1], "Yes, use it as a cited hypothesis.", "user").unwrap();
        assert_eq!(r.closed, vec![1]);
        assert!(r.guidance.starts_with("Answer to escalation #1 (L1, round 2: Should the target"), "{}", r.guidance);
        assert!(r.guidance.ends_with("Yes, use it as a cited hypothesis."));

        let r = resolve(&ctx, 3, &[2, 3], "", "user").unwrap();
        assert_eq!(r.closed, vec![2, 3]);
        assert!(r.guidance.is_empty());

        let all = list(&ctx);
        assert!(all.iter().all(|e| e.status == "resolved"));
        let text = fs::read_to_string(ctx.campaign.root.join(&all[0].rel)).unwrap();
        assert!(text.contains("- status: resolved\n"));
        assert!(text.contains("## Resolution ("));
        assert!(text.contains("Yes, use it as a cited hypothesis."));
        assert!(fs::read_to_string(ctx.campaign.root.join(&all[1].rel)).unwrap().contains("Dismissed without an answer."));
        assert!(open(&ctx).is_empty());
        // Already resolved, or missing.
        assert_eq!(resolve(&ctx, 3, &[1], "", "user").unwrap_err().code, 2);
        assert_eq!(resolve(&ctx, 3, &[42], "", "user").unwrap_err().code, 2);
    }

    #[test]
    fn resolving_an_old_file_inserts_the_status_line() {
        let (_t, ctx) = setup();
        let dir = ctx.campaign.escalations_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("023-both.md"), OLD_LEAD).unwrap();
        resolve(&ctx, 7, &[23], "", "user").unwrap();
        let text = fs::read_to_string(dir.join("023-both.md")).unwrap();
        assert!(text.contains("- at: 2026-11-17T15:25:27.38957185Z\n- status: resolved\n\nBoth separation"), "{text}");
        let e = parse("escalations/023-both.md", &text).unwrap();
        assert_eq!(e.status, "resolved");
        assert!(e.summary.starts_with("Both separation lemmas"));
    }

    #[test]
    fn amendment_escalations_are_decided_through_amend() {
        let (_t, ctx) = setup();
        let (n, rel) = write_amendment(&ctx, "amendments/004-x.md", "x", "lean/Apc/Spec/Defs.lean", "weakens", "body\n").unwrap();
        let err = resolve(&ctx, 1, &[n], "ok", "user").unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("/amend approve 4 --accept-weakening"), "{}", err.message);
        close_for_amendment(&ctx, &rel, "applied", "user").unwrap();
        let e = list(&ctx).into_iter().find(|e| e.n == n).unwrap();
        assert_eq!(e.status, "applied");
    }

    #[test]
    fn resolve_cmd_offline_queues_the_answer_as_guidance() {
        let (_t, ctx) = setup();
        write_lead(&ctx, 2, "L1", "Should goal G carry the cited hypothesis?").unwrap().unwrap();
        write_lead(&ctx, 2, "L2", "Different question about permutations and lists?").unwrap().unwrap();
        let out = resolve_cmd(&ctx, "1", "yes").unwrap();
        assert!(out.contains("queued as guidance"), "{out}");
        let msgs = events::drain(&ctx);
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            events::Control::Guidance { text } => assert!(text.starts_with("Answer to escalation #1 (L1, round 2:") && text.ends_with(": yes"), "{text}"),
            other => panic!("{other:?}"),
        }
        assert!(resolve_cmd(&ctx, "2", "").unwrap().contains("dismissed"));
        assert!(events::drain(&ctx).is_empty());
        assert_eq!(resolve_cmd(&ctx, "2", "").unwrap_err().code, 2);
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_numbers("23").unwrap(), vec![23]);
        assert_eq!(parse_numbers("1-3,7, 9-10").unwrap(), vec![1, 2, 3, 7, 9, 10]);
        assert_eq!(parse_numbers("#5").unwrap(), vec![5]);
        assert!(parse_numbers("3-1").is_err());
        assert!(parse_numbers("abc").is_err());
        assert!(parse_numbers("").is_err());
    }

    #[test]
    fn duplicates_are_skipped_but_distinct_questions_kept() {
        let (_t, ctx) = setup();
        let first = "The cheat-sheet goal depends on the Aaronson et al. lemma, which is only cited as a hypothesis; the human should decide whether to accept it.";
        assert_eq!(write_lead(&ctx, 4, "L2", first).unwrap(), Ok(1));
        let reworded = "The cheat-sheet goal depends on the Aaronson et al. lemma, which is only cited as a hypothesis. The human must decide whether to accept it.";
        assert_eq!(write_lead(&ctx, 5, "L3", reworded).unwrap(), Err(1));
        // quartic-query-separation 023 and 024: one paper, two different questions.
        let body23 = body_of(OLD_LEAD);
        assert!(similarity(&body23, OLD_24) < DUPLICATE_THRESHOLD, "{}", similarity(&body23, OLD_24));
        assert_eq!(write_lead(&ctx, 6, "L2", body23.trim()).unwrap(), Ok(2));
        assert_eq!(write_lead(&ctx, 6, "L2", OLD_24).unwrap(), Ok(3));
        // Once resolved, the same text may be raised again.
        resolve(&ctx, 6, &[1], "", "user").unwrap();
        assert_eq!(write_lead(&ctx, 7, "L3", reworded).unwrap(), Ok(4));
    }

    #[test]
    fn list_text_shows_open_only_by_default() {
        let (_t, ctx) = setup();
        write_lead(&ctx, 1, "L1", "Question one about the spec of sort?").unwrap().unwrap();
        write_lead(&ctx, 1, "L2", "Entirely different matter regarding permutations?").unwrap().unwrap();
        resolve(&ctx, 1, &[1], "", "user").unwrap();
        let t = list_text(&ctx, false);
        assert!(!t.contains("#1 "), "{t}");
        assert!(t.contains("#2 "), "{t}");
        assert!(t.contains("1 open."));
        let t = list_text(&ctx, true);
        assert!(t.contains("#1   resolved"), "{t}");
    }
}
