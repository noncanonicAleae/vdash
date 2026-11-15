//! Model backends (ROADMAP M2-03): the `claude` and `agy` (Gemini) CLIs, plus a
//! scripted fake for tests. Everything above this module calls models through
//! [`pool::Pool`], never a backend directly.

pub mod claude;
pub mod fake;
pub mod gemini;
pub mod pool;

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::config::{BackendKind, Role};
use crate::paths::Ctx;

/// What an agent may do. Each backend maps a profile onto its own flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// No tools at all; everything is in the prompt.
    None,
    /// Read and search files.
    Readonly,
    /// Read, edit files, and run `vdash check` / `vdash axioms`.
    Lean,
}

impl Profile {
    pub fn for_role(role: Role) -> Profile {
        match role {
            Role::Critic => Profile::None,
            Role::Orchestrator | Role::TeamLead | Role::Reviewer | Role::Architect | Role::Monitor => {
                Profile::Readonly
            }
            Role::SpecWriter | Role::Worker | Role::Falsifier | Role::Integrator => Profile::Lean,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentCall {
    pub role: Role,
    pub prompt: String,
    pub skill: String,
    pub schema: Value,
    pub cwd: PathBuf,
    pub profile: Profile,
    pub model: Option<String>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Timeout,
    Crash,
    Quota,
    NotLoggedIn,
    AgentError(String),
}

impl Failure {
    pub fn label(&self) -> String {
        match self {
            Failure::Timeout => "timeout".into(),
            Failure::Crash => "crash".into(),
            Failure::Quota => "quota".into(),
            Failure::NotLoggedIn => "not_logged_in".into(),
            Failure::AgentError(m) => format!("agent_error: {m}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentResult {
    pub backend: Option<BackendKind>,
    pub model: Option<String>,
    pub text: String,
    pub structured: Option<Value>,
    pub failure: Option<Failure>,
    pub cost_usd: Option<f64>,
    pub tokens: Option<u64>,
    pub seconds: f64,
    pub denials: Vec<String>,
    /// The argv used, with the skill text replaced by its hash.
    pub argv: Vec<String>,
    pub raw: String,
}

pub trait Backend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn run(&self, ctx: &Ctx, call: &AgentCall) -> AgentResult;
}

/// Environment variables never passed to agents (API keys, and the parent Claude
/// Code session's own markers).
pub fn scrub_env(cmd: &mut Command, ctx: &Ctx) {
    let strip_prefixes = ["CLAUDE_CODE_"];
    let strip_exact = [
        "CLAUDECODE",
        "CLAUDE_PID",
        "CLAUDE_EFFORT",
        "ANTHROPIC_BASE_URL",
    ];
    let keys = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "GEMINI_API_KEY", "GOOGLE_API_KEY"];
    for (k, _) in std::env::vars_os() {
        let Some(k) = k.to_str() else { continue };
        if strip_exact.contains(&k) || strip_prefixes.iter().any(|p| k.starts_with(p)) {
            cmd.env_remove(k);
        }
    }
    if ctx.cfg.use_subscription {
        for k in keys {
            cmd.env_remove(k);
        }
    }
    let exe_dir = ctx.exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let path = std::env::var("PATH").unwrap_or_default();
    cmd.env("PATH", format!("{}:{path}", exe_dir.display()));
    cmd.env("VDASH_CAMPAIGN", &ctx.campaign.root);
}

/// Case-insensitive search for usage-limit wording.
pub fn looks_like_quota(text: &str) -> bool {
    let t = text.to_lowercase();
    ["usage limit", "rate limit", "rate_limit", "quota", "overloaded", "resource_exhausted", "too many requests"]
        .iter()
        .any(|w| t.contains(w))
}

pub fn looks_like_not_logged_in(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "please run /login",
        "not logged in",
        "invalid api key",
        "please set an auth method",
        "oauth token has expired",
        "authentication_error",
    ]
    .iter()
    .any(|w| t.contains(w))
}

pub(crate) fn hash_short(s: &str) -> String {
    crate::util::sha256_hex(s.as_bytes())[..16].to_string()
}
