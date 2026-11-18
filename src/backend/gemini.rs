//! Gemini backend through Google's Antigravity CLI, `agy` (facts A1–A17).
//!
//! `agy` takes the prompt as the value of `-p=` (stdin is not read), has no
//! system-prompt flag, and supports `--json-schema` like Claude.

use std::process::Command;
use std::time::Instant;

use serde_json::Value;

use super::{looks_like_not_logged_in, looks_like_quota, scrub_env, AgentCall, AgentResult, Backend, Failure, Profile};
use crate::config::BackendKind;
use crate::paths::Ctx;
use crate::proc;

/// Linux limits one argv entry to 128 KiB; stay well below it (fact A3).
pub const MAX_PROMPT_BYTES: usize = 100_000;

pub struct AgyCli {
    pub command: Vec<String>,
}

impl AgyCli {
    pub fn new(command: Vec<String>) -> Self {
        AgyCli { command }
    }

    /// The resumed session id, when this call continues an agy conversation.
    fn resumed(call: &AgentCall) -> Option<&str> {
        call.resume.as_ref().filter(|r| r.session.backend == BackendKind::Gemini).map(|r| r.session.id.as_str())
    }

    pub fn full_prompt(call: &AgentCall) -> String {
        // A resumed conversation already holds the skill; send only the new message.
        if Self::resumed(call).is_some() {
            return format!("{}\n\nYour reply must be a single JSON object matching the schema you were given.", call.prompt.trim_end());
        }
        format!(
            "{}\n\n---\n\nYour reply must be a single JSON object matching the schema you were given.\n\n{}",
            call.skill.trim_end(),
            call.prompt
        )
    }

    /// Roles that run commands (`vdash check`, `grep` over Mathlib). The monitor runs
    /// alongside the engine with no snapshot to revert against, and the critic needs
    /// no tools, so both keep agy's default mode (no shell commands).
    pub fn sandboxed(call: &AgentCall) -> bool {
        match call.profile {
            Profile::Lean => true,
            Profile::Readonly => call.role != crate::config::Role::Monitor,
            Profile::None => false,
        }
    }

    pub fn args(call: &AgentCall, text: &str) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "--output-format".into(),
            "json".into(),
            "--print-timeout".into(),
            format!("{}s", call.timeout.as_secs().max(1)),
        ];
        if let Some(m) = &call.model {
            a.push("--model".into());
            a.push(m.clone());
        }
        a.push("--json-schema".into());
        a.push(call.schema.to_string());
        a.push("--disable-slash-commands".into());
        if let Some(id) = Self::resumed(call) {
            // Fact A18: `--conversation <id>` continues it (prompt cache hits).
            a.push("--conversation".into());
            a.push(id.to_string());
        }
        if Self::sandboxed(call) {
            // Fact A15: in print mode agy's built-in tool-safety monitor soft-denies
            // every shell command (`--settings` allow rules do not help) and the turn
            // ends. `--mode auto` lets commands run; `--sandbox` confines shell and
            // file writes to the workspace (plus /tmp) and keeps reads and network
            // (A16). Never `--add-dir`: it makes the directory writable (A17).
            a.push("--mode".into());
            a.push("auto".into());
            a.push("--sandbox".into());
        }
        a.push(format!("-p={text}"));
        a
    }
}

/// Parse one `agy --output-format json` result (fact A4).
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
    r.session = v.get("conversation_id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        // `error` is an object `{message, code}` for startup failures and a plain
        // string for failures mid-turn (e.g. "The stream was interrupted ...").
        let msg = match err {
            Value::String(s) => s.clone(),
            _ => err.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
        };
        let code = err.get("code").and_then(Value::as_i64);
        if msg.to_lowercase().contains("stream was interrupted") {
            r.text = msg;
            r.failure = Some(Failure::Crash);
            return r;
        }
        r.failure = Some(if code == Some(41) || looks_like_not_logged_in(&msg) {
            Failure::NotLoggedIn
        } else if looks_like_quota(&msg) || code == Some(429) {
            Failure::Quota
        } else {
            Failure::AgentError(if msg.is_empty() { "agy reported an error without a message".into() } else { crate::util::clip(&msg, 300, false) })
        });
        r.text = msg;
        return r;
    }
    r.text = v.get("response").and_then(Value::as_str).unwrap_or("").to_string();
    r.structured = v.get("structured_output").filter(|s| s.is_object()).cloned();
    r.tokens = v.get("usage").and_then(|u| u.get("total_tokens")).and_then(Value::as_u64);
    for d in v.get("denied_actions").and_then(Value::as_array).into_iter().flatten() {
        let action = d.get("action").and_then(Value::as_str).unwrap_or("?");
        let name = d.get("display_name").and_then(Value::as_str).unwrap_or("");
        r.denials.push(format!("{action}: {name}"));
    }
    let status = v.get("status").and_then(Value::as_str).unwrap_or("");
    if r.structured.is_none() && !r.denials.is_empty() {
        r.failure = Some(Failure::AgentError(format!("tool call denied ({}); agy ended the turn", r.denials.join(", "))));
        return r;
    }
    if status != "SUCCESS" {
        r.failure = Some(if looks_like_quota(&r.text) || looks_like_quota(status) {
            Failure::Quota
        } else {
            Failure::AgentError(format!("status {status}"))
        });
    } else if r.text.contains("no output produced") && r.text.contains("permission") {
        // A tool the agent tried was not allowed; agy ends the turn (fact A11).
        r.denials.push(crate::util::clip(r.text.trim(), 300, false));
        r.failure = Some(Failure::AgentError("a tool call was denied and agy ended the turn".into()));
    } else if looks_like_quota(&r.text) && r.structured.is_none() {
        r.failure = Some(Failure::Quota);
    }
    r
}

impl Backend for AgyCli {
    fn kind(&self) -> BackendKind {
        BackendKind::Gemini
    }

    fn run(&self, ctx: &Ctx, call: &AgentCall) -> AgentResult {
        let start = Instant::now();
        let text = Self::full_prompt(call);
        if text.len() > MAX_PROMPT_BYTES {
            return AgentResult {
                backend: Some(BackendKind::Gemini),
                failure: Some(Failure::AgentError(format!(
                    "prompt too large for agy ({} bytes > {MAX_PROMPT_BYTES})",
                    text.len()
                ))),
                ..Default::default()
            };
        }
        let (prog, pre) = self.command.split_first().map(|(p, r)| (p.clone(), r.to_vec())).unwrap_or(("agy".into(), vec![]));
        let args = Self::args(call, &text);
        let mut cmd = Command::new(&prog);
        cmd.args(&pre).args(&args).current_dir(&call.cwd);
        scrub_env(&mut cmd, ctx);
        let shown: Vec<String> = std::iter::once(prog.clone())
            .chain(pre.iter().cloned())
            .chain(args.iter().map(|a| {
                if a.starts_with("-p=") {
                    format!("-p=<skill sha256:{} + prompt>", super::hash_short(&call.skill))
                } else {
                    a.clone()
                }
            }))
            .collect();
        // Allow agy a little longer than its own print timeout.
        let hard = call.timeout + std::time::Duration::from_secs(30);
        let mut r = match proc::run(&mut cmd, None, hard) {
            Ok(out) if out.timed_out => AgentResult { failure: Some(Failure::Timeout), raw: out.stdout_lossy(), ..Default::default() },
            Ok(out) => {
                let mut r = parse(&out.stdout_lossy());
                if r.failure == Some(Failure::Crash) {
                    r.raw.push_str(&out.stderr_lossy());
                }
                r
            }
            Err(e) => AgentResult { failure: Some(Failure::Crash), raw: e.message, ..Default::default() },
        };
        r.backend = Some(BackendKind::Gemini);
        r.model = call.model.clone();
        r.resumed = Self::resumed(call).is_some();
        if r.failure == Some(Failure::Quota) {
            r.retry_after = super::quota_reset(&format!("{}\n{}", r.text, r.raw));
        }
        r.seconds = start.elapsed().as_secs_f64();
        r.argv = shown;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success() {
        let s = r#"{"conversation_id":"c","status":"SUCCESS","response":"{\"answer\":4}\n","duration_seconds":2.3,"num_turns":1,"structured_output":{"answer":4},"usage":{"total_tokens":8354}}"#;
        let r = parse(s);
        assert_eq!(r.failure, None);
        assert_eq!(r.structured.unwrap()["answer"], 4);
        assert_eq!(r.tokens, Some(8354));
    }

    #[test]
    fn classifies_failures() {
        let nl = r#"{"session_id":"x","error":{"type":"Error","message":"Please set an Auth method in your settings","code":41}}"#;
        assert_eq!(parse(nl).failure, Some(Failure::NotLoggedIn));
        let denied = r#"{"status":"SUCCESS","response":"\njetski: no output produced — a tool required the \"command\" permission that headless mode cannot prompt for, so it was auto-denied."}"#;
        let r = parse(denied);
        assert!(matches!(r.failure, Some(Failure::AgentError(_))));
        assert_eq!(r.denials.len(), 1);
        let d = r#"{"status":"SUCCESS","response":"","denied_actions":[{"action":"command","display_name":"Bash"}]}"#;
        let r = parse(d);
        assert!(matches!(r.failure, Some(Failure::AgentError(ref m)) if m.contains("denied")));
        assert_eq!(r.denials, vec!["command: Bash".to_string()]);
        let q = r#"{"error":{"message":"RESOURCE_EXHAUSTED: quota exceeded","code":8}}"#;
        assert_eq!(parse(q).failure, Some(Failure::Quota));
        assert_eq!(parse("Segmentation fault").failure, Some(Failure::Crash));
        let interrupted = r#"{"status":"ERROR","response":"{}","error":"The stream was interrupted. Please continue the task you were working on.","structured_output":{"lens":"soundness","findings":[],"verdict":"ok"},"finished_reason":"error"}"#;
        assert_eq!(parse(interrupted).failure, Some(Failure::Crash), "a cut-off turn is not a valid review");
    }

    fn call(role: crate::config::Role, profile: Profile) -> AgentCall {
        AgentCall {
            role,
            prompt: "p".into(),
            skill: "s".into(),
            schema: serde_json::json!({"type":"object"}),
            cwd: ".".into(),
            profile,
            model: None,
            timeout: std::time::Duration::from_secs(60),
            resume: None,
        }
    }

    #[test]
    fn conversation_id_is_parsed_and_resume_sends_only_the_new_message() {
        use crate::backend::{Resume, Session};
        use crate::config::Role;
        let s = r#"{"conversation_id":"c7bf","status":"SUCCESS","response":"{}","structured_output":{}}"#;
        assert_eq!(parse(s).session.as_deref(), Some("c7bf"));
        let mut c = call(Role::Worker, Profile::Lean);
        c.skill = "SKILL TEXT".into();
        c.prompt = "fix it".into();
        assert!(AgyCli::full_prompt(&c).contains("SKILL TEXT"));
        c.resume = Some(Resume { session: Session { backend: BackendKind::Gemini, id: "c7bf".into() }, prompt: "fix it".into() });
        let text = AgyCli::full_prompt(&c);
        assert!(!text.contains("SKILL TEXT") && text.starts_with("fix it"), "{text}");
        let a = AgyCli::args(&c, &text).join(" ");
        assert!(a.contains("--conversation c7bf"), "{a}");
        c.resume.as_mut().unwrap().session.backend = BackendKind::Claude;
        assert!(!AgyCli::args(&c, "x").join(" ").contains("--conversation"));
    }

    #[test]
    fn command_roles_run_in_the_sandbox_in_auto_mode() {
        use crate::config::Role;
        for (r, p) in [(Role::Worker, Profile::Lean), (Role::Reviewer, Profile::Readonly)] {
            let a = AgyCli::args(&call(r, p), "hello");
            let joined = a.join(" ");
            assert!(joined.contains("--mode auto --sandbox"), "{r}: {joined}");
            assert_eq!(a.last().unwrap(), "-p=hello");
            assert!(!joined.contains("dangerously") && !joined.contains("--add-dir"), "{joined}");
        }
        for (r, p) in [(Role::Monitor, Profile::Readonly), (Role::Critic, Profile::None)] {
            let a = AgyCli::args(&call(r, p), "x").join(" ");
            assert!(!a.contains("--mode") && !a.contains("--sandbox"), "{r}: {a}");
        }
    }
}
