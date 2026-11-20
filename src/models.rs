//! The model catalog (ROADMAP M8-08): which `backend:model` names `/model` offers.
//!
//! Gemini models come from `agy models` (fact A6), cached for a day. The Claude CLI
//! cannot list models (C17), so its catalog is the built-in aliases and ids; the
//! aliases always mean the newest model of their family. A model a CLI rejected
//! (`Failure::ModelUnavailable`) is recorded here and left out for a week, or until
//! `vdash model refresh` finds it works again.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backend::Failure;
use crate::config::{BackendKind, Config};
use crate::error::{Result, VdashError};
use crate::paths::Ctx;

/// Claude aliases first (they follow the newest model), then pinned ids (fact C12).
pub const CLAUDE_MODELS: &[&str] = &[
    "opus",
    "sonnet",
    "haiku",
    "fable",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-2",
];

/// Used only until `agy models` has been read once (fact A6, 2026-11-20).
pub const GEMINI_FALLBACK: &[&str] = &[
    "gemini-4-neon-xhigh",
    "gemini-4-neon-high",
    "gemini-4-neon-medium",
    "gemini-4-neon-low",
    "gemini-4-hydrogen-xhigh",
    "gemini-4-hydrogen-high",
    "gemini-4-hydrogen-medium",
    "gemini-4-hydrogen-low",
    "gemini-3.8-flash-high",
    "gemini-3.8-flash-medium",
    "gemini-3.8-flash-low",
];

/// How long a fetched Gemini list is trusted before it is read again.
pub const GEMINI_TTL: Duration = Duration::from_secs(24 * 3600);
/// How long a rejected model stays out of the catalog without a new rejection.
pub const REJECTED_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rejected {
    pub reason: String,
    /// Unix seconds.
    pub at: u64,
}

/// `models.json` in the user's cache directory, shared by every campaign.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cache {
    /// Gemini ids from the last `agy models`, in its order.
    #[serde(default)]
    pub gemini: Vec<String>,
    /// When `gemini` was fetched (unix seconds).
    #[serde(default)]
    pub gemini_fetched: Option<u64>,
    /// `backend:model` → why the CLI rejected it.
    #[serde(default)]
    pub rejected: BTreeMap<String, Rejected>,
}

/// What `/model` offers, per backend, plus what is left out and why.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub claude: Vec<String>,
    pub gemini: Vec<String>,
    /// (`backend:model`, reason), for display.
    pub rejected: Vec<(String, String)>,
    /// True when the Gemini list came from `agy models`, not the built-in fallback.
    pub gemini_live: bool,
}

impl Catalog {
    pub fn for_backend(&self, backend: &str) -> &[String] {
        match backend {
            "claude" => &self.claude,
            "gemini" => &self.gemini,
            _ => &[],
        }
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn key(backend: BackendKind, model: &str) -> String {
    format!("{backend}:{model}")
}

/// A missing or unreadable cache is an empty one.
pub fn load(path: &Path) -> Cache {
    crate::jsonio::read_json(path).unwrap_or_default()
}

pub fn save(path: &Path, cache: &Cache) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| VdashError::new(1, format!("cannot create {}: {e}", dir.display())))?;
    }
    crate::jsonio::write_atomic(path, cache)
}

/// The built-in Claude aliases and the ids they resolved to (fact C12). Each pair
/// names one model, so a rejection of either covers both.
const CLAUDE_ALIASES: &[(&str, &str)] =
    &[("opus", "claude-opus-5-5"), ("sonnet", "claude-sonnet-5-5"), ("haiku", "claude-haiku-5-5"), ("fable", "claude-fable-5-2")];

fn twin(backend: BackendKind, model: &str) -> Option<&'static str> {
    if backend != BackendKind::Claude {
        return None;
    }
    CLAUDE_ALIASES.iter().find_map(|(a, id)| if *a == model { Some(*id) } else if *id == model { Some(*a) } else { None })
}

/// The rejection still in force for this model (or its alias twin), if any.
pub fn rejected<'a>(cache: &'a Cache, backend: BackendKind, model: &str, now: u64) -> Option<&'a Rejected> {
    let live = |m: &str| cache.rejected.get(&key(backend, m)).filter(|r| now.saturating_sub(r.at) < REJECTED_TTL.as_secs());
    live(model).or_else(|| twin(backend, model).and_then(live))
}

/// The first sentence of a CLI's rejection message, which names the problem; the
/// rest is advice and links.
pub fn short_reason(reason: &str) -> String {
    let r = reason.trim();
    let first = match r.find(". ") {
        Some(i) => &r[..=i],
        None => r,
    };
    crate::util::clip(first.lines().next().unwrap_or(""), 200, false)
}

pub fn gemini_stale(cache: &Cache, now: u64) -> bool {
    cache.gemini.is_empty() || cache.gemini_fetched.is_none_or(|t| now.saturating_sub(t) >= GEMINI_TTL.as_secs())
}

pub fn catalog(cache: &Cache, now: u64) -> Catalog {
    let live = !cache.gemini.is_empty();
    let gemini: Vec<String> =
        if live { cache.gemini.clone() } else { GEMINI_FALLBACK.iter().map(|s| s.to_string()).collect() };
    let keep = |b: BackendKind, list: Vec<String>| -> Vec<String> { list.into_iter().filter(|m| rejected(cache, b, m, now).is_none()).collect() };
    Catalog {
        claude: keep(BackendKind::Claude, CLAUDE_MODELS.iter().map(|s| s.to_string()).collect()),
        gemini: keep(BackendKind::Gemini, gemini),
        rejected: cache
            .rejected
            .iter()
            .filter(|(_, r)| now.saturating_sub(r.at) < REJECTED_TTL.as_secs())
            .map(|(k, r)| (k.clone(), r.reason.clone()))
            .collect(),
        gemini_live: live,
    }
}

/// The catalog as of now, from the cache file.
pub fn current(path: &Path) -> Catalog {
    catalog(&load(path), now_unix())
}

/// Record that a CLI rejected `model`. Best effort: the run goes on either way.
pub fn mark_rejected(path: &Path, backend: BackendKind, model: &str, reason: &str) {
    let mut c = load(path);
    c.rejected.insert(key(backend, model), Rejected { reason: short_reason(reason), at: now_unix() });
    let _ = save(path, &c);
}

/// The Gemini ids in `agy models` output: one `<id>\t<label>` line each. The
/// orchestrator entries are agy's own, and the Claude and GPT ids belong to the
/// other backends, so only `gemini-*` model ids are kept.
pub fn parse_agy_models(out: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for line in out.lines() {
        let Some((id, _label)) = line.split_once('\t') else { continue };
        let id = id.trim();
        if id.starts_with("gemini-") && !id.contains("orchestrator") && !id.contains(char::is_whitespace) && !v.iter().any(|x| x == id) {
            v.push(id.to_string());
        }
    }
    v
}

const LEVELS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// The catalog model most like `model` that is not `model` itself: for Gemini the
/// same reasoning level, nearest name first, then agy's order (newest first); for
/// Claude the first alias in the catalog.
pub fn suggest(cat: &Catalog, backend: BackendKind, model: &str) -> Option<String> {
    let list = cat.for_backend(&backend.to_string());
    match backend {
        BackendKind::Claude => list.iter().find(|m| m.as_str() != model).cloned(),
        BackendKind::Gemini => {
            let level = model.rsplit('-').next().filter(|l| LEVELS.contains(l));
            let common = |m: &str| model.chars().zip(m.chars()).take_while(|(a, b)| a == b).count();
            let mut best: Option<(usize, &String)> = None;
            for m in list.iter().filter(|m| m.as_str() != model) {
                if level.is_some() && m.rsplit('-').next() != level {
                    continue;
                }
                let c = common(m);
                if best.is_none_or(|(b, _)| c > b) {
                    best = Some((c, m));
                }
            }
            best.map(|(_, m)| m.clone())
        }
    }
}

/// One warning per configured `backend:model` the catalog says will not work: a
/// rejected model, or a Gemini model the live `agy models` list no longer has.
/// Claude ids missing from the built-in list are not warned about, since the CLI
/// may know models newer than this binary.
pub fn role_warnings(cfg: &Config, cache: &Cache, now: u64) -> Vec<String> {
    let cat = catalog(cache, now);
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (role, entries) in &cfg.roles {
        for e in entries {
            let Some(m) = &e.model else { continue };
            let why = if let Some(r) = rejected(cache, e.backend, m, now) {
                r.reason.clone()
            } else if e.backend == BackendKind::Gemini && cat.gemini_live && !cat.gemini.contains(m) {
                "no longer listed by `agy models`".to_string()
            } else {
                continue;
            };
            seen.entry(format!("{}\u{0}{why}", key(e.backend, m))).or_default().push(role.to_string());
        }
    }
    seen.into_iter()
        .map(|(k, roles)| {
            let (name, why) = k.split_once('\u{0}').unwrap_or((&k, ""));
            let (b, m) = name.split_once(':').unwrap_or((name, ""));
            let fix = b
                .parse::<BackendKind>()
                .ok()
                .and_then(|b| suggest(&cat, b, m))
                .map(|s| format!("; try `/model {b} {s}`"))
                .unwrap_or_default();
            format!("{name} ({}) may not work: {why}{fix}", roles.join(", "))
        })
        .collect()
}

fn command_for(ctx: &Ctx, k: BackendKind, default: &str) -> (String, Vec<String>) {
    let cmd = ctx.cfg.backends.get(&k).map(|b| b.command.clone()).unwrap_or_default();
    match cmd.split_first() {
        Some((p, rest)) => (p.clone(), rest.to_vec()),
        None => (default.to_string(), Vec::new()),
    }
}

/// Read `agy models` into the cache. Returns how many Gemini models it listed.
pub fn refresh_gemini(ctx: &Ctx, cache: &mut Cache) -> Result<usize> {
    let (prog, pre) = command_for(ctx, BackendKind::Gemini, "agy");
    let mut cmd = Command::new(&prog);
    cmd.args(&pre).arg("models");
    crate::backend::scrub_env(&mut cmd, ctx);
    let out = crate::proc::run(&mut cmd, None, Duration::from_secs(60))?;
    let list = parse_agy_models(&out.stdout_lossy());
    if out.timed_out || list.is_empty() {
        let err = out.stderr_lossy();
        return Err(VdashError::new(
            1,
            format!("`{prog} models` listed no Gemini models{}", if err.trim().is_empty() { String::new() } else { format!(": {}", crate::util::clip(err.trim(), 200, false)) }),
        ));
    }
    cache.rejected.retain(|k, _| !k.strip_prefix("gemini:").is_some_and(|m| list.iter().any(|x| x == m)));
    let n = list.len();
    cache.gemini = list;
    cache.gemini_fetched = Some(now_unix());
    Ok(n)
}

/// One `claude -p` call on `model`. A model the account cannot use is rejected
/// before any inference, so this is free while the model stays unavailable (C17).
pub fn probe_claude(ctx: &Ctx, model: &str) -> Option<Failure> {
    let (prog, pre) = command_for(ctx, BackendKind::Claude, "claude");
    let mut cmd = Command::new(&prog);
    cmd.args(&pre).args([
        "-p",
        "--output-format",
        "json",
        "--model",
        model,
        "--tools",
        "",
        "--max-turns",
        "1",
        "--no-session-persistence",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--disable-slash-commands",
    ]);
    crate::backend::scrub_env(&mut cmd, ctx);
    match crate::proc::run(&mut cmd, Some(b"Reply with the single word ok"), Duration::from_secs(120)) {
        Ok(out) if out.timed_out => Some(Failure::Timeout),
        Ok(out) => crate::backend::claude::parse(&out.stdout_lossy()).failure,
        Err(_) => Some(Failure::Crash),
    }
}

/// `vdash model refresh`: reread `agy models` (when Gemini is configured) and probe
/// again the Claude models marked rejected. Returns one line per finding.
pub fn refresh(ctx: &Ctx, probe: bool) -> Result<Vec<String>> {
    let path = &ctx.models_cache;
    let mut cache = load(path);
    let mut lines = Vec::new();
    if ctx.cfg.backends.contains_key(&BackendKind::Gemini) {
        match refresh_gemini(ctx, &mut cache) {
            Ok(n) => lines.push(format!("gemini: {n} models from `agy models`")),
            Err(e) => lines.push(format!("gemini: {} (keeping the previous list)", e.message)),
        }
    }
    if probe {
        let now = now_unix();
        let claude: Vec<String> = cache
            .rejected
            .keys()
            .filter_map(|k| k.strip_prefix("claude:"))
            .filter(|m| rejected(&cache, BackendKind::Claude, m, now).is_some())
            .map(str::to_string)
            .collect();
        for m in claude {
            match probe_claude(ctx, &m) {
                None => {
                    cache.rejected.remove(&key(BackendKind::Claude, &m));
                    lines.push(format!("claude:{m} answers again"));
                }
                Some(Failure::ModelUnavailable(why)) => {
                    let why = short_reason(&why);
                    cache.rejected.insert(key(BackendKind::Claude, &m), Rejected { reason: why.clone(), at: now });
                    lines.push(format!("claude:{m} is still unavailable: {why}"));
                }
                Some(f) => lines.push(format!("claude:{m} could not be checked ({}); still marked unavailable", f.label())),
            }
        }
    }
    save(path, &cache)?;
    Ok(lines)
}

/// Text for `/model` and `vdash model`: the catalog and what was left out.
pub fn catalog_text(cat: &Catalog) -> String {
    let mut s = String::from("Available models:\n");
    s.push_str(&format!("  claude   {}\n", cat.claude.join(", ")));
    s.push_str(&format!(
        "  gemini   {}{}\n",
        cat.gemini.join(", "),
        if cat.gemini_live { "" } else { "  (built-in list; `/model refresh` reads `agy models`)" }
    ));
    for (k, why) in &cat.rejected {
        s.push_str(&format!("  unavailable: {k} ({why})\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGY: &str = "Fetching available models...\ngemini-4-neon-high\tGemini 4 Neon (High)\ngemini-4-neon-low\tGemini 4 Neon (Low)\ngemini-4-argon-high\tGemini 4 Argon (High)\ngemini-4-hydrogen-high\tGemini 4 Hydrogen (High)\ngemini-pro-orchestrator-neon\tOrchestrator (Neon)\ngemini-3.8-flash-high\tGemini 3.8 Flash (High)\nclaude-opus-5-5-low\tClaude Opus 5.5 (Low)\ngpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n";

    #[test]
    fn parses_agy_models_keeping_only_gemini_model_ids() {
        assert_eq!(
            parse_agy_models(AGY),
            vec!["gemini-4-neon-high", "gemini-4-neon-low", "gemini-4-argon-high", "gemini-4-hydrogen-high", "gemini-3.8-flash-high"]
        );
        assert!(parse_agy_models("Fetching available models...\n").is_empty());
    }

    #[test]
    fn catalog_uses_the_live_list_and_drops_rejected_models_until_they_expire() {
        let now = 10_000_000;
        let mut c = Cache::default();
        let cat = catalog(&c, now);
        assert!(!cat.gemini_live);
        assert_eq!(cat.gemini[0], "gemini-4-neon-xhigh");
        assert!(cat.claude.contains(&"fable".to_string()));

        c.gemini = parse_agy_models(AGY);
        c.gemini_fetched = Some(now - 60);
        c.rejected.insert("claude:fable".into(), Rejected { reason: "requires usage credits".into(), at: now - 60 });
        c.rejected.insert("gemini:gemini-4-neon-low".into(), Rejected { reason: "old".into(), at: now - REJECTED_TTL.as_secs() });
        let cat = catalog(&c, now);
        assert!(cat.gemini_live);
        assert!(!cat.claude.contains(&"fable".to_string()));
        assert!(!cat.claude.contains(&"claude-fable-5-2".to_string()), "the alias and its id are one model");
        assert!(cat.claude.contains(&"opus".to_string()));
        assert!(cat.gemini.contains(&"gemini-4-neon-low".to_string()), "an expired rejection is forgotten");
        assert_eq!(cat.rejected, vec![("claude:fable".to_string(), "requires usage credits".to_string())]);
        assert!(!gemini_stale(&c, now));
        assert!(gemini_stale(&c, now + GEMINI_TTL.as_secs()));
    }

    #[test]
    fn suggests_the_same_level_in_the_newest_family() {
        let c = Cache { gemini: parse_agy_models(AGY), gemini_fetched: Some(1), ..Default::default() };
        let cat = catalog(&c, 2);
        assert_eq!(suggest(&cat, BackendKind::Gemini, "gemini-4-argon-high").as_deref(), Some("gemini-4-neon-high"));
        assert_eq!(suggest(&cat, BackendKind::Gemini, "gemini-4-argon-medium"), None, "no medium level is listed");
        assert_eq!(suggest(&cat, BackendKind::Claude, "fable").as_deref(), Some("opus"));
    }

    #[test]
    fn mark_rejected_round_trips_through_the_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("cache/vdash/models.json");
        mark_rejected(&p, BackendKind::Claude, "fable", "Fable 5.2 requires usage credits.");
        let c = load(&p);
        assert!(rejected(&c, BackendKind::Claude, "fable", now_unix()).is_some());
        assert_eq!(c.rejected["claude:fable"].reason, "Fable 5.2 requires usage credits.");
        assert_eq!(short_reason("Fable 5.2 requires usage credits. Switch to another model, or see https://x.y/z."), "Fable 5.2 requires usage credits.");
        assert!(!current(&p).claude.contains(&"fable".to_string()));
    }

    #[test]
    fn warns_about_configured_models_that_will_not_work() {
        let tmp = tempfile::TempDir::new().unwrap();
        let host = tmp.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
        let camp = tmp.path().join("camp");
        std::fs::create_dir_all(&camp).unwrap();
        std::fs::write(
            camp.join("campaign.toml"),
            format!(
                "id = \"0123456789abcdef0123456789abcdef\"\nhost_project = \"{}\"\n[roles]\nworker = [\"gemini:gemini-4-argon-high\", \"claude:fable\"]\ncritic = [\"claude:fable\"]\nreviewer = [\"gemini:gemini-4-neon-high\", \"claude:claude-opus-9\"]\n",
                host.display()
            ),
        )
        .unwrap();
        let cfg = crate::config::load(&camp).unwrap();
        let now = 1_000;
        let mut c = Cache::default();
        // Before anything is known, nothing is flagged.
        assert!(role_warnings(&cfg, &c, now).is_empty());
        c.gemini = vec!["gemini-4-neon-high".into(), "gemini-4-hydrogen-high".into()];
        c.gemini_fetched = Some(now);
        c.rejected.insert("claude:fable".into(), Rejected { reason: "Fable 5.2 requires usage credits.".into(), at: now });
        let w = role_warnings(&cfg, &c, now);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].starts_with("claude:fable (critic, worker) may not work: Fable 5.2 requires usage credits."), "{w:?}");
        assert!(w[0].ends_with("try `/model claude opus`"), "{w:?}");
        assert!(w[1].contains("gemini:gemini-4-argon-high (worker)") && w[1].ends_with("try `/model gemini gemini-4-neon-high`"), "{w:?}");
    }
}
