//! Slash commands (ROADMAP M4-01): parsing, help text and completion.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Plain text: to the spec-writer before approval, guidance afterwards.
    Text(String),
    Cmd(Cmd),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Help,
    Status,
    Model(Option<(String, String)>),
    /// `/model default`: restore the built-in Claude/Gemini split for every role.
    ModelDefault,
    Login(Option<String>),
    Search(String),
    Btw(String),
    Spec(Option<String>),
    Review,
    Approve,
    Restart,
    Prove { single: bool },
    Pause,
    Resume,
    Stop,
    Kill,
    Verbose(Option<bool>),
    Gate,
    Tcb,
    Amend(Option<(bool, u32)>),
    /// `/amend approve N --accept-weakening`: apply an escalated proposal.
    AmendAcceptWeakening(u32),
    AmendPropose(String),
    /// `/escalations [all]` lists; `/escalations N` shows one file.
    Escalations { all: bool, show: Option<u32> },
    /// `/resolve <N[-M][,…]> [answer]`.
    Resolve { which: String, answer: String },
    Agents,
    Logs(String),
    Bulletin,
    Cost,
    Doctor,
    Clear,
    Quit,
}

/// (name, arguments, description) — the order is the order of `/help`.
pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("help", "", "list commands"),
    ("status", "", "campaign summary"),
    ("model", "[default | <role|all> <backend>[:<model>][,<backend>[:<model>]] | <backend> <model>]", "show or change which backend and model serves each role"),
    ("login", "[claude|gemini]", "sign in to your Claude or Gemini account"),
    ("search", "<query>", "search Mathlib, campaign Lean files, and online Loogle docs"),
    ("btw", "<question>", "ask the monitor a side question; work continues"),
    ("spec", "[feedback]", "draft or revise the spec; loops until the reviewers say ok"),
    ("review", "", "rerun the spec reviewers (and revise until they say ok)"),
    ("approve", "", "approve the spec (stops a running spec loop first)"),
    ("restart", "", "reopen a finished/approved campaign in spec phase to add goals"),
    ("prove", "[single]", "start the proof engine in the background"),
    ("pause", "", "pause the engine before its next agent"),
    ("resume", "", "resume a paused engine"),
    ("stop", "", "stop the engine, or end the spec loop (good enough)"),
    ("kill", "", "kill the engine and all agent processes now, for good"),
    ("verbose", "[on|off]", "toggle (or set) showing proofs as agents write them"),
    ("gate", "", "run the gatekeeper now"),
    ("tcb", "", "show what a reader must trust"),
    ("amend", "[approve N [--accept-weakening] | reject N | propose <file>]", "list, decide or propose spec amendments"),
    ("escalations", "[all | N]", "open escalations (all: also closed ones; N: one in full)"),
    ("resolve", "<N[-M][,…]> [answer]", "close escalations; an answer also becomes guidance"),
    ("agents", "", "agents running now"),
    ("logs", "<agent>", "one agent's transcript, e.g. /logs worker-3"),
    ("bulletin", "", "recent shared notices"),
    ("cost", "", "usage per role and backend"),
    ("doctor", "", "check tools and logins"),
    ("clear", "", "clear the transcript"),
    ("quit", "", "leave the app (the engine keeps running)"),
];

pub const ROLES: &[&str] = &[
    "all",
    "spec_writer",
    "architect",
    "orchestrator",
    "team_lead",
    "integrator",
    "critic",
    "monitor",
    "reviewer",
    "falsifier",
    "worker",
];

pub const BACKENDS: &[&str] = &["claude", "gemini"];

pub const CLAUDE_MODELS: &[&str] = &[
    // Probed on the subscription 2026-11-14 (ROADMAP fact C12): each id answered, and
    // the aliases resolve to the newest model of their family.
    "opus",
    "sonnet",
    "haiku",
    "fable",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-2",
];

pub const GEMINI_MODELS: &[&str] = &[
    "gemini-4-argon-high",
    "gemini-4-argon-medium",
    "gemini-4-argon-low",
    "gemini-4-hydrogen-xhigh",
    "gemini-4-hydrogen-high",
    "gemini-4-hydrogen-medium",
    "gemini-4-hydrogen-low",
    "gemini-3.8-flash-high",
    "gemini-3.8-flash-medium",
    "gemini-3.8-flash-low",
];

pub fn models_for_backend(backend: &str) -> &'static [&'static str] {
    match backend {
        "claude" => CLAUDE_MODELS,
        "gemini" => GEMINI_MODELS,
        _ => &[],
    }
}

fn usage(name: &str) -> String {
    let (n, a, _) = COMMANDS.iter().find(|(n, _, _)| *n == name).copied().unwrap_or((name, "", ""));
    if a.is_empty() {
        format!("usage: /{n}")
    } else {
        format!("usage: /{n} {a}")
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// `/model` arguments, with a backend list such as `gemini, claude` joined into one
/// word (`gemini,claude`).
fn model_words(args: &str) -> Vec<String> {
    let joined = args.split(',').map(str::trim).collect::<Vec<_>>().join(",");
    joined.split_whitespace().map(String::from).collect()
}

pub fn parse(line: &str) -> Result<Input, String> {
    let line = line.trim();
    let Some(rest) = line.strip_prefix('/') else {
        return Ok(Input::Text(line.to_string()));
    };
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    let words: Vec<&str> = args.split_whitespace().collect();
    let none = |c: Cmd| if args.is_empty() { Ok(c) } else { Err(usage(name)) };
    let cmd = match name {
        "help" | "?" => none(Cmd::Help),
        "status" => none(Cmd::Status),
        "model" | "models" => match model_words(args).iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] | ["list"] => Ok(Cmd::Model(None)),
            ["default"] => Ok(Cmd::ModelDefault),
            [backend @ ("claude" | "gemini"), model] => {
                let entry = if let Some(rest) = model.strip_prefix(&format!("{backend}:")) {
                    format!("{backend}:{rest}")
                } else {
                    format!("{backend}:{model}")
                };
                Ok(Cmd::Model(Some((backend.to_string(), entry))))
            }
            [role, entry] => {
                if !ROLES.contains(role) {
                    return Err(format!("unknown role '{role}'. Roles: {}", ROLES.join(", ")));
                }
                for part in entry.split(',') {
                    let backend = part.split(':').next().unwrap_or("");
                    if !BACKENDS.contains(&backend) {
                        return Err(format!("unknown backend '{backend}'. Backends: claude, gemini"));
                    }
                }
                Ok(Cmd::Model(Some((role.to_string(), entry.to_string()))))
            }
            [role, backend, model] => {
                if !ROLES.contains(role) {
                    return Err(format!("unknown role '{role}'. Roles: {}", ROLES.join(", ")));
                }
                if !BACKENDS.contains(backend) {
                    return Err(format!("unknown backend '{backend}'. Backends: claude, gemini"));
                }
                Ok(Cmd::Model(Some((role.to_string(), format!("{backend}:{model}")))))
            }
            _ => Err(usage("model")),
        },
        "login" | "signin" | "auth" => match words.as_slice() {
            [] => Ok(Cmd::Login(None)),
            [b] if BACKENDS.contains(b) => Ok(Cmd::Login(Some((*b).to_string()))),
            [_] => Err("unknown backend. Usage: /login [claude|gemini]".to_string()),
            _ => Err(usage("login")),
        },
        "search" | "mathlib" | "loogle" => {
            if args.is_empty() {
                Err(usage("search"))
            } else {
                Ok(Cmd::Search(args.to_string()))
            }
        }
        "btw" => {
            if args.is_empty() {
                Err(usage("btw"))
            } else {
                Ok(Cmd::Btw(args.to_string()))
            }
        }
        "spec" => Ok(Cmd::Spec(if args.is_empty() { None } else { Some(args.to_string()) })),
        "review" => none(Cmd::Review),
        "approve" => none(Cmd::Approve),
        "restart" => none(Cmd::Restart),
        "prove" => match words.as_slice() {
            [] => Ok(Cmd::Prove { single: false }),
            ["single"] | ["--single"] => Ok(Cmd::Prove { single: true }),
            _ => Err(usage("prove")),
        },
        "pause" => none(Cmd::Pause),
        "resume" => none(Cmd::Resume),
        "stop" => none(Cmd::Stop),
        "kill" => none(Cmd::Kill),
        "verbose" => match words.as_slice() {
            [] => Ok(Cmd::Verbose(None)),
            ["on"] => Ok(Cmd::Verbose(Some(true))),
            ["off"] => Ok(Cmd::Verbose(Some(false))),
            _ => Err(usage("verbose")),
        },
        "gate" => none(Cmd::Gate),
        "tcb" => none(Cmd::Tcb),
        "amend" | "amendments" => match words.as_slice() {
            [] | ["list"] => Ok(Cmd::Amend(None)),
            ["propose", f] => Ok(Cmd::AmendPropose(f.to_string())),
            ["approve", n, "--accept-weakening"] => n.parse::<u32>().map(Cmd::AmendAcceptWeakening).map_err(|_| usage("amend")),
            [verb @ ("approve" | "reject"), n] => n
                .parse::<u32>()
                .map(|n| Cmd::Amend(Some((*verb == "approve", n))))
                .map_err(|_| usage("amend")),
            _ => Err(usage("amend")),
        },
        "escalations" | "escalation" | "esc" => match words.as_slice() {
            [] => Ok(Cmd::Escalations { all: false, show: None }),
            ["all"] | ["--all"] => Ok(Cmd::Escalations { all: true, show: None }),
            [n] => n.trim_start_matches('#').parse::<u32>().map(|n| Cmd::Escalations { all: false, show: Some(n) }).map_err(|_| usage("escalations")),
            _ => Err(usage("escalations")),
        },
        "resolve" | "dismiss" => match words.as_slice() {
            [] => Err(usage("resolve")),
            [which, ..] => {
                if crate::escalation::parse_numbers(which).is_err() {
                    return Err(usage("resolve"));
                }
                let answer = args[which.len()..].trim().to_string();
                if name == "dismiss" && !answer.is_empty() {
                    return Err("/dismiss takes no answer; use /resolve N <answer>".to_string());
                }
                Ok(Cmd::Resolve { which: which.to_string(), answer })
            }
        },
        "agents" => none(Cmd::Agents),
        "logs" | "log" => match words.as_slice() {
            [a] => Ok(Cmd::Logs(a.to_string())),
            _ => Err(usage("logs")),
        },
        "bulletin" => none(Cmd::Bulletin),
        "cost" | "usage" => none(Cmd::Cost),
        "doctor" => none(Cmd::Doctor),
        "clear" => none(Cmd::Clear),
        "quit" | "exit" | "q" => none(Cmd::Quit),
        other => {
            let best = COMMANDS.iter().map(|(n, _, _)| (edit_distance(other, n), *n)).min();
            return Err(match best {
                Some((d, n)) if d <= 2 => format!("unknown command /{other}. Did you mean /{n}?"),
                _ => format!("unknown command /{other}. Type /help for the list."),
            });
        }
    }?;
    Ok(Input::Cmd(cmd))
}

/// Completions for the text typed so far. Each is the full replacement line.
pub fn complete(line: &str, agents: &[String]) -> Vec<String> {
    let Some(rest) = line.strip_prefix('/') else { return Vec::new() };
    let parts: Vec<&str> = rest.split(' ').collect();
    match parts.as_slice() {
        [prefix] => COMMANDS.iter().filter(|(n, _, _)| n.starts_with(prefix)).map(|(n, _, _)| format!("/{n}")).collect(),
        ["login", b] => BACKENDS.iter().filter(|x| x.starts_with(b)).map(|x| format!("/login {x}")).collect(),
        ["model", role] => std::iter::once(&"default").chain(ROLES).filter(|r| r.starts_with(role)).map(|r| format!("/model {r}")).collect(),
        ["model", backend @ ("claude" | "gemini"), model_prefix] => models_for_backend(backend)
            .iter()
            .filter(|m| m.starts_with(model_prefix))
            .map(|m| format!("/model {backend} {m}"))
            .collect(),
        ["model", role, be] => {
            // Complete the last entry of a list such as `gemini,cl`.
            let (head, be) = match be.rfind(',') {
                Some(i) => (&be[..=i], &be[i + 1..]),
                None => ("", *be),
            };
            if let Some((b, m_prefix)) = be.split_once(':') {
                models_for_backend(b)
                    .iter()
                    .filter(|m| m.starts_with(m_prefix))
                    .map(|m| format!("/model {role} {head}{b}:{m}"))
                    .collect()
            } else {
                let mut out: Vec<String> = BACKENDS.iter().filter(|b| b.starts_with(be)).map(|b| format!("/model {role} {head}{b}")).collect();
                // A role (not a backend name) can be shared by both backends.
                if head.is_empty() && !BACKENDS.contains(role) {
                    out.extend(["claude,gemini", "gemini,claude"].iter().filter(|l| l.starts_with(be)).map(|l| format!("/model {role} {l}")));
                }
                out
            }
        }
        ["model", role, be, model_prefix] => models_for_backend(be)
            .iter()
            .filter(|m| m.starts_with(model_prefix))
            .map(|m| format!("/model {role} {be} {m}"))
            .collect(),
        ["logs", a] => agents.iter().filter(|x| x.starts_with(a)).map(|x| format!("/logs {x}")).collect(),
        ["amend", v] => ["approve", "reject", "list", "propose"].iter().filter(|x| x.starts_with(v)).map(|x| format!("/amend {x}")).collect(),
        ["prove", v] => ["single"].iter().filter(|x| x.starts_with(v)).map(|x| format!("/prove {x}")).collect(),
        _ => Vec::new(),
    }
}

/// Description shown next to a completion.
pub fn describe(completion: &str) -> &'static str {
    let name = completion.trim_start_matches('/').split(' ').next().unwrap_or("");
    if completion.contains(' ') {
        return "";
    }
    COMMANDS.iter().find(|(n, _, _)| *n == name).map(|(_, _, d)| *d).unwrap_or("")
}

pub fn args_hint(name: &str) -> &'static str {
    COMMANDS.iter().find(|(n, _, _)| *n == name).map(|(_, a, _)| *a).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_command() {
        assert_eq!(parse("hello there").unwrap(), Input::Text("hello there".into()));
        assert_eq!(parse("/help").unwrap(), Input::Cmd(Cmd::Help));
        assert_eq!(parse("/model").unwrap(), Input::Cmd(Cmd::Model(None)));
        assert_eq!(parse("/model list").unwrap(), Input::Cmd(Cmd::Model(None)));
        assert_eq!(parse("/model default").unwrap(), Input::Cmd(Cmd::ModelDefault));
        assert_eq!(parse("/model worker gemini:gemini-4-argon-high").unwrap(), Input::Cmd(Cmd::Model(Some(("worker".into(), "gemini:gemini-4-argon-high".into())))));
        assert_eq!(parse("/model worker gemini gemini-4-argon-high").unwrap(), Input::Cmd(Cmd::Model(Some(("worker".into(), "gemini:gemini-4-argon-high".into())))));
        assert_eq!(parse("/model worker gemini,claude:sonnet").unwrap(), Input::Cmd(Cmd::Model(Some(("worker".into(), "gemini,claude:sonnet".into())))));
        assert_eq!(parse("/model worker gemini, claude").unwrap(), Input::Cmd(Cmd::Model(Some(("worker".into(), "gemini,claude".into())))));
        assert!(parse("/model worker gemini,gpt").unwrap_err().contains("unknown backend"));
        assert_eq!(parse("/model claude opus").unwrap(), Input::Cmd(Cmd::Model(Some(("claude".into(), "claude:opus".into())))));
        assert_eq!(parse("/login").unwrap(), Input::Cmd(Cmd::Login(None)));
        assert_eq!(parse("/login claude").unwrap(), Input::Cmd(Cmd::Login(Some("claude".into()))));
        assert_eq!(parse("/login gemini").unwrap(), Input::Cmd(Cmd::Login(Some("gemini".into()))));
        assert_eq!(parse("/search List.Perm").unwrap(), Input::Cmd(Cmd::Search("List.Perm".into())));
        assert_eq!(parse("/btw why is L1 stuck?").unwrap(), Input::Cmd(Cmd::Btw("why is L1 stuck?".into())));
        assert_eq!(parse("/spec").unwrap(), Input::Cmd(Cmd::Spec(None)));
        assert_eq!(parse("/spec make it stronger").unwrap(), Input::Cmd(Cmd::Spec(Some("make it stronger".into()))));
        assert_eq!(parse("/prove single").unwrap(), Input::Cmd(Cmd::Prove { single: true }));
        assert_eq!(parse("/amend approve 3").unwrap(), Input::Cmd(Cmd::Amend(Some((true, 3)))));
        assert_eq!(parse("/amend reject 12").unwrap(), Input::Cmd(Cmd::Amend(Some((false, 12)))));
        assert_eq!(parse("/amend approve 4 --accept-weakening").unwrap(), Input::Cmd(Cmd::AmendAcceptWeakening(4)));
        assert_eq!(parse("/logs worker-3").unwrap(), Input::Cmd(Cmd::Logs("worker-3".into())));
        for c in ["status", "review", "approve", "restart", "pause", "resume", "stop", "kill", "gate", "tcb", "agents", "bulletin", "cost", "doctor", "clear", "quit"] {
            assert!(matches!(parse(&format!("/{c}")), Ok(Input::Cmd(_))), "{c}");
        }
    }

    #[test]
    fn errors_name_the_usage_and_suggest() {
        assert_eq!(parse("/model worker").unwrap_err(), "usage: /model [default | <role|all> <backend>[:<model>][,<backend>[:<model>]] | <backend> <model>]");
        assert!(parse("/model wrker claude").unwrap_err().contains("unknown role"));
        assert!(parse("/model worker gpt").unwrap_err().contains("unknown backend"));
        assert_eq!(parse("/btw").unwrap_err(), "usage: /btw <question>");
        assert_eq!(parse("/search").unwrap_err(), "usage: /search <query>");
        assert_eq!(parse("/amend approve x").unwrap_err(), "usage: /amend [approve N [--accept-weakening] | reject N | propose <file>]");
        assert_eq!(parse("/amend propose p.json").unwrap(), Input::Cmd(Cmd::AmendPropose("p.json".into())));
        assert_eq!(parse("/escalations").unwrap(), Input::Cmd(Cmd::Escalations { all: false, show: None }));
        assert_eq!(parse("/escalations all").unwrap(), Input::Cmd(Cmd::Escalations { all: true, show: None }));
        assert_eq!(parse("/esc #23").unwrap(), Input::Cmd(Cmd::Escalations { all: false, show: Some(23) }));
        assert_eq!(
            parse("/resolve 23 you can use both lemmas as cited hypotheses").unwrap(),
            Input::Cmd(Cmd::Resolve { which: "23".into(), answer: "you can use both lemmas as cited hypotheses".into() })
        );
        assert_eq!(parse("/resolve 1-15,19").unwrap(), Input::Cmd(Cmd::Resolve { which: "1-15,19".into(), answer: String::new() }));
        assert_eq!(parse("/dismiss 4").unwrap(), Input::Cmd(Cmd::Resolve { which: "4".into(), answer: String::new() }));
        assert!(parse("/resolve").is_err());
        assert!(parse("/resolve abc").is_err());
        assert!(parse("/dismiss 4 because").is_err());
        assert_eq!(parse("/gaet").unwrap_err(), "unknown command /gaet. Did you mean /gate?");
        assert!(parse("/zzzzzz").unwrap_err().contains("/help"));
        assert_eq!(parse("/status now").unwrap_err(), "usage: /status");
    }

    #[test]
    fn completion() {
        assert_eq!(complete("/st", &[]), vec!["/status", "/stop"]);
        assert_eq!(complete("/model wo", &[]), vec!["/model worker"]);
        assert_eq!(complete("/model de", &[]), vec!["/model default"]);
        assert_eq!(complete("/model worker g", &[]), vec!["/model worker gemini", "/model worker gemini,claude"]);
        assert_eq!(complete("/model worker gemini,c", &[]), vec!["/model worker gemini,claude"]);
        assert_eq!(complete("/model claude g", &[]), Vec::<String>::new());
        assert_eq!(complete("/model worker claude:o", &[]), vec!["/model worker claude:opus"]);
        assert_eq!(complete("/model worker gemini gemini-3.8-flash-h", &[]), vec!["/model worker gemini gemini-3.8-flash-high"]);
        assert_eq!(complete("/login c", &[]), vec!["/login claude"]);
        assert_eq!(complete("/logs w", &["worker-1".into(), "critic-2".into()]), vec!["/logs worker-1"]);
        assert!(complete("plain", &[]).is_empty());
        assert_eq!(describe("/gate"), "run the gatekeeper now");
    }
}
