//! App state and the event reducer (ROADMAP M4-02). `apply` is a pure function of
//! the state and one event, so the transcript can be rebuilt by replaying the log.

use std::collections::BTreeMap;

use crate::events::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A stage or round divider.
    Rule,
    Info,
    Good,
    Bad,
    Warn,
    /// Something the user typed.
    User,
    /// Output of a command or an answer, shown as a block.
    Output,
    /// A model's words (lead summaries, monitor answers).
    Voice,
    Muted,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub time: String,
    pub kind: Kind,
    pub who: String,
    pub text: String,
}

impl Line {
    pub fn new(kind: Kind, who: &str, text: impl Into<String>) -> Self {
        Line { time: clock_now(), kind, who: who.to_string(), text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub id: String,
    pub role: String,
    pub backend: String,
    pub goal: String,
    pub started: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Health {
    pub paused_until: Option<String>,
    pub paused_seconds: f64,
    pub unavailable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub campaign: String,
    pub phase: String,
    pub round: u32,
    pub stage: String,
    pub targets: BTreeMap<String, (bool, Vec<String>)>,
    pub goals: BTreeMap<String, usize>,
    pub agents: BTreeMap<String, AgentRow>,
    pub backends: BTreeMap<String, Health>,
    pub transcript: Vec<Line>,
    pub engine: Option<u32>,
    pub engine_started: Option<String>,
    pub paused: bool,
    pub cost: f64,
    pub calls: u64,
    pub seen_agents: Vec<String>,
    pub busy: Vec<String>,
}

pub const MAX_LINES: usize = 2000;

pub fn clock_now() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!("{:02}:{:02}", now.hour(), now.minute())
}

/// "HH:MM" from an RFC 3339 timestamp, in local time where possible.
pub fn clock_of(ts: &str) -> String {
    match time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339) {
        Ok(t) => {
            let t = time::UtcOffset::current_local_offset().map(|o| t.to_offset(o)).unwrap_or(t);
            format!("{:02}:{:02}", t.hour(), t.minute())
        }
        Err(_) => clock_now(),
    }
}

pub fn seconds_since(ts: &str) -> Option<f64> {
    let t = time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339).ok()?;
    Some((time::OffsetDateTime::now_utc() - t).as_seconds_f64().max(0.0))
}

pub fn human_secs(s: f64) -> String {
    let s = s as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

impl App {
    pub fn new(campaign: &str) -> Self {
        App {
            campaign: campaign.to_string(),
            phase: "init".into(),
            round: 0,
            stage: "idle".into(),
            targets: BTreeMap::new(),
            goals: BTreeMap::new(),
            agents: BTreeMap::new(),
            backends: BTreeMap::new(),
            transcript: Vec::new(),
            engine: None,
            engine_started: None,
            paused: false,
            cost: 0.0,
            calls: 0,
            seen_agents: Vec::new(),
            busy: Vec::new(),
        }
    }

    pub fn push(&mut self, l: Line) {
        self.transcript.push(l);
        if self.transcript.len() > MAX_LINES {
            let n = self.transcript.len() - MAX_LINES;
            self.transcript.drain(..n);
        }
    }

    pub fn say(&mut self, kind: Kind, who: &str, text: impl Into<String>) {
        self.push(Line::new(kind, who, text));
    }

    /// Load the durable parts from state.json.
    pub fn load_state(&mut self, st: &crate::paths::State) {
        self.phase = st.phase.clone();
        self.round = st.round;
        self.stage = st.stage.clone();
        self.targets = st.targets.iter().map(|(k, t)| (k.clone(), (t.accepted, t.reasons.clone()))).collect();
        let mut goals = BTreeMap::new();
        for g in st.goals.values() {
            *goals.entry(g.status.clone()).or_insert(0) += 1;
        }
        self.goals = goals;
        self.cost = st.usage.values().flat_map(|m| m.values()).map(|u| u.cost_usd).sum();
        self.calls = st.usage.values().flat_map(|m| m.values()).map(|u| u.calls).sum();
    }

    pub fn running_on(&self, backend: &str) -> usize {
        self.agents.values().filter(|a| a.backend == backend).count()
    }

    pub fn apply(&mut self, e: &Event) {
        let time = clock_of(&e.ts);
        let line = |kind: Kind, who: &str, text: String| Line { time: time.clone(), kind, who: who.to_string(), text };
        if e.round > self.round {
            self.round = e.round;
        }
        let l: Option<Line> = match e.kind.as_str() {
            "engine_started" => {
                self.engine = e.u64("pid").map(|p| p as u32);
                self.engine_started = Some(e.ts.clone());
                self.agents.clear();
                self.paused = false;
                Some(line(Kind::Info, "engine", format!("started ({} mode)", if e.str("mode").is_empty() { "team" } else { e.str("mode") })))
            }
            "engine_stopped" => {
                self.engine = None;
                self.agents.clear();
                let ok = e.u64("code") == Some(0);
                Some(line(if ok { Kind::Good } else { Kind::Warn }, "engine", format!("stopped: {}", e.str("reason"))))
            }
            "engine_killed" => {
                self.engine = None;
                self.agents.clear();
                Some(line(Kind::Warn, "engine", format!("killed (pid {})", e.u64("pid").unwrap_or(0))))
            }
            "stage" => {
                self.stage = e.str("stage").to_string();
                if self.phase == "approved" {
                    self.phase = "proving".into();
                }
                Some(line(Kind::Rule, "", format!("round {} · {}", e.round, self.stage.replace('_', " "))))
            }
            "agent_started" => {
                let id = e.str("agent").to_string();
                if !self.seen_agents.contains(&id) {
                    self.seen_agents.push(id.clone());
                }
                self.agents.insert(
                    id.clone(),
                    AgentRow { id, role: e.str("role").into(), backend: String::new(), goal: e.str("goal").into(), started: e.ts.clone() },
                );
                None
            }
            "agent_backend" => {
                if let Some(a) = self.agents.get_mut(e.str("agent")) {
                    a.backend = e.str("backend").into();
                }
                None
            }
            "agent_finished" => {
                let id = e.str("agent").to_string();
                self.agents.remove(&id);
                self.cost += e.f64("cost_usd").unwrap_or(0.0);
                self.calls += 1;
                let secs = e.f64("seconds").map(human_secs).unwrap_or_default();
                let goal = if e.str("goal").is_empty() { String::new() } else { format!(" · {}", e.str("goal")) };
                let backend = e.str("backend");
                match e.data.get("failure").and_then(|f| f.as_str()) {
                    Some(f) => Some(line(Kind::Bad, &id, format!("{backend}{goal} · {secs} · failed: {f}"))),
                    None => {
                        let cost = e.f64("cost_usd").filter(|c| *c > 0.0).map(|c| format!(" · ${c:.2}")).unwrap_or_default();
                        Some(line(Kind::Muted, &id, format!("{backend}{goal} · {secs}{cost}")))
                    }
                }
            }
            "gate" => {
                let before: BTreeMap<String, bool> = self.targets.iter().map(|(k, v)| (k.clone(), v.0)).collect();
                if let Some(ts) = e.data.get("targets").and_then(|t| t.as_object()) {
                    self.targets = ts
                        .iter()
                        .map(|(k, v)| {
                            let acc = v["accepted"].as_bool().unwrap_or(false);
                            let rs = v["reasons"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                            (k.clone(), (acc, rs))
                        })
                        .collect();
                }
                let newly: Vec<String> = self.targets.iter().filter(|(k, v)| v.0 && !before.get(*k).copied().unwrap_or(false)).map(|(k, _)| k.clone()).collect();
                let acc = e.u64("accepted").unwrap_or(0);
                let total = e.u64("total").unwrap_or(0);
                let mut text = format!("{acc}/{total} targets accepted");
                if !newly.is_empty() {
                    text.push_str(&format!(" · newly accepted: {}", newly.join(", ")));
                }
                let kind = if total > 0 && acc == total { Kind::Good } else { Kind::Info };
                Some(line(kind, "gate", text))
            }
            "violation" => Some(line(Kind::Warn, "policy", format!("reverted an out-of-scope edit ({}) to {}", e.str("change"), e.str("path")))),
            "goal" => {
                let s = e.str("status");
                let kind = match s {
                    "proved" => Kind::Good,
                    "refuted" => Kind::Warn,
                    _ => Kind::Muted,
                };
                let why = if e.str("reason").is_empty() { String::new() } else { format!(": {}", e.str("reason")) };
                Some(line(kind, "goal", format!("{} {s}{why}", e.str("id"))))
            }
            "bulletin" => Some(line(Kind::Info, "bulletin", e.str("text").to_string())),
            "lead_summary" => Some(line(Kind::Voice, e.str("lead"), e.str("summary").to_string())),
            "leads" => {
                let n = e.data.get("leads").and_then(|l| l.as_array()).map(|a| a.len()).unwrap_or(0);
                Some(line(Kind::Muted, "orchestrator", format!("{n} lead(s) this round")))
            }
            "plan" => Some(line(Kind::Info, "architect", format!("plan revision {} · {} goals", e.u64("revision").unwrap_or(0), e.u64("goals").unwrap_or(0)))),
            "backend_paused" => {
                let b = e.str("backend").to_string();
                let secs = e.f64("seconds").unwrap_or(0.0);
                let h = self.backends.entry(b.clone()).or_default();
                h.paused_until = Some(e.ts.clone());
                h.paused_seconds = secs;
                Some(line(Kind::Warn, &b, format!("usage limit reached; paused for {}", human_secs(secs))))
            }
            "backend_resumed" => {
                let b = e.str("backend").to_string();
                self.backends.entry(b.clone()).or_default().paused_until = None;
                Some(line(Kind::Good, &b, "back from its usage limit".to_string()))
            }
            "amendment" => Some(line(Kind::Warn, "amend", format!("amendment {} is {}", e.u64("n").unwrap_or(0), e.str("status")))),
            "escalation" => {
                let from = e.str("from");
                let from = if from.is_empty() { String::new() } else { format!(" ({from})") };
                Some(line(Kind::Bad, "escalation", format!("#{}{from} needs you: {} · /resolve {} [answer]", e.u64("n").unwrap_or(0), e.str("text"), e.u64("n").unwrap_or(0))))
            }
            "escalation_resolved" => Some(line(
                Kind::Good,
                "escalation",
                format!("#{} {}", e.u64("n").unwrap_or(0), if e.data.get("answered").and_then(serde_json::Value::as_bool).unwrap_or(false) { "answered; the answer goes to the orchestrator" } else { "dismissed" }),
            )),
            "paused" => {
                self.paused = true;
                None
            }
            "resumed" => {
                self.paused = false;
                None
            }
            "round_done" => {
                self.stage = "idle".into();
                None
            }
            "note" => {
                let t = e.str("text");
                if t.contains("not logged in") {
                    for b in ["claude", "gemini"] {
                        if t.starts_with(b) {
                            self.backends.entry(b.into()).or_default().unavailable = true;
                        }
                    }
                }
                Some(line(Kind::Muted, "", t.to_string()))
            }
            _ => None,
        };
        if let Some(l) = l {
            self.push(l);
        }
    }

    /// Whether a backend is paused right now.
    pub fn is_paused(&self, backend: &str) -> Option<f64> {
        let h = self.backends.get(backend)?;
        let since = seconds_since(h.paused_until.as_deref()?)?;
        let left = h.paused_seconds - since;
        (left > 0.0).then_some(left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(kind: &str, data: serde_json::Value) -> Event {
        Event::new(2, kind, data)
    }

    #[test]
    fn reducer_handles_every_event_kind() {
        let mut a = App::new("demo");
        a.apply(&ev("engine_started", json!({"pid": 42, "mode": "team"})));
        assert_eq!(a.engine, Some(42));
        a.apply(&ev("stage", json!({"stage": "gate_pre"})));
        assert_eq!(a.stage, "gate_pre");
        assert_eq!(a.transcript.last().unwrap().kind, Kind::Rule);
        a.apply(&ev("agent_started", json!({"agent": "worker-1", "role": "worker", "goal": "t1"})));
        a.apply(&ev("agent_backend", json!({"agent": "worker-1", "backend": "gemini"})));
        assert_eq!(a.running_on("gemini"), 1);
        a.apply(&ev("agent_finished", json!({"agent": "worker-1", "backend": "gemini", "goal": "t1", "failure": null, "cost_usd": 0.25, "seconds": 75.0})));
        assert!(a.agents.is_empty());
        assert!(a.transcript.last().unwrap().text.contains("1m15s · $0.25"));
        a.apply(&ev("agent_finished", json!({"agent": "critic-2", "backend": "claude", "failure": "timeout", "seconds": 3.0})));
        assert_eq!(a.transcript.last().unwrap().kind, Kind::Bad);
        a.apply(&ev("gate", json!({"accepted": 1, "total": 2, "targets": {"Apc.a": {"accepted": true, "reasons": []}, "Apc.b": {"accepted": false, "reasons": ["bad_axioms"]}}})));
        assert!(a.transcript.last().unwrap().text.contains("newly accepted: Apc.a"));
        assert_eq!(a.targets["Apc.b"].1, vec!["bad_axioms".to_string()]);
        a.apply(&ev("violation", json!({"path": "lean/Apc/Spec/Defs.lean", "change": "modified"})));
        assert_eq!(a.transcript.last().unwrap().kind, Kind::Warn);
        a.apply(&ev("goal", json!({"id": "g2", "status": "proved"})));
        a.apply(&ev("bulletin", json!({"text": "L1: use List.Perm"})));
        a.apply(&ev("lead_summary", json!({"lead": "L1", "summary": "two lemmas left"})));
        assert_eq!(a.transcript.last().unwrap().who, "L1");
        a.apply(&ev("leads", json!({"leads": [1, 2]})));
        a.apply(&ev("plan", json!({"revision": 2, "goals": 7})));
        a.apply(&ev("backend_paused", json!({"backend": "claude", "seconds": 120.0})));
        assert!(a.is_paused("claude").is_some());
        a.apply(&ev("backend_resumed", json!({"backend": "claude"})));
        assert!(a.is_paused("claude").is_none());
        a.apply(&ev("amendment", json!({"n": 1, "status": "pending"})));
        a.apply(&ev("escalation", json!({"n": 1, "text": "potential bug"})));
        a.apply(&ev("paused", json!({})));
        assert!(a.paused);
        a.apply(&ev("resumed", json!({})));
        assert!(!a.paused);
        a.apply(&ev("note", json!({"text": "gemini is not logged in; it is unavailable"})));
        assert!(a.backends["gemini"].unavailable);
        a.apply(&ev("round_done", json!({})));
        assert_eq!(a.stage, "idle");
        a.apply(&ev("engine_stopped", json!({"reason": "all targets accepted", "code": 0})));
        assert_eq!(a.engine, None);
        assert_eq!(a.transcript.last().unwrap().kind, Kind::Good);
        a.apply(&ev("something_new", json!({})));
    }

    #[test]
    fn transcript_is_capped() {
        let mut a = App::new("x");
        for i in 0..(MAX_LINES + 50) {
            a.say(Kind::Info, "", format!("{i}"));
        }
        assert_eq!(a.transcript.len(), MAX_LINES);
        assert_eq!(a.transcript[0].text, "50");
    }

    #[test]
    fn human_seconds() {
        assert_eq!(human_secs(5.0), "5s");
        assert_eq!(human_secs(125.0), "2m05s");
        assert_eq!(human_secs(7300.0), "2h01m");
    }
}
