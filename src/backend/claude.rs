//! Claude Code CLI backend (facts C1–C11).

use std::process::Command;
use std::time::Instant;

use serde_json::Value;

use super::{looks_like_not_logged_in, looks_like_quota, scrub_env, AgentCall, AgentResult, Backend, Failure, Profile};
use crate::config::BackendKind;
use crate::paths::Ctx;
use crate::proc;

/// Search permissions every tool-using role gets.
const SEARCH_RULES: [&str; 5] = ["Bash(vdash search *)", "Bash(grep *)", "Bash(rg *)", "WebSearch", "WebFetch"];

pub struct ClaudeCli {
    pub command: Vec<String>,
}

impl ClaudeCli {
    pub fn new(command: Vec<String>) -> Self {
        ClaudeCli { command }
    }

    /// The argv after the program name. Public for tests.
    pub fn args(call: &AgentCall) -> Vec<String> {
        let mut a: Vec<String> = vec!["-p".into(), "--output-format".into(), "json".into()];
        if let Some(m) = &call.model {
            a.push("--model".into());
            a.push(m.clone());
        }
        a.push("--append-system-prompt".into());
        a.push(call.skill.clone());
        a.push("--json-schema".into());
        a.push(call.schema.to_string());
        match call.profile {
            Profile::None => {
                a.push("--tools".into());
                a.push(String::new());
            }
            // Mathlib lives outside the campaign, where `--restricted` blocks the Grep
            // and Read tools; plain `grep` and `vdash search` through Bash can read it
            // (fact C10). WebSearch and WebFetch work headless (C11).
            Profile::Readonly => {
                a.push("--tools".into());
                a.push("Read,Glob,Grep,Bash,WebSearch,WebFetch".into());
                a.push("--allowedTools".into());
                for r in SEARCH_RULES {
                    a.push(r.into());
                }
            }
            Profile::Lean => {
                a.push("--tools".into());
                a.push("Read,Glob,Grep,Edit,Write,Bash,WebSearch,WebFetch".into());
                a.push("--allowedTools".into());
                for r in ["Bash(vdash check *)", "Bash(vdash axioms *)", "Edit", "Write"].into_iter().chain(SEARCH_RULES) {
                    a.push(r.into());
                }
            }
        }
        for f in [
            "--permission-mode",
            "dontAsk",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--no-session-persistence",
            "--restricted",
        ] {
            a.push(f.into());
        }
        a
    }
}

/// Parse one `claude -p --output-format json` result (fact C2).
pub fn parse(stdout: &str) -> AgentResult {
    let mut r = AgentResult { raw: stdout.to_string(), ..Default::default() };
    let v: Value = match serde_json::from_str(stdout.trim()) {
        Ok(v @ Value::Object(_)) => v,
        _ => {
            r.failure = Some(if looks_like_not_logged_in(stdout) {
                Failure::NotLoggedIn
            } else if looks_like_quota(stdout) {
                Failure::Quota
            } else {
                Failure::Crash
            });
            return r;
        }
    };
    r.text = v.get("result").and_then(Value::as_str).unwrap_or("").to_string();
    r.structured = v.get("structured_output").filter(|s| s.is_object()).cloned();
    r.cost_usd = v.get("total_cost_usd").and_then(Value::as_f64);
    r.tokens = v
        .get("usage")
        .map(|u| ["input_tokens", "output_tokens"].iter().filter_map(|k| u.get(k)?.as_u64()).sum());
    if let Some(d) = v.get("permission_denials").and_then(Value::as_array) {
        for x in d {
            let tool = x.get("tool_name").and_then(Value::as_str).unwrap_or("?");
            let input = x.get("tool_input").map(|i| {
                i.get("command")
                    .or_else(|| i.get("file_path"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| i.to_string())
            });
            r.denials.push(format!("{tool}: {}", input.unwrap_or_default()));
        }
    }
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let status = v.get("api_error_status").and_then(Value::as_u64);
    if matches!(status, Some(429) | Some(529)) || (is_error && looks_like_quota(&r.text)) {
        r.failure = Some(Failure::Quota);
    } else if is_error && looks_like_not_logged_in(&r.text) {
        r.failure = Some(Failure::NotLoggedIn);
    } else if is_error {
        let sub = v.get("subtype").and_then(Value::as_str).unwrap_or("error");
        r.failure = Some(Failure::AgentError(format!("{sub}: {}", crate::util::clip(&r.text, 300, false))));
    }
    r
}

impl Backend for ClaudeCli {
    fn kind(&self) -> BackendKind {
        BackendKind::Claude
    }

    fn run(&self, ctx: &Ctx, call: &AgentCall) -> AgentResult {
        let start = Instant::now();
        let (prog, pre) = self.command.split_first().map(|(p, r)| (p.clone(), r.to_vec())).unwrap_or(("claude".into(), vec![]));
        let args = Self::args(call);
        let mut cmd = Command::new(&prog);
        cmd.args(&pre).args(&args).current_dir(&call.cwd);
        scrub_env(&mut cmd, ctx);
        let shown: Vec<String> = std::iter::once(prog.clone())
            .chain(pre.iter().cloned())
            .chain(args.iter().map(|a| {
                if *a == call.skill {
                    format!("<skill sha256:{}>", super::hash_short(a))
                } else {
                    a.clone()
                }
            }))
            .collect();
        let mut r = match proc::run(&mut cmd, Some(call.prompt.as_bytes()), call.timeout) {
            Ok(out) if out.timed_out => AgentResult { failure: Some(Failure::Timeout), raw: out.stdout_lossy(), ..Default::default() },
            Ok(out) => {
                let mut r = parse(&out.stdout_lossy());
                if r.failure == Some(Failure::Crash) {
                    let err = out.stderr_lossy();
                    if looks_like_not_logged_in(&err) {
                        r.failure = Some(Failure::NotLoggedIn);
                    } else if looks_like_quota(&err) {
                        r.failure = Some(Failure::Quota);
                    }
                    r.raw.push_str(&err);
                }
                r
            }
            Err(e) => AgentResult { failure: Some(Failure::Crash), raw: e.message, ..Default::default() },
        };
        r.backend = Some(BackendKind::Claude);
        r.model = call.model.clone();
        r.seconds = start.elapsed().as_secs_f64();
        r.argv = shown;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_with_structured_output() {
        let s = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"answer\":4}","structured_output":{"answer":4},"total_cost_usd":0.0008,"permission_denials":[{"tool_name":"Bash","tool_input":{"command":"touch x"}}],"api_error_status":null}"#;
        let r = parse(s);
        assert_eq!(r.failure, None);
        assert_eq!(r.structured.unwrap()["answer"], 4);
        assert_eq!(r.cost_usd, Some(0.0008));
        assert_eq!(r.denials, vec!["Bash: touch x".to_string()]);
    }

    #[test]
    fn classifies_failures() {
        assert_eq!(parse("not json at all").failure, Some(Failure::Crash));
        let q = r#"{"is_error":true,"subtype":"error","result":"Claude AI usage limit reached","api_error_status":null}"#;
        assert_eq!(parse(q).failure, Some(Failure::Quota));
        let q2 = r#"{"is_error":true,"result":"x","api_error_status":529}"#;
        assert_eq!(parse(q2).failure, Some(Failure::Quota));
        let l = r#"{"is_error":true,"result":"Invalid API key · Please run /login"}"#;
        assert_eq!(parse(l).failure, Some(Failure::NotLoggedIn));
        let e = r#"{"is_error":true,"subtype":"error_max_turns","result":"stopped"}"#;
        assert!(matches!(parse(e).failure, Some(Failure::AgentError(_))));
    }

    #[test]
    fn profiles_map_to_flags() {
        let mut call = AgentCall {
            role: crate::config::Role::Critic,
            prompt: "p".into(),
            skill: "s".into(),
            schema: serde_json::json!({"type":"object"}),
            cwd: ".".into(),
            profile: Profile::None,
            model: Some("haiku".into()),
            timeout: std::time::Duration::from_secs(1),
        };
        let a = ClaudeCli::args(&call);
        let i = a.iter().position(|x| x == "--tools").unwrap();
        assert_eq!(a[i + 1], "");
        assert!(a.contains(&"--restricted".to_string()));
        assert!(!a.contains(&"--bare".to_string()));
        call.profile = Profile::Lean;
        let a = ClaudeCli::args(&call);
        for r in ["Bash(vdash check *)", "Bash(vdash search *)", "WebSearch", "WebFetch"] {
            assert!(a.contains(&r.to_string()), "{r}");
        }
        assert!(a.contains(&"dontAsk".to_string()));
        call.profile = Profile::Readonly;
        let a = ClaudeCli::args(&call);
        assert!(a.contains(&"Bash(vdash search *)".to_string()));
        assert!(!a.contains(&"Bash(vdash check *)".to_string()) && !a.contains(&"Edit".to_string()));
    }
}
