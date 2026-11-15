//! Agent reply schemas (ROADMAP §3.6, M2-05) and a small JSON-Schema validator.
//!
//! Each schema is defined once and used three ways: passed to the CLI's
//! `--json-schema`, shown in skills, and checked here before a reply is trusted.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde_json::{json, Value};

/// Reply kinds. Most roles have one; the team lead has two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reply {
    Worker,
    Falsifier,
    LeadPlan,
    LeadSummary,
    Orchestrator,
    Architect,
    Integrator,
    SpecWriter,
    Reviewer,
    Critic,
    Monitor,
}

impl Reply {
    pub const ALL: [Reply; 11] = [
        Reply::Worker,
        Reply::Falsifier,
        Reply::LeadPlan,
        Reply::LeadSummary,
        Reply::Orchestrator,
        Reply::Architect,
        Reply::Integrator,
        Reply::SpecWriter,
        Reply::Reviewer,
        Reply::Critic,
        Reply::Monitor,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Reply::Worker => "worker",
            Reply::Falsifier => "falsifier",
            Reply::LeadPlan => "lead_plan",
            Reply::LeadSummary => "lead_summary",
            Reply::Orchestrator => "orchestrator",
            Reply::Architect => "architect",
            Reply::Integrator => "integrator",
            Reply::SpecWriter => "spec_writer",
            Reply::Reviewer => "reviewer",
            Reply::Critic => "critic",
            Reply::Monitor => "monitor",
        }
    }

    pub fn schema(self) -> &'static Value {
        &SCHEMAS[&self]
    }
}

fn s() -> Value {
    json!({"type": "string"})
}
fn sa() -> Value {
    json!({"type": "array", "items": {"type": "string"}})
}
fn en(v: &[&str]) -> Value {
    json!({"type": "string", "enum": v})
}
fn obj(props: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let mut p = serde_json::Map::new();
    for (k, v) in props {
        p.insert(k.to_string(), v);
    }
    json!({"type": "object", "properties": p, "required": required, "additionalProperties": false})
}

fn amendment() -> Value {
    obj(
        vec![
            ("file", s()),
            ("old_text", s()),
            ("new_text", s()),
            ("justification", s()),
            ("evidence_kind", en(&["disproof", "counterexample", "argument"])),
            ("evidence_file", json!({"type": ["string", "null"]})),
            ("evidence_lean_name", json!({"type": ["string", "null"]})),
            ("alternatives_tried", s()),
        ],
        &[
            "file",
            "old_text",
            "new_text",
            "justification",
            "evidence_kind",
            "evidence_file",
            "evidence_lean_name",
            "alternatives_tried",
        ],
    )
}

fn plan_goal() -> Value {
    obj(
        vec![
            ("id", s()),
            ("kind", en(&["target", "lemma", "witness"])),
            ("lean_name", s()),
            ("statement", s()),
            ("file", s()),
            ("depends_on", sa()),
            ("parent_target", s()),
        ],
        &["id", "kind", "lean_name", "statement", "file", "depends_on", "parent_target"],
    )
}

static SCHEMAS: LazyLock<BTreeMap<Reply, Value>> = LazyLock::new(|| {
    let status = en(&["open", "claimed", "proved", "refuted", "blocked"]);
    let mut m = BTreeMap::new();
    m.insert(
        Reply::Worker,
        obj(
            vec![
                ("goal_id", s()),
                ("status", en(&["proved", "partial", "stuck"])),
                ("lean_names", sa()),
                (
                    "new_definitions",
                    json!({"type": "array", "items": obj(vec![("name", s()), ("witness", s())], &["name", "witness"])}),
                ),
                ("notes", s()),
                ("amendment", json!({"anyOf": [amendment(), {"type": "null"}]})),
            ],
            &["goal_id", "status", "lean_names", "new_definitions", "notes"],
        ),
    );
    m.insert(
        Reply::Falsifier,
        obj(
            vec![
                ("goal_id", s()),
                ("verdict", en(&["false", "plausible", "unknown"])),
                ("evidence", s()),
                ("lean_name", json!({"type": ["string", "null"]})),
            ],
            &["goal_id", "verdict", "evidence", "lean_name"],
        ),
    );
    m.insert(
        Reply::LeadPlan,
        obj(
            vec![
                ("refute", sa()),
                (
                    "shared",
                    obj(
                        vec![("definitions", sa()), ("lemma_statements", sa()), ("conventions", s())],
                        &["definitions", "lemma_statements", "conventions"],
                    ),
                ),
                (
                    "assignments",
                    json!({"type": "array", "items": obj(
                        vec![("goal_id", s()), ("file", s()), ("instructions", s()), ("existing_lemmas", sa())],
                        &["goal_id", "file", "instructions", "existing_lemmas"],
                    )}),
                ),
            ],
            &["refute", "shared", "assignments"],
        ),
    );
    m.insert(
        Reply::LeadSummary,
        obj(
            vec![
                ("summary", json!({"type": "string", "maxLength": 1200})),
                ("goal_status", json!({"type": "object", "additionalProperties": status})),
                ("notes", s()),
                ("bulletin", sa()),
                ("amendments", json!({"type": "array", "items": amendment()})),
                ("escalations", sa()),
            ],
            &["summary", "goal_status", "notes", "bulletin", "amendments", "escalations"],
        ),
    );
    m.insert(
        Reply::Orchestrator,
        obj(
            vec![
                (
                    "leads",
                    json!({"type": "array", "items": obj(
                        vec![("lead_id", s()), ("goal_ids", sa()), ("brief", s())],
                        &["lead_id", "goal_ids", "brief"],
                    )}),
                ),
                ("revise_plan", json!({"type": "boolean"})),
                ("integrate", json!({"type": "boolean"})),
                ("notes", s()),
            ],
            &["leads", "revise_plan", "integrate", "notes"],
        ),
    );
    m.insert(
        Reply::Architect,
        obj(
            vec![("goals", json!({"type": "array", "items": plan_goal()})), ("rationale", s())],
            &["goals", "rationale"],
        ),
    );
    m.insert(
        Reply::Integrator,
        obj(
            vec![
                (
                    "moved",
                    json!({"type": "array", "items": obj(
                        vec![("lean_name", s()), ("from", s()), ("to", s())],
                        &["lean_name", "from", "to"],
                    )}),
                ),
                ("removed_duplicates", sa()),
                ("notes", s()),
            ],
            &["moved", "removed_duplicates", "notes"],
        ),
    );
    m.insert(
        Reply::SpecWriter,
        obj(
            vec![
                ("files_written", sa()),
                (
                    "targets",
                    json!({"type": "array", "items": obj(
                        vec![("name", s()), ("kind", en(&["property", "nonvacuity", "mutation"]))],
                        &["name", "kind"],
                    )}),
                ),
                ("departures_added", json!({"type": "integer"})),
                ("notes", s()),
            ],
            &["files_written", "targets", "departures_added", "notes"],
        ),
    );
    m.insert(
        Reply::Reviewer,
        obj(
            vec![
                ("lens", en(&["soundness", "strength", "fidelity", "legibility"])),
                (
                    "findings",
                    json!({"type": "array", "items": obj(
                        vec![
                            ("severity", en(&["blocker", "major", "minor"])),
                            ("where", s()),
                            ("issue", s()),
                            ("suggestion", s()),
                        ],
                        &["severity", "where", "issue", "suggestion"],
                    )}),
                ),
                ("verdict", en(&["ok", "revise"])),
            ],
            &["lens", "findings", "verdict"],
        ),
    );
    m.insert(
        Reply::Critic,
        obj(
            vec![
                ("verdict", en(&["accept", "reject", "escalate"])),
                ("touches_trusted_assumptions", json!({"type": "boolean"})),
                ("closer_to_code", json!({"type": "boolean"})),
                ("reason", s()),
            ],
            &["verdict", "touches_trusted_assumptions", "closer_to_code", "reason"],
        ),
    );
    m.insert(
        Reply::Monitor,
        obj(
            vec![
                ("answer", s()),
                (
                    "skill_suggestions",
                    json!({"type": "array", "items": obj(vec![("skill", s()), ("change", s())], &["skill", "change"])}),
                ),
            ],
            &["answer", "skill_suggestions"],
        ),
    );
    m
});

fn type_ok(v: &Value, t: &str) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => false,
    }
}

/// Validate `v` against the subset of JSON Schema used in this file.
pub fn validate(v: &Value, schema: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    check(v, schema, "", &mut errs);
    errs
}

fn at(path: &str) -> String {
    if path.is_empty() {
        "(root)".to_string()
    } else {
        path.to_string()
    }
}

fn check(v: &Value, schema: &Value, path: &str, errs: &mut Vec<String>) {
    if let Some(alts) = schema.get("anyOf").and_then(Value::as_array) {
        if !alts.iter().any(|a| validate(v, a).is_empty()) {
            errs.push(format!("{}: matches none of the allowed shapes", at(path)));
        }
        return;
    }
    if let Some(t) = schema.get("type") {
        let ok = match t {
            Value::String(t) => type_ok(v, t),
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).any(|t| type_ok(v, t)),
            _ => true,
        };
        if !ok {
            errs.push(format!("{}: expected {}", at(path), t));
            return;
        }
    }
    if let Some(e) = schema.get("enum").and_then(Value::as_array) {
        if !e.contains(v) {
            let allowed: Vec<String> = e.iter().map(|x| x.to_string()).collect();
            errs.push(format!("{}: must be one of {}", at(path), allowed.join(", ")));
        }
    }
    if let (Some(max), Some(s)) = (schema.get("maxLength").and_then(Value::as_u64), v.as_str()) {
        if s.chars().count() as u64 > max {
            errs.push(format!("{}: longer than {max} characters", at(path)));
        }
    }
    if let Some(o) = v.as_object() {
        if let Some(req) = schema.get("required").and_then(Value::as_array) {
            for k in req.iter().filter_map(Value::as_str) {
                if !o.contains_key(k) {
                    errs.push(format!("{}: missing", join(path, k)));
                }
            }
        }
        let props = schema.get("properties").and_then(Value::as_object);
        let addl = schema.get("additionalProperties");
        for (k, val) in o {
            match props.and_then(|p| p.get(k)) {
                Some(ps) => check(val, ps, &join(path, k), errs),
                None => match addl {
                    Some(Value::Bool(false)) => {
                        errs.push(format!("{}: unexpected key", join(path, k)))
                    }
                    Some(sch @ Value::Object(_)) => check(val, sch, &join(path, k), errs),
                    _ => {}
                },
            }
        }
    }
    if let (Some(a), Some(items)) = (v.as_array(), schema.get("items")) {
        for (i, x) in a.iter().enumerate() {
            check(x, items, &format!("{path}[{i}]"), errs);
        }
    }
}

fn join(path: &str, k: &str) -> String {
    if path.is_empty() {
        k.to_string()
    } else {
        format!("{path}.{k}")
    }
}

/// Required top-level keys of a reply schema (used to keep skills in sync).
pub fn required_keys(r: Reply) -> Vec<&'static str> {
    r.schema()["required"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(r: Reply) -> Value {
        match r {
            Reply::Worker => json!({"goal_id":"g1","status":"proved","lean_names":["Apc.x"],"new_definitions":[],"notes":""}),
            Reply::Falsifier => json!({"goal_id":"g1","verdict":"unknown","evidence":"","lean_name":null}),
            Reply::LeadPlan => json!({"refute":[],"shared":{"definitions":[],"lemma_statements":[],"conventions":""},"assignments":[{"goal_id":"g1","file":"lean/Apc/Solution.lean","instructions":"","existing_lemmas":[]}]}),
            Reply::LeadSummary => json!({"summary":"s","goal_status":{"g1":"open"},"notes":"","bulletin":[],"amendments":[],"escalations":[]}),
            Reply::Orchestrator => json!({"leads":[{"lead_id":"L1","goal_ids":["g1"],"brief":""}],"revise_plan":false,"integrate":false,"notes":""}),
            Reply::Architect => json!({"goals":[{"id":"g1","kind":"target","lean_name":"Apc.t","statement":"theorem t : True","file":"lean/Apc/Solution.lean","depends_on":[],"parent_target":"Apc.t"}],"rationale":""}),
            Reply::Integrator => json!({"moved":[],"removed_duplicates":[],"notes":""}),
            Reply::SpecWriter => json!({"files_written":[],"targets":[{"name":"Apc.t","kind":"property"}],"departures_added":0,"notes":""}),
            Reply::Reviewer => json!({"lens":"strength","findings":[{"severity":"minor","where":"x","issue":"y","suggestion":"z"}],"verdict":"ok"}),
            Reply::Critic => json!({"verdict":"reject","touches_trusted_assumptions":false,"closer_to_code":false,"reason":""}),
            Reply::Monitor => json!({"answer":"a","skill_suggestions":[]}),
        }
    }

    #[test]
    fn valid_replies_pass_and_mutations_fail() {
        for r in Reply::ALL {
            let v = valid(r);
            assert!(validate(&v, r.schema()).is_empty(), "{:?}: {:?}", r, validate(&v, r.schema()));
            // missing a required key
            let mut m = v.clone();
            let k = required_keys(r)[0];
            m.as_object_mut().unwrap().remove(k);
            let e = validate(&m, r.schema());
            assert!(e.iter().any(|x| x.contains("missing")), "{r:?}: {e:?}");
            // extra key
            let mut m = v.clone();
            m.as_object_mut().unwrap().insert("zzz".into(), json!(1));
            assert!(!validate(&m, r.schema()).is_empty(), "{r:?}");
            // wrong type
            let m = json!([1, 2]);
            assert!(!validate(&m, r.schema()).is_empty(), "{r:?}");
        }
    }

    #[test]
    fn nested_paths_and_enums() {
        let v = json!({"refute":[],"shared":{"definitions":[],"lemma_statements":[],"conventions":""},"assignments":[{"goal_id":"g1","instructions":"","existing_lemmas":[]}]});
        let e = validate(&v, Reply::LeadPlan.schema());
        assert_eq!(e, vec!["assignments[0].file: missing".to_string()]);
        let v = json!({"goal_id":"g1","status":"done","lean_names":[],"new_definitions":[],"notes":""});
        let e = validate(&v, Reply::Worker.schema());
        assert!(e[0].starts_with("status: must be one of"));
        let long = "x".repeat(1300);
        let v = json!({"summary":long,"goal_status":{"g1":"nope"},"notes":"","bulletin":[],"amendments":[],"escalations":[]});
        let e = validate(&v, Reply::LeadSummary.schema());
        assert_eq!(e.len(), 2, "{e:?}");
    }

    #[test]
    fn worker_amendment_optional_and_checked() {
        let mut v = valid(Reply::Worker);
        v["amendment"] = Value::Null;
        assert!(validate(&v, Reply::Worker.schema()).is_empty());
        v["amendment"] = json!({"file":"x"});
        assert!(!validate(&v, Reply::Worker.schema()).is_empty());
    }
}
