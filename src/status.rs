//! Plain-text campaign summary (ROADMAP M3-08). No model calls.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::error::Result;
use crate::events;
use crate::paths::{Ctx, State};
use crate::util;

pub struct Summary {
    pub state: State,
    pub engine_pid: Option<u32>,
    pub bulletin_tail: Vec<String>,
    pub amendments: Vec<String>,
    pub escalations: Vec<String>,
}

fn list_dir(p: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(p)
        .map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".md")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub fn gather(ctx: &Ctx) -> Result<Summary> {
    let state = util::load_state(&ctx.campaign)?;
    let bulletin = std::fs::read_to_string(ctx.campaign.bulletin_file()).unwrap_or_default();
    let mut tail: Vec<String> = bulletin.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect();
    if tail.len() > 5 {
        tail.drain(..tail.len() - 5);
    }
    Ok(Summary {
        state,
        engine_pid: events::engine_running(ctx),
        bulletin_tail: tail,
        amendments: list_dir(&ctx.campaign.amendments_dir()),
        escalations: list_dir(&ctx.campaign.escalations_dir()),
    })
}

pub fn render_summary(s: &Summary) -> String {
    let st = &s.state;
    let mut o = String::new();
    let engine = match s.engine_pid {
        Some(p) => format!("running (pid {p})"),
        None => "not running".into(),
    };
    let _ = writeln!(o, "phase {} · round {} · stage {} · engine {engine}", st.phase, st.round, st.stage);
    let acc = st.targets.values().filter(|t| t.accepted).count();
    let _ = writeln!(o, "\ntargets: {acc}/{} accepted", st.targets.len());
    for (n, t) in &st.targets {
        if t.accepted {
            let since = t.since_round.map(|r| format!(" (since round {r})")).unwrap_or_default();
            let _ = writeln!(o, "  ✓ {n}{since}");
        } else {
            let why = if t.reasons.is_empty() { "not checked yet".into() } else { t.reasons.join(", ") };
            let _ = writeln!(o, "  ✗ {n}: {why}");
        }
    }
    if !st.goals.is_empty() {
        let mut by: BTreeMap<&str, usize> = BTreeMap::new();
        for g in st.goals.values() {
            *by.entry(g.status.as_str()).or_default() += 1;
        }
        let parts: Vec<String> = by.iter().map(|(k, v)| format!("{v} {k}")).collect();
        let _ = writeln!(o, "\ngoals: {}", parts.join(", "));
    }
    if !st.leads.is_empty() {
        let _ = writeln!(o, "\nleads:");
        for (id, l) in &st.leads {
            let line: String = l.summary.lines().next().unwrap_or("").chars().take(110).collect();
            let _ = writeln!(o, "  {id} ({} goals, round {}): {line}", l.goal_ids.len(), l.last_round);
        }
    }
    if !s.amendments.is_empty() || !s.escalations.is_empty() {
        let _ = writeln!(o, "\namendments: {} · escalations: {}", s.amendments.len(), s.escalations.len());
        for e in &s.escalations {
            let _ = writeln!(o, "  ! {e}");
        }
    }
    if !st.usage.is_empty() {
        let _ = writeln!(o, "\nusage:");
        let mut tot_cost = 0.0;
        let mut tot_calls = 0;
        for (role, m) in &st.usage {
            for (b, u) in m {
                tot_cost += u.cost_usd;
                tot_calls += u.calls;
                let _ = writeln!(o, "  {role:<13} {b:<7} {:>4} calls  {:>8.1}s  ${:.3}", u.calls, u.seconds, u.cost_usd);
            }
        }
        let _ = writeln!(o, "  {:<13} {:<7} {tot_calls:>4} calls             ${tot_cost:.3}", "total", "");
    }
    if !s.bulletin_tail.is_empty() {
        let _ = writeln!(o, "\nbulletin:");
        for b in &s.bulletin_tail {
            let _ = writeln!(o, "  {}", b.trim_start_matches("- "));
        }
    }
    o
}

pub fn render(ctx: &Ctx) -> Result<String> {
    Ok(render_summary(&gather(ctx)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{BackendUsage, LeadState, TargetState};

    #[test]
    fn renders_fixture_state() {
        let mut st = State::new_init();
        st.phase = "proving".into();
        st.round = 3;
        st.stage = "leads".into();
        st.targets.insert("Apc.a".into(), TargetState { accepted: true, reasons: vec![], since_round: Some(2) });
        st.targets.insert("Apc.b".into(), TargetState { accepted: false, reasons: vec!["bad_axioms".into()], since_round: None });
        st.leads.insert("L1".into(), LeadState { goal_ids: vec!["g1".into()], summary: "working on b".into(), last_round: 3 });
        st.usage.entry("worker".into()).or_default().insert("claude".into(), BackendUsage { calls: 2, cost_usd: 0.5, seconds: 12.0 });
        let s = Summary { state: st, engine_pid: None, bulletin_tail: vec!["- round 3 · L1: try induction".into()], amendments: vec![], escalations: vec![] };
        let out = render_summary(&s);
        assert!(out.starts_with("phase proving · round 3 · stage leads · engine not running\n"));
        assert!(out.contains("targets: 1/2 accepted"));
        assert!(out.contains("  ✓ Apc.a (since round 2)"));
        assert!(out.contains("  ✗ Apc.b: bad_axioms"));
        assert!(out.contains("L1 (1 goals, round 3): working on b"));
        assert!(out.contains("round 3 · L1: try induction"));
        assert!(out.contains("$0.500"));
    }
}
