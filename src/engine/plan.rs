//! The architect's plan (ROADMAP §3.4, M3-02).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::paths::State;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    pub id: String,
    pub kind: String,
    pub lean_name: String,
    pub statement: String,
    pub file: String,
    pub depends_on: Vec<String>,
    pub parent_target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u32,
    pub revision: u32,
    pub goals: Vec<Goal>,
}

impl Plan {
    pub fn goal(&self, id: &str) -> Option<&Goal> {
        self.goals.iter().find(|g| g.id == id)
    }
}

pub fn valid_file(f: &str) -> bool {
    f == "lean/Apc/Solution.lean"
        || (f.starts_with("lean/Apc/Proof/") && f.ends_with(".lean") && !f.contains(".."))
}

/// Problems with a plan; empty when it is usable.
pub fn validate(goals: &[Goal], state: &State) -> Vec<String> {
    let mut errs = Vec::new();
    let mut ids = BTreeSet::new();
    for g in goals {
        if !ids.insert(g.id.as_str()) {
            errs.push(format!("duplicate goal id {}", g.id));
        }
        if !matches!(g.kind.as_str(), "target" | "lemma" | "witness") {
            errs.push(format!("{}: kind must be target, lemma or witness", g.id));
        }
        if !g.lean_name.starts_with("Apc.") {
            errs.push(format!("{}: lean_name must start with Apc.", g.id));
        }
        if !valid_file(&g.file) {
            errs.push(format!("{}: file must be lean/Apc/Solution.lean or under lean/Apc/Proof/", g.id));
        }
    }
    for g in goals {
        for d in &g.depends_on {
            if !ids.contains(d.as_str()) {
                errs.push(format!("{}: depends on unknown goal {d}", g.id));
            }
        }
    }
    // Cycles.
    let deps: BTreeMap<&str, Vec<&str>> =
        goals.iter().map(|g| (g.id.as_str(), g.depends_on.iter().map(String::as_str).collect())).collect();
    let mut state_of: BTreeMap<&str, u8> = BTreeMap::new();
    fn visit<'a>(n: &'a str, deps: &BTreeMap<&'a str, Vec<&'a str>>, st: &mut BTreeMap<&'a str, u8>) -> bool {
        match st.get(n) {
            Some(1) => return false,
            Some(2) => return true,
            _ => {}
        }
        st.insert(n, 1);
        for d in deps.get(n).map(|v| v.as_slice()).unwrap_or(&[]) {
            if deps.contains_key(d) && !visit(d, deps, st) {
                return false;
            }
        }
        st.insert(n, 2);
        true
    }
    for g in goals {
        if !visit(&g.id, &deps, &mut state_of) {
            errs.push(format!("dependency cycle through {}", g.id));
            break;
        }
    }
    for t in state.targets.keys() {
        let n = goals.iter().filter(|g| g.kind == "target" && &g.lean_name == t).count();
        if n != 1 {
            errs.push(format!("target {t} must have exactly one goal of kind target (has {n})"));
        }
    }
    errs
}

/// Engine-created goal that makes an orphaned file (one no goal owns) compile again.
pub const REPAIR_PREFIX: &str = "repair_";

pub fn repair_goal(module: &str, file: &str, parent: &str) -> Goal {
    Goal {
        id: format!("{REPAIR_PREFIX}{}", module.replace('.', "_")),
        kind: "repair".into(),
        lean_name: module.to_string(),
        statement: format!(
            "(repair) {file} is imported by the solution but is owned by no goal and does not build cleanly. Make it compile with no errors and no sorry. Keep the statements other modules use."
        ),
        file: file.to_string(),
        depends_on: vec![],
        parent_target: parent.to_string(),
    }
}

/// `plan` with its repair goals replaced by `repairs`.
pub fn with_repairs(plan: &Plan, repairs: Vec<Goal>) -> Plan {
    let mut goals: Vec<Goal> = plan.goals.iter().filter(|g| g.kind != "repair").cloned().collect();
    goals.extend(repairs);
    Plan { goals, ..plan.clone() }
}

/// Merge a new plan into the state's goals: new goals start open, removed goals are
/// dropped, proved goals keep their status if name and statement are unchanged.
pub fn merge_into_state(old: Option<&Plan>, new: &Plan, state: &mut State) {
    let mut goals = BTreeMap::new();
    for g in &new.goals {
        let keep = old
            .and_then(|o| o.goal(&g.id))
            .filter(|og| og.lean_name == g.lean_name && og.statement == g.statement)
            .and_then(|_| state.goals.get(&g.id).cloned());
        let gs = keep.unwrap_or(crate::paths::GoalState {
            status: "open".into(),
            lean_name: g.lean_name.clone(),
            file: g.file.clone(),
            lead: None,
            verified_round: None,
        });
        goals.insert(g.id.clone(), gs);
    }
    state.goals = goals;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{State, TargetState};

    fn g(id: &str, kind: &str, name: &str, deps: &[&str]) -> Goal {
        Goal {
            id: id.into(),
            kind: kind.into(),
            lean_name: name.into(),
            statement: format!("theorem {name} : True"),
            file: "lean/Apc/Solution.lean".into(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            parent_target: "Apc.t".into(),
        }
    }

    fn st() -> State {
        let mut s = State::new_init();
        s.targets.insert("Apc.t".into(), TargetState { accepted: false, reasons: vec![], since_round: None });
        s
    }

    #[test]
    fn validation_rules() {
        let s = st();
        assert!(validate(&[g("g1", "target", "Apc.t", &[])], &s).is_empty());
        let e = validate(&[g("g1", "lemma", "Apc.l", &["g9"])], &s);
        assert!(e.iter().any(|x| x.contains("unknown goal g9")));
        assert!(e.iter().any(|x| x.contains("exactly one goal of kind target")));
        let e = validate(&[g("g1", "target", "Apc.t", &["g2"]), g("g2", "lemma", "Apc.l", &["g1"])], &s);
        assert!(e.iter().any(|x| x.contains("cycle")));
        let mut bad = g("g1", "target", "Apc.t", &[]);
        bad.file = "lean/Apc/Spec/Defs.lean".into();
        assert!(!validate(&[bad], &s).is_empty());
        let e = validate(&[g("g1", "target", "Apc.t", &[]), g("g1", "lemma", "Mathlib.x", &[])], &s);
        assert!(e.iter().any(|x| x.contains("duplicate")));
        assert!(e.iter().any(|x| x.contains("Apc.")));
    }

    #[test]
    fn merge_keeps_proved_unchanged_goals() {
        let mut s = st();
        let p1 = Plan { version: 1, revision: 1, goals: vec![g("g1", "target", "Apc.t", &[]), g("g2", "lemma", "Apc.l", &[])] };
        merge_into_state(None, &p1, &mut s);
        s.goals.get_mut("g2").unwrap().status = "proved".into();
        let mut p2 = p1.clone();
        p2.revision = 2;
        p2.goals.push(g("g3", "lemma", "Apc.m", &[]));
        merge_into_state(Some(&p1), &p2, &mut s);
        assert_eq!(s.goals["g2"].status, "proved");
        assert_eq!(s.goals["g3"].status, "open");
        let mut p3 = p2.clone();
        p3.goals[1].statement = "theorem Apc.l : False".into();
        merge_into_state(Some(&p2), &p3, &mut s);
        assert_eq!(s.goals["g2"].status, "open");
    }
}
