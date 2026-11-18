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
    /// Continue an earlier session instead of starting fresh. The pool uses it only
    /// when it picks the same backend; otherwise the call runs fresh with `prompt`.
    pub resume: Option<Resume>,
}

/// An earlier agent session that a follow-up call can continue (fact C14 / A18):
/// the model keeps its context, so the follow-up sends only the new message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub backend: BackendKind,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub session: Session,
    /// The message sent in place of the full prompt when the session is resumed.
    pub prompt: String,
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
    /// The CLI's session id, for a follow-up call that resumes this one.
    pub session: Option<String>,
    /// With a `Quota` failure: how long until the limit resets, when the CLI said.
    pub retry_after: Option<Duration>,
    /// True when this call continued an earlier session.
    pub resumed: bool,
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

/// How long until a usage limit resets, from the CLI's message. Understands an
/// epoch suffix (`...limit reached|1763575200`), a relative delay (`retry in 34.5s`,
/// `reset after 2h15m30s`) and a wall-clock time (`resets 3pm`, `resets 11:30pm
/// (Europe/Paris)`, `resets at 15:00`). `None` when the message gives no time.
pub fn quota_reset(text: &str) -> Option<Duration> {
    quota_reset_at(text, std::time::SystemTime::now(), &|tz| local_offset_s(tz))
}

/// [`quota_reset`] with an explicit clock and a `tz name -> UTC offset seconds`
/// lookup (`None` = the local zone). Public for tests.
pub fn quota_reset_at(text: &str, now: std::time::SystemTime, offset_s: &dyn Fn(Option<&str>) -> Option<i64>) -> Option<Duration> {
    let now_s = now.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    let lower = text.to_lowercase();
    // 1. `|<epoch seconds>`
    if let Some(i) = lower.find("limit reached|") {
        let digits: String = lower[i + 14..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(t) = digits.parse::<i64>() {
            return Some(Duration::from_secs((t - now_s).max(0) as u64));
        }
    }
    // 2. relative: `retry in 34.5s`, `reset after 2h15m30s`, `retry after 90 seconds`
    for key in ["retry in ", "retry after ", "reset after ", "resets in ", "reset in ", "try again in "] {
        if let Some(i) = lower.find(key) {
            if let Some(d) = parse_rel(&lower[i + key.len()..]) {
                return Some(d);
            }
        }
    }
    // 3. wall clock: `resets 3pm`, `resets at 11:30pm (Europe/Paris)`, `resets 15:00`
    let i = lower.find("resets ").or_else(|| lower.find("reset at "))?;
    let rest = lower[i..].split_once(' ')?.1.trim_start();
    let rest = rest.strip_prefix("at ").unwrap_or(rest);
    let (secs_of_day, after) = parse_clock(rest)?;
    let tz = after.trim_start().strip_prefix('(').and_then(|r| r.split_once(')')).map(|(z, _)| z.trim());
    // Keep the zone's original case (`Europe/Paris`): find it in `text`.
    let tz_orig = tz.and_then(|z| {
        let lt = text.to_lowercase();
        lt.find(z).map(|j| text[j..j + z.len()].to_string())
    });
    let off = offset_s(tz_orig.as_deref())?;
    let local_now = now_s + off;
    let today = local_now - local_now.rem_euclid(86_400);
    let mut t = today + secs_of_day;
    if t <= local_now {
        t += 86_400;
    }
    Some(Duration::from_secs((t - local_now) as u64))
}

fn parse_rel(s: &str) -> Option<Duration> {
    let mut total = 0f64;
    let mut num = String::new();
    let mut any = false;
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            chars.next();
            continue;
        }
        if c == ' ' && !num.is_empty() {
            chars.next();
            continue;
        }
        if num.is_empty() {
            break;
        }
        let unit: String = std::iter::from_fn(|| chars.next_if(|c| c.is_ascii_alphabetic())).collect();
        let n: f64 = num.parse().ok()?;
        num.clear();
        let mult = match unit.as_str() {
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
            "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
            "ms" => 0.001,
            _ => return if any { Some(Duration::from_secs_f64(total)) } else { None },
        };
        total += n * mult;
        any = true;
    }
    any.then(|| Duration::from_secs_f64(total))
}

/// `3pm`, `11:30pm`, `11:30 pm`, `15:00` -> seconds since midnight, and the rest.
fn parse_clock(s: &str) -> Option<(i64, &str)> {
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == ':')).unwrap_or(s.len());
    let (num, mut rest) = s.split_at(end);
    let (h, m) = match num.split_once(':') {
        Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
        None => (num.parse::<i64>().ok()?, 0),
    };
    let r = rest.trim_start();
    let mut h = h;
    if let Some(x) = r.strip_prefix("am") {
        if h == 12 {
            h = 0;
        }
        rest = x;
    } else if let Some(x) = r.strip_prefix("pm") {
        if h < 12 {
            h += 12;
        }
        rest = x;
    } else if !num.contains(':') {
        return None; // a bare number is not a time
    }
    if !(0..24).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    Some((h * 3600 + m * 60, rest))
}

/// UTC offset in seconds of `tz` (or the local zone), from `date +%z`: the `time`
/// crate cannot read the local offset once threads are running.
fn local_offset_s(tz: Option<&str>) -> Option<i64> {
    let mut cmd = Command::new("date");
    cmd.arg("+%z");
    if let Some(z) = tz {
        if !z.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c)) {
            return None;
        }
        cmd.env("TZ", z);
    }
    let out = cmd.output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.len() != 5 {
        return None;
    }
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let h: i64 = s[1..3].parse().ok()?;
    let m: i64 = s[3..5].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn quota_reset_parses_epoch_relative_and_clock_times() {
        // 2026-11-18 10:00:00 UTC
        let now = UNIX_EPOCH + Duration::from_secs(1_795_428_000);
        let utc = |_: Option<&str>| Some(0i64);
        let paris = |z: Option<&str>| Some(if z == Some("Europe/Paris") { 3600 } else { 0 });
        let r = |t: &str| quota_reset_at(t, now, &utc).map(|d| d.as_secs());
        assert_eq!(r("Claude AI usage limit reached|1795431600"), Some(3600));
        assert_eq!(r("Please retry in 34.5s."), Some(34));
        assert_eq!(r("Your quota will reset after 2h15m30s."), Some(8130));
        assert_eq!(r("try again in 5 minutes"), Some(300));
        assert_eq!(r("5-hour limit reached ∙ resets 3pm"), Some(5 * 3600));
        assert_eq!(r("You've hit your limit · resets 9am"), Some(23 * 3600));
        assert_eq!(r("Session limit reached ∙ resets 11:30pm"), Some(13 * 3600 + 1800));
        assert_eq!(r("limit; resets at 15:00"), Some(5 * 3600));
        // 3pm Paris = 14:00 UTC, four hours from now.
        assert_eq!(quota_reset_at("resets 3pm (Europe/Paris)", now, &paris).map(|d| d.as_secs()), Some(4 * 3600));
        assert_eq!(r("rate limit exceeded"), None);
        assert_eq!(r("resets 3 of your counters"), None);
    }
}
