//! The interactive app (ROADMAP M4). It never runs proof rounds itself: it starts
//! or attaches to an engine, follows `logs/events.jsonl`, and sends control
//! messages. Long actions run `vdash <subcommand>` as child processes so the screen
//! never blocks.

pub mod app;
pub mod commands;
pub mod view;
pub mod watch;

use std::io;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{event::DisableBracketedPaste, event::EnableBracketedPaste, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::config::{self, Role, RoleEntry};
use crate::error::{VdashError, Result};
use crate::events::{self, Control};
use crate::paths::Ctx;
use crate::util;
use app::{App, Kind, Line};
use commands::{Cmd, Input};

#[derive(Debug, Default, Clone)]
pub struct Ui {
    pub input: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub hist_idx: Option<usize>,
    pub scroll: usize,
    pub completions: Vec<String>,
    pub completion_idx: usize,
    pub tick: u64,
    pub confirm_quit: bool,
    pub busy: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Submit(String),
    Quit,
}

fn byte_at(s: &str, ch: usize) -> usize {
    s.char_indices().nth(ch).map(|(i, _)| i).unwrap_or(s.len())
}

impl Ui {
    fn len(&self) -> usize {
        self.input.chars().count()
    }

    fn refresh_completions(&mut self, agents: &[String]) {
        self.completions = if self.input.starts_with('/') && !self.input.contains("  ") {
            let c = commands::complete(&self.input, agents);
            if c.len() == 1 && c[0] == self.input.trim_end() {
                Vec::new()
            } else {
                c
            }
        } else {
            Vec::new()
        };
        self.completion_idx = self.completion_idx.min(self.completions.len().saturating_sub(1));
    }

    fn set_input(&mut self, s: String) {
        self.cursor = s.chars().count();
        self.input = s;
    }

    fn insert(&mut self, c: char) {
        let at = byte_at(&self.input, self.cursor);
        self.input.insert(at, c);
        self.cursor += 1;
    }
}

/// Pure key handling: `(Ui, KeyEvent) -> Action` plus edits to `ui`.
pub fn handle_key(ui: &mut Ui, key: KeyEvent, agents: &[String]) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if !(ctrl && key.code == KeyCode::Char('c')) {
        ui.confirm_quit = false;
    }
    match key.code {
        KeyCode::Char('c') if ctrl => {
            if !ui.input.is_empty() {
                ui.set_input(String::new());
                ui.completions.clear();
            } else if ui.confirm_quit {
                return Action::Quit;
            } else {
                ui.confirm_quit = true;
            }
        }
        KeyCode::Char('d') if ctrl => return Action::Quit,
        KeyCode::Char('u') if ctrl => ui.set_input(String::new()),
        KeyCode::Char('a') if ctrl => ui.cursor = 0,
        KeyCode::Char('e') if ctrl => ui.cursor = ui.len(),
        KeyCode::Char('w') if ctrl => {
            let chars: Vec<char> = ui.input.chars().collect();
            let mut i = ui.cursor;
            while i > 0 && chars[i - 1] == ' ' {
                i -= 1;
            }
            while i > 0 && chars[i - 1] != ' ' {
                i -= 1;
            }
            let rest: String = chars[ui.cursor..].iter().collect();
            ui.input = chars[..i].iter().collect::<String>() + &rest;
            ui.cursor = i;
        }
        KeyCode::Char('l') if ctrl => {}
        KeyCode::Char(c) => ui.insert(c),
        KeyCode::Backspace => {
            if ui.cursor > 0 {
                let at = byte_at(&ui.input, ui.cursor - 1);
                ui.input.remove(at);
                ui.cursor -= 1;
            }
        }
        KeyCode::Delete => {
            if ui.cursor < ui.len() {
                let at = byte_at(&ui.input, ui.cursor);
                ui.input.remove(at);
            }
        }
        KeyCode::Left => ui.cursor = ui.cursor.saturating_sub(1),
        KeyCode::Right => ui.cursor = (ui.cursor + 1).min(ui.len()),
        KeyCode::Home => ui.cursor = 0,
        KeyCode::End => ui.cursor = ui.len(),
        KeyCode::Esc => {
            ui.completions.clear();
            return Action::None;
        }
        KeyCode::Tab | KeyCode::BackTab => {
            if !ui.completions.is_empty() {
                let pick = ui.completions[ui.completion_idx].clone();
                ui.set_input(format!("{pick} "));
                ui.completion_idx = 0;
            }
        }
        KeyCode::Up if !ui.completions.is_empty() => {
            ui.completion_idx = ui.completion_idx.checked_sub(1).unwrap_or(ui.completions.len() - 1);
            return Action::None;
        }
        KeyCode::Down if !ui.completions.is_empty() => {
            ui.completion_idx = (ui.completion_idx + 1) % ui.completions.len();
            return Action::None;
        }
        KeyCode::Up => {
            if ui.history.is_empty() {
                return Action::None;
            }
            let i = match ui.hist_idx {
                None => ui.history.len() - 1,
                Some(i) => i.saturating_sub(1),
            };
            ui.hist_idx = Some(i);
            let h = ui.history[i].clone();
            ui.set_input(h);
            ui.completions.clear();
            return Action::None;
        }
        KeyCode::Down => {
            match ui.hist_idx {
                Some(i) if i + 1 < ui.history.len() => {
                    ui.hist_idx = Some(i + 1);
                    let h = ui.history[i + 1].clone();
                    ui.set_input(h);
                }
                _ => {
                    ui.hist_idx = None;
                    ui.set_input(String::new());
                }
            }
            ui.completions.clear();
            return Action::None;
        }
        KeyCode::PageUp => ui.scroll += 10,
        KeyCode::PageDown => ui.scroll = ui.scroll.saturating_sub(10),
        KeyCode::Enter => {
            // Enter on an open completion list accepts the completion first.
            if !ui.completions.is_empty() && ui.input.trim_end() != ui.completions[ui.completion_idx] {
                let pick = ui.completions[ui.completion_idx].clone();
                if commands::args_hint(pick.trim_start_matches('/')).is_empty() {
                    ui.completions.clear();
                    ui.set_input(String::new());
                    ui.history.push(pick.clone());
                    ui.hist_idx = None;
                    return Action::Submit(pick);
                }
                ui.set_input(format!("{pick} "));
                ui.refresh_completions(agents);
                return Action::None;
            }
            let line = ui.input.trim().to_string();
            ui.set_input(String::new());
            ui.completions.clear();
            ui.hist_idx = None;
            ui.scroll = 0;
            if line.is_empty() {
                return Action::None;
            }
            if ui.history.last() != Some(&line) {
                ui.history.push(line.clone());
            }
            return Action::Submit(line);
        }
        _ => {}
    }
    ui.refresh_completions(agents);
    Action::None
}

/// Where a plain-text message goes (M5-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRoute {
    /// Save as `sources/BRIEF.md` and draft the spec.
    Draft,
    /// Revise the existing draft with this feedback.
    Revise,
    /// Queue as guidance for the running engine's orchestrator.
    Guidance,
    /// Queue as guidance for the running spec loop's next revision.
    SpecGuidance,
    /// No engine: append to `STRATEGY.md` for the next run.
    Strategy,
}

pub fn text_route(phase: &str, has_brief: bool, engine_running: bool) -> TextRoute {
    match phase {
        "init" | "spec" if engine_running => TextRoute::SpecGuidance,
        "init" | "spec" if !has_brief => TextRoute::Draft,
        "init" | "spec" => TextRoute::Revise,
        _ if engine_running => TextRoute::Guidance,
        _ => TextRoute::Strategy,
    }
}

enum Msg {
    Lines(Vec<Line>),
    Done(String),
    Reload,
}

struct Runner {
    ctx: Ctx,
    tx: mpsc::Sender<Msg>,
}

impl Runner {
    /// Run `vdash <args>` in the background and stream its output into the transcript.
    fn vdash(&self, label: &str, who: &'static str, args: Vec<String>) {
        let exe = self.ctx.exe.clone();
        let root = self.ctx.campaign.root.clone();
        let tx = self.tx.clone();
        let label = label.to_string();
        std::thread::spawn(move || {
            let mut cmd = Command::new(&exe);
            cmd.arg("-C").arg(&root).args(&args);
            let res = crate::proc::run(&mut cmd, None, Duration::from_secs(4 * 3600));
            let mut lines = Vec::new();
            match res {
                Ok(out) => {
                    let text = format!("{}{}", out.stdout_lossy(), out.stderr_lossy());
                    let ok = out.status == Some(0);
                    let body = text.trim_end();
                    if !body.is_empty() {
                        lines.push(Line::new(if ok { Kind::Output } else { Kind::Bad }, who, body.to_string()));
                    }
                    if !ok && body.is_empty() {
                        lines.push(Line::new(Kind::Bad, who, format!("/{label} failed (exit {:?})", out.status)));
                    }
                }
                Err(e) => lines.push(Line::new(Kind::Bad, who, e.message)),
            }
            let _ = tx.send(Msg::Lines(lines));
            let _ = tx.send(Msg::Done(label));
            let _ = tx.send(Msg::Reload);
        });
    }
}

fn role_table(ctx: &Ctx, app: &App) -> String {
    let mut s = String::from("role           backend[:model]\n");
    for r in Role::ALL {
        let es: Vec<String> = ctx.cfg.entries_for(r).iter().map(|e| e.to_string()).collect();
        s.push_str(&format!("{:<14} {}\n", r.to_string(), es.join(", ")));
    }
    s.push('\n');
    for (k, b) in &ctx.cfg.backends {
        let st = if app.backends.get(&k.to_string()).is_some_and(|h| h.unavailable) {
            "unavailable".to_string()
        } else if app.is_paused(&k.to_string()).is_some() {
            "paused at its usage limit".to_string()
        } else {
            format!("{} running, up to {}", app.running_on(&k.to_string()), b.max_concurrent)
        };
        s.push_str(&format!("{k:<8} `{}` · {st}\n", b.command.join(" ")));
    }
    s.push_str("\nAvailable models:\n");
    s.push_str(&format!("  claude   {}\n", commands::CLAUDE_MODELS.join(", ")));
    s.push_str(&format!("  gemini   {}\n", commands::GEMINI_MODELS.join(", ")));
    s.push_str("\nChange with:\n  /model <role|all> <backend>[:<model>]   (e.g. /model worker gemini:gemini-4-argon-high)\n  /model <claude|gemini> <model>          (sets the model for all roles on that backend)");
    s
}

pub fn check_backend_login(bin: &str, kind: config::BackendKind) -> (bool, String) {
    match kind {
        config::BackendKind::Claude => {
            let mut cmd = Command::new(bin);
            cmd.args(["auth", "status"]);
            if let Ok(out) = crate::proc::run(&mut cmd, None, Duration::from_secs(6)) {
                let text = format!("{}{}", out.stdout_lossy(), out.stderr_lossy());
                if out.status == Some(0) && !crate::backend::looks_like_not_logged_in(&text) {
                    let first = text.lines().next().unwrap_or("logged in").trim();
                    return (true, format!("claude ({bin}): signed in ({first})"));
                }
            }
            let mut vcmd = Command::new(bin);
            vcmd.arg("--version");
            match crate::proc::run(&mut vcmd, None, Duration::from_secs(5)) {
                Ok(out) if out.status == Some(0) => (
                    false,
                    format!("claude ({bin}): installed ({}) · run `/login claude` to sign in", out.stdout_lossy().trim()),
                ),
                _ => (false, format!("claude ({bin}): binary not found on PATH")),
            }
        }
        config::BackendKind::Gemini => {
            let mut vcmd = Command::new(bin);
            vcmd.arg("--version");
            match crate::proc::run(&mut vcmd, None, Duration::from_secs(5)) {
                Ok(out) if out.status == Some(0) => {
                    let ver = out.stdout_lossy().lines().next().unwrap_or("installed").trim().to_string();
                    (
                        true,
                        format!("gemini ({bin}): installed ({ver}) · run `/login gemini` to sign in or switch account"),
                    )
                }
                _ => (false, format!("gemini ({bin}): binary not found on PATH")),
            }
        }
    }
}

pub fn login_status(cfg: Option<&config::Config>) -> String {
    let claude_bin = cfg
        .and_then(|c| c.backends.get(&config::BackendKind::Claude))
        .and_then(|b| b.command.first())
        .map(String::as_str)
        .unwrap_or("claude");
    let gemini_bin = cfg
        .and_then(|c| c.backends.get(&config::BackendKind::Gemini))
        .and_then(|b| b.command.first())
        .map(String::as_str)
        .unwrap_or("agy");
    let (_, c_msg) = check_backend_login(claude_bin, config::BackendKind::Claude);
    let (_, g_msg) = check_backend_login(gemini_bin, config::BackendKind::Gemini);
    format!(
        "Account status:\n  • {c_msg}\n  • {g_msg}\n\nSign in interactively with `/login claude` or `/login gemini` (CLI: `vdash login claude` / `vdash login gemini`)."
    )
}

pub fn login_interactive(cfg: Option<&config::Config>, backend: &str) -> Result<String> {
    use std::process::Stdio;
    match backend {
        "claude" => {
            let bin = cfg
                .and_then(|c| c.backends.get(&config::BackendKind::Claude))
                .and_then(|b| b.command.first())
                .map(String::as_str)
                .unwrap_or("claude");
            println!("\n⊢ vdash · launching Claude sign-in (`{bin} auth login --claudeai`)...\n");
            let status = Command::new(bin)
                .args(["auth", "login", "--claudeai"])
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status();
            match status {
                Ok(s) if s.success() => Ok("Claude sign-in completed.".to_string()),
                Ok(_) => {
                    println!("\n⊢ vdash · opening interactive `{bin}` session (type `/login` then `/exit`)...\n");
                    let s2 = Command::new(bin)
                        .stdin(Stdio::inherit())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .status()
                        .map_err(|e| VdashError::new(5, format!("could not launch {bin}: {e}")))?;
                    if s2.success() {
                        Ok("Claude interactive sign-in finished.".to_string())
                    } else {
                        Err(VdashError::new(1, format!("Claude sign-in exited with {s2}")))
                    }
                }
                Err(e) => Err(VdashError::new(5, format!("could not launch {bin}: {e}"))),
            }
        }
        "gemini" => {
            let bin = cfg
                .and_then(|c| c.backends.get(&config::BackendKind::Gemini))
                .and_then(|b| b.command.first())
                .map(String::as_str)
                .unwrap_or("agy");
            println!(
                "\n⊢ vdash · launching Gemini / Antigravity (`{bin}`) for interactive sign-in.\n  Follow any on-screen OAuth prompts, then exit (`Ctrl-C` or `/quit`) to return to Vdash.\n"
            );
            let status = Command::new(bin)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|e| VdashError::new(5, format!("could not launch {bin}: {e}")))?;
            if status.success() {
                Ok("Gemini interactive sign-in finished.".to_string())
            } else {
                Ok(format!("Returned from `{bin}` ({status})."))
            }
        }
        other => Err(VdashError::new(
            2,
            format!("unknown backend '{other}' (expected claude or gemini)"),
        )),
    }
}

fn logs_of(ctx: &Ctx, agent: &str) -> Result<String> {
    let mut found = None;
    for e in walkdir::WalkDir::new(ctx.campaign.logs_dir()).max_depth(2).into_iter().flatten() {
        if e.file_name().to_string_lossy() == format!("{agent}.json") {
            found = Some(e.path().to_path_buf());
        }
    }
    let Some(p) = found else {
        return Err(VdashError::new(2, format!("no transcript for {agent} (agent ids look like worker-3)")));
    };
    let v: serde_json::Value = crate::jsonio::read_json(&p)?;
    let mut s = format!(
        "{} · {} · {}{} · {:.0}s · ${:.3}\n",
        v["agent"].as_str().unwrap_or(agent),
        v["role"].as_str().unwrap_or(""),
        v["backend"].as_str().unwrap_or(""),
        v["model"].as_str().map(|m| format!(":{m}")).unwrap_or_default(),
        v["seconds"].as_f64().unwrap_or(0.0),
        v["cost_usd"].as_f64().unwrap_or(0.0)
    );
    if let Some(f) = v["failure"].as_str() {
        s.push_str(&format!("failed: {f}\n"));
    }
    if let Some(d) = v["denials"].as_array().filter(|d| !d.is_empty()) {
        s.push_str(&format!("denied tool calls: {}\n", d.len()));
    }
    if v["reply"].is_object() {
        s.push_str(&serde_json::to_string_pretty(&v["reply"]).unwrap_or_default());
    }
    s.push_str(&format!("\nfull transcript: {}", p.display()));
    Ok(s)
}

fn usage_text(ctx: &Ctx) -> String {
    let Ok(st) = util::load_state(&ctx.campaign) else { return "no state".into() };
    if st.usage.is_empty() {
        return "no agent calls yet".into();
    }
    let mut s = String::from("role          backend  calls    time     cost\n");
    let (mut c, mut n) = (0.0, 0);
    for (role, m) in &st.usage {
        for (b, u) in m {
            c += u.cost_usd;
            n += u.calls;
            s.push_str(&format!("{role:<13} {b:<8} {:>5} {:>8} {:>8}\n", u.calls, app::human_secs(u.seconds), format!("${:.3}", u.cost_usd)));
        }
    }
    s.push_str(&format!("{:<13} {:<8} {n:>5} {:>8} {:>8}", "total", "", "", format!("${c:.3}")));
    s
}

const LOOP_HINT: &str = "It keeps revising until all four reviewers say ok: type to add guidance between rounds, /stop when the spec is good enough.";

fn help_text() -> String {
    let mut s = String::new();
    for (n, a, d) in commands::COMMANDS {
        let usage = if a.is_empty() { format!("/{n}") } else { format!("/{n} {a}") };
        s.push_str(&format!("{usage:<42} {d}\n"));
    }
    s.push_str("\nPlain text goes to the spec-writer before approval, and to the orchestrator as guidance afterwards.");
    s
}

struct Session {
    ctx: Ctx,
    app: App,
    ui: Ui,
    runner: Runner,
    rx: mpsc::Receiver<Msg>,
    offset: u64,
    pending_login: Option<String>,
    /// `/approve` arrived while a spec loop ran: approve once it has stopped.
    pending_approve: bool,
    /// `/verbose` is on: show proofs as agents write them.
    verbose: Option<watch::ProofWatch>,
}

/// Busy labels of the commands that run a spec loop.
const SPEC_LOOP_LABELS: [&str; 3] = ["spec draft", "spec revise", "review"];

impl Session {
    fn reload_state(&mut self) {
        if let Ok(st) = util::load_state(&self.ctx.campaign) {
            self.app.load_state(&st);
        }
        self.app.engine = events::engine_running(&self.ctx);
    }

    /// A spec loop is running (or starting) for this campaign.
    fn spec_loop_running(&self) -> bool {
        matches!(self.app.phase.as_str(), "init" | "spec")
            && (self.app.engine.is_some() || self.ui.busy.iter().any(|b| SPEC_LOOP_LABELS.contains(&b.as_str())))
    }

    /// Start a spec loop, unless one is already running.
    fn spec_loop(&mut self, label: &str, args: Vec<String>) {
        if self.spec_loop_running() {
            self.app.say(Kind::Warn, "vdash", "a spec loop is already running: type to add guidance for its next revision, or /stop when the spec is good enough");
            return;
        }
        self.busy(label);
        self.runner.vdash(label, "spec", args);
    }

    fn busy(&mut self, label: &str) {
        self.ui.busy.push(label.to_string());
    }

    fn exec(&mut self, line: String) -> bool {
        match commands::parse(&line) {
            Err(e) => {
                self.app.say(Kind::User, "you", line);
                self.app.say(Kind::Warn, "vdash", e);
            }
            Ok(Input::Text(t)) => {
                self.app.say(Kind::User, "you", t.clone());
                self.text(t);
            }
            Ok(Input::Cmd(c)) => {
                self.app.say(Kind::User, "you", line.clone());
                return self.cmd(c);
            }
        }
        false
    }

    fn text(&mut self, t: String) {
        let brief = self.ctx.campaign.brief_file();
        let has_brief = std::fs::read_to_string(&brief).map(|s| !s.trim().is_empty()).unwrap_or(false);
        match text_route(&self.app.phase, has_brief, self.app.engine.is_some() || self.spec_loop_running()) {
            TextRoute::Draft => {
                if let Some(d) = brief.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(&brief, format!("{t}\n"));
                self.app.say(Kind::Info, "vdash", format!("saved as sources/BRIEF.md; the spec-writer is drafting, then four reviewers critique the draft. {LOOP_HINT}"));
                self.spec_loop("spec draft", vec!["spec".into(), "draft".into()]);
            }
            TextRoute::Revise => {
                self.app.say(Kind::Info, "vdash", format!("sent to the spec-writer as feedback; the reviewers run again afterwards. {LOOP_HINT}"));
                self.spec_loop("spec revise", vec!["spec".into(), "revise".into(), t]);
            }
            TextRoute::SpecGuidance => match events::send(&self.ctx, &Control::Guidance { text: t }) {
                Ok(()) => self.app.say(Kind::Info, "vdash", "guidance queued for the spec-writer's next revision"),
                Err(e) => self.app.say(Kind::Bad, "vdash", e.message),
            },
            TextRoute::Guidance => match events::send(&self.ctx, &Control::Guidance { text: t }) {
                Ok(()) => self.app.say(Kind::Info, "vdash", "guidance queued for the orchestrator's next round"),
                Err(e) => self.app.say(Kind::Bad, "vdash", e.message),
            },
            TextRoute::Strategy => {
                let p = self.ctx.campaign.strategy_file();
                let mut cur = std::fs::read_to_string(&p).unwrap_or_default();
                cur.push_str(&format!("\n## Guidance ({})\n\n{}\n", util::now_iso(), t.trim()));
                match std::fs::write(&p, cur) {
                    Ok(()) => self.app.say(Kind::Info, "vdash", "added to STRATEGY.md; the architect and orchestrator read it"),
                    Err(e) => self.app.say(Kind::Bad, "vdash", e.to_string()),
                }
            }
        }
    }

    fn control(&mut self, c: Control, word: &str) {
        if self.app.engine.is_none() {
            self.app.say(Kind::Warn, "vdash", "no engine is running; start one with /prove");
            return;
        }
        let when = if matches!(self.app.phase.as_str(), "init" | "spec") {
            "the spec loop acts on it after its current writer/reviewer step"
        } else {
            "the engine acts on it before its next agent or stage"
        };
        match events::send(&self.ctx, &c) {
            Ok(()) => self.app.say(Kind::Info, "vdash", format!("{word} sent; {when}")),
            Err(e) => self.app.say(Kind::Bad, "vdash", e.message),
        }
    }

    fn cmd(&mut self, c: Cmd) -> bool {
        let out = |s: &mut Session, who: &str, text: String| s.app.say(Kind::Output, who, text);
        match c {
            Cmd::Quit => return true,
            Cmd::Help => out(self, "help", help_text()),
            Cmd::Clear => self.app.transcript.clear(),
            Cmd::Status => match crate::status::render(&self.ctx) {
                Ok(s) => out(self, "status", s.trim_end().to_string()),
                Err(e) => self.app.say(Kind::Bad, "status", e.message),
            },
            Cmd::Model(None) => {
                let t = role_table(&self.ctx, &self.app);
                out(self, "model", t)
            }
            Cmd::Model(Some((role, entry))) => {
                let parsed: Result<RoleEntry> = entry.parse();
                let Ok(parsed) = parsed else {
                    self.app.say(Kind::Warn, "model", format!("cannot parse '{entry}'"));
                    return false;
                };
                if !self.ctx.cfg.backends.contains_key(&parsed.backend) {
                    self.app.say(Kind::Warn, "model", format!("backend {} is not configured in campaign.toml", parsed.backend));
                    return false;
                }
                let roles: Vec<Role> = if role == "all" {
                    Role::ALL.to_vec()
                } else if role == "claude" || role == "gemini" {
                    let matching: Vec<Role> = Role::ALL
                        .into_iter()
                        .filter(|&r| {
                            self.ctx
                                .cfg
                                .entries_for(r)
                                .first()
                                .is_some_and(|e| e.backend == parsed.backend)
                        })
                        .collect();
                    if matching.is_empty() {
                        Role::ALL.to_vec()
                    } else {
                        matching
                    }
                } else {
                    role.parse().map(|r| vec![r]).unwrap_or_default()
                };
                for r in &roles {
                    if self.app.engine.is_some() {
                        let _ = events::send(&self.ctx, &Control::SetRole { role: r.to_string(), entries: vec![entry.clone()] });
                    } else if let Err(e) = config::set_role(&self.ctx.campaign.root, *r, std::slice::from_ref(&parsed)) {
                        self.app.say(Kind::Bad, "model", e.message);
                        return false;
                    }
                }
                if let Ok(cfg) = config::load(&self.ctx.campaign.root) {
                    self.ctx.cfg = cfg;
                }
                let how = if self.app.engine.is_some() { "sent to the engine; it applies to the next attempt of every agent, including one waiting on a usage limit" } else { "saved to campaign.toml" };
                let target_desc = if role == "claude" || role == "gemini" {
                    format!("all {role} roles ({})", roles.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(", "))
                } else {
                    role
                };
                self.app.say(Kind::Good, "model", format!("{target_desc} → {entry} ({how})"));
            }
            Cmd::Login(None) => {
                self.busy("login");
                self.runner.vdash("login", "login", vec!["login".into()]);
            }
            Cmd::Login(Some(b)) => {
                self.pending_login = Some(b);
            }
            Cmd::Search(q) => {
                self.busy("search");
                self.runner.vdash("search", "search", vec!["search".into(), q]);
            }
            Cmd::Btw(q) => {
                self.busy("btw");
                self.runner.vdash("btw", "monitor", vec!["btw".into(), q]);
            }
            Cmd::Spec(None) => self.spec_loop("spec draft", vec!["spec".into(), "draft".into()]),
            Cmd::Spec(Some(t)) => self.spec_loop("spec revise", vec!["spec".into(), "revise".into(), t]),
            Cmd::Review => self.spec_loop("review", vec!["spec".into(), "review".into()]),
            Cmd::Approve if self.spec_loop_running() => {
                if self.app.engine.is_some() {
                    let _ = events::send(&self.ctx, &Control::Stop);
                }
                self.pending_approve = true;
                self.app.say(Kind::Info, "vdash", "the spec loop stops after its current step; the spec is approved then");
            }
            Cmd::Approve => {
                self.busy("approve");
                self.runner.vdash("approve", "approve", vec!["approve".into()]);
            }
            Cmd::Restart => {
                self.busy("restart");
                self.runner.vdash("restart", "restart", vec!["restart".into()]);
            }
            Cmd::Prove { single } => {
                if let Some(pid) = self.app.engine {
                    self.app.say(Kind::Info, "vdash", format!("the engine is already running (pid {pid})"));
                } else {
                    let mut args = vec!["prove".to_string(), "--detach".to_string()];
                    if single {
                        args.push("--single".into());
                    }
                    self.busy("prove");
                    self.runner.vdash("prove", "engine", args);
                }
            }
            Cmd::Pause => self.control(Control::Pause, "pause"),
            Cmd::Resume => self.control(Control::Resume, "resume"),
            Cmd::Stop => self.control(Control::Stop, "stop"),
            Cmd::Kill => match crate::kill::kill(&self.ctx) {
                Ok(s) => self.app.say(Kind::Warn, "vdash", s),
                Err(e) => self.app.say(Kind::Warn, "vdash", e.message),
            },
            Cmd::Verbose => {
                if self.verbose.take().is_some() {
                    self.app.say(Kind::Info, "vdash", "verbose off");
                } else {
                    let mut w = watch::ProofWatch::default();
                    w.poll(&self.ctx); // baseline: only later changes are shown
                    self.verbose = Some(w);
                    self.app.say(Kind::Info, "vdash", "verbose on: new Lean lines in the proof files show here as agents write them");
                }
            }
            Cmd::Gate => {
                self.busy("gate");
                self.runner.vdash("gate", "gate", vec!["gate".into()]);
            }
            Cmd::Tcb => {
                self.busy("tcb");
                self.runner.vdash("tcb", "tcb", vec!["tcb".into()]);
            }
            Cmd::Doctor => {
                self.busy("doctor");
                self.runner.vdash("doctor", "doctor", vec!["doctor".into()]);
            }
            Cmd::Amend(None) => {
                self.busy("amend");
                self.runner.vdash("amend", "amend", vec!["amend".into(), "list".into()]);
            }
            Cmd::Amend(Some((approve, n))) => {
                self.busy("amend");
                self.runner.vdash("amend", "amend", vec!["amend".into(), if approve { "approve" } else { "reject" }.into(), n.to_string()]);
            }
            Cmd::AmendPropose(f) => {
                self.busy("amend");
                self.runner.vdash("amend", "amend", vec!["amend".into(), "propose".into(), f]);
            }
            Cmd::Agents => {
                let text = if self.app.agents.is_empty() {
                    "no agents running".to_string()
                } else {
                    self.app
                        .agents
                        .values()
                        .map(|a| {
                            format!("{:<14} {:<7} {:<10} {}", a.id, a.backend, a.goal, app::seconds_since(&a.started).map(app::human_secs).unwrap_or_default())
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                out(self, "agents", text)
            }
            Cmd::Logs(a) => match logs_of(&self.ctx, &a) {
                Ok(s) => out(self, &a, s),
                Err(e) => self.app.say(Kind::Warn, "logs", e.message),
            },
            Cmd::Bulletin => {
                let b = std::fs::read_to_string(self.ctx.campaign.bulletin_file()).unwrap_or_default();
                let lines: Vec<&str> = b.lines().filter(|l| !l.trim().is_empty()).collect();
                let tail = lines[lines.len().saturating_sub(20)..].join("\n");
                out(self, "bulletin", if tail.is_empty() { "the bulletin is empty".into() } else { tail })
            }
            Cmd::Cost => {
                let t = usage_text(&self.ctx);
                out(self, "cost", t)
            }
        }
        false
    }

    fn pump(&mut self) -> bool {
        let mut changed = false;
        while let Ok(m) = self.rx.try_recv() {
            changed = true;
            match m {
                Msg::Lines(ls) => {
                    for l in ls {
                        self.app.push(l);
                    }
                }
                Msg::Done(label) => {
                    if let Some(i) = self.ui.busy.iter().position(|b| *b == label) {
                        self.ui.busy.remove(i);
                    }
                    let loop_left = self.ui.busy.iter().any(|b| SPEC_LOOP_LABELS.contains(&b.as_str()));
                    if self.pending_approve && SPEC_LOOP_LABELS.contains(&label.as_str()) && !loop_left {
                        self.pending_approve = false;
                        self.busy("approve");
                        self.runner.vdash("approve", "approve", vec!["approve".into()]);
                    }
                }
                Msg::Reload => self.reload_state(),
            }
        }
        if let Some(w) = self.verbose.as_mut() {
            for (file, text) in w.poll(&self.ctx) {
                changed = true;
                self.app.say(Kind::Output, &file, text);
            }
        }
        let (evs, off) = events::follow(&self.ctx.campaign.events_file(), self.offset);
        if !evs.is_empty() {
            changed = true;
            for e in &evs {
                self.app.apply(e);
            }
        }
        self.offset = off;
        changed
    }
}

struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

/// Run the app until the user quits. Quitting never stops the engine.
pub fn run(ctx: Ctx) -> Result<()> {
    let name = ctx.campaign.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "campaign".into());
    let mut app = App::new(&name);
    let (evs, offset) = events::tail(&ctx.campaign.events_file(), 500);
    if !evs.is_empty() {
        app.say(Kind::Output, "vdash", view::banner_text());
    }
    for e in &evs {
        app.apply(e);
    }
    let (tx, rx) = mpsc::channel();
    let mut s = Session { runner: Runner { ctx: ctx.clone(), tx }, ctx, app, ui: Ui::default(), rx, offset, pending_login: None, pending_approve: false, verbose: None };
    s.reload_state();
    if s.app.engine.is_none() {
        // An engine_started without a matching stop in the replay is stale.
        s.app.agents.clear();
    }

    enable_raw_mode().map_err(|e| VdashError::new(5, format!("terminal: {e}")))?;
    let _guard = TermGuard;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste).map_err(|e| VdashError::new(5, format!("terminal: {e}")))?;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        prev(info);
    }));
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout())).map_err(|e| VdashError::new(5, format!("terminal: {e}")))?;
    let mut last_reload = Instant::now();
    let mut dirty = true;
    loop {
        if dirty {
            term.draw(|f| view::draw(f, &s.app, &s.ui)).map_err(|e| VdashError::new(5, format!("terminal: {e}")))?;
            dirty = false;
        }
        if let Some(backend) = s.pending_login.take() {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen, crossterm::cursor::Show);
            let res = login_interactive(Some(&s.ctx.cfg), &backend);
            let _ = execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste);
            let _ = enable_raw_mode();
            let _ = term.clear();
            match res {
                Ok(msg) => {
                    s.app.backends.entry(backend.clone()).or_default().unavailable = false;
                    s.app.say(Kind::Good, "login", msg);
                }
                Err(e) => {
                    s.app.say(Kind::Warn, "login", e.message);
                }
            }
            dirty = true;
            continue;
        }
        if event::poll(Duration::from_millis(80)).unwrap_or(false) {
            match event::read() {
                Ok(CEvent::Key(k)) => {
                    let agents = s.app.seen_agents.clone();
                    match handle_key(&mut s.ui, k, &agents) {
                        Action::Quit => break,
                        Action::Submit(line) => {
                            if s.exec(line) {
                                break;
                            }
                        }
                        Action::None => {}
                    }
                    dirty = true;
                }
                Ok(CEvent::Paste(text)) => {
                    for c in text.chars() {
                        if c != '\r' {
                            s.ui.insert(if c == '\n' { ' ' } else { c });
                        }
                    }
                    dirty = true;
                }
                Ok(CEvent::Resize(..)) => dirty = true,
                Ok(CEvent::Mouse(m)) => match m.kind {
                    event::MouseEventKind::ScrollUp => {
                        s.ui.scroll += 3;
                        dirty = true;
                    }
                    event::MouseEventKind::ScrollDown => {
                        s.ui.scroll = s.ui.scroll.saturating_sub(3);
                        dirty = true;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if s.pump() {
            dirty = true;
        }
        if last_reload.elapsed() >= Duration::from_secs(1) {
            s.reload_state();
            last_reload = Instant::now();
            dirty = true;
        }
        s.ui.tick = s.ui.tick.wrapping_add(1);
        if !s.app.agents.is_empty() || !s.ui.busy.is_empty() {
            dirty = true;
        }
    }
    drop(term);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn typ(ui: &mut Ui, s: &str) {
        for c in s.chars() {
            handle_key(ui, k(KeyCode::Char(c)), &[]);
        }
    }

    #[test]
    fn typing_editing_and_submit() {
        let mut ui = Ui::default();
        typ(&mut ui, "helo");
        handle_key(&mut ui, k(KeyCode::Left), &[]);
        typ(&mut ui, "l");
        assert_eq!(ui.input, "hello");
        handle_key(&mut ui, k(KeyCode::Home), &[]);
        handle_key(&mut ui, k(KeyCode::Delete), &[]);
        assert_eq!(ui.input, "ello");
        handle_key(&mut ui, k(KeyCode::End), &[]);
        handle_key(&mut ui, k(KeyCode::Backspace), &[]);
        assert_eq!(ui.input, "ell");
        typ(&mut ui, " world");
        handle_key(&mut ui, ctrl('w'), &[]);
        assert_eq!(ui.input, "ell ");
        assert_eq!(handle_key(&mut ui, k(KeyCode::Enter), &[]), Action::Submit("ell".into()));
        assert!(ui.input.is_empty());
    }

    #[test]
    fn history_navigation() {
        let mut ui = Ui::default();
        typ(&mut ui, "one");
        handle_key(&mut ui, k(KeyCode::Enter), &[]);
        typ(&mut ui, "two");
        handle_key(&mut ui, k(KeyCode::Enter), &[]);
        handle_key(&mut ui, k(KeyCode::Up), &[]);
        assert_eq!(ui.input, "two");
        handle_key(&mut ui, k(KeyCode::Up), &[]);
        assert_eq!(ui.input, "one");
        handle_key(&mut ui, k(KeyCode::Down), &[]);
        assert_eq!(ui.input, "two");
        handle_key(&mut ui, k(KeyCode::Down), &[]);
        assert_eq!(ui.input, "");
    }

    #[test]
    fn completion_with_tab_and_enter() {
        let mut ui = Ui::default();
        typ(&mut ui, "/ga");
        assert_eq!(ui.completions, vec!["/gate".to_string()]);
        // Enter on a complete argument-less command runs it.
        assert_eq!(handle_key(&mut ui, k(KeyCode::Enter), &[]), Action::Submit("/gate".into()));
        typ(&mut ui, "/mo");
        handle_key(&mut ui, k(KeyCode::Tab), &[]);
        assert_eq!(ui.input, "/model ");
        typ(&mut ui, "wor");
        assert_eq!(ui.completions, vec!["/model worker".to_string()]);
        handle_key(&mut ui, k(KeyCode::Esc), &[]);
        assert!(ui.completions.is_empty());
    }

    #[test]
    fn ctrl_c_clears_then_asks_then_quits() {
        let mut ui = Ui::default();
        typ(&mut ui, "abc");
        assert_eq!(handle_key(&mut ui, ctrl('c'), &[]), Action::None);
        assert!(ui.input.is_empty());
        assert_eq!(handle_key(&mut ui, ctrl('c'), &[]), Action::None);
        assert!(ui.confirm_quit);
        assert_eq!(handle_key(&mut ui, ctrl('c'), &[]), Action::Quit);
        assert_eq!(handle_key(&mut Ui::default(), ctrl('d'), &[]), Action::Quit);
    }

    fn session(tmp: &tempfile::TempDir) -> Session {
        let host = tmp.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
        let camp = tmp.path().join("c");
        crate::paths::init_campaign(&camp, &host, None, &crate::config::BackendKind::ALL).unwrap();
        let ctx = Ctx::for_test(&camp, tmp.path().join("state")).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut s = Session { runner: Runner { ctx: ctx.clone(), tx }, ctx, app: App::new("c"), ui: Ui::default(), rx, offset: 0, pending_login: None, pending_approve: false, verbose: None };
        s.reload_state();
        s
    }

    #[test]
    fn verbose_toggles_and_shows_new_proof_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut s = session(&tmp);
        let proof = s.ctx.campaign.proof_dir();
        std::fs::create_dir_all(&proof).unwrap();
        std::fs::write(proof.join("A.lean"), "theorem a : True := by\n  trivial\n").unwrap();
        s.exec("/verbose".into());
        assert!(s.verbose.is_some());
        s.pump();
        assert!(!s.app.transcript.iter().any(|l| l.text.contains("theorem b")));
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(proof.join("A.lean"), "theorem a : True := by\n  trivial\ntheorem b : 1 = 1 := by\n  rfl\n").unwrap();
        s.pump();
        assert!(s.app.transcript.iter().any(|l| l.text.contains("theorem b")));
        s.exec("/verbose".into());
        assert!(s.verbose.is_none());
    }

    #[test]
    fn controls_need_an_engine_and_land_in_the_inbox() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut s = session(&tmp);
        s.exec("/pause".into());
        assert!(s.app.transcript.last().unwrap().text.contains("no engine is running"));
        let _lock = crate::events::EngineLock::acquire(&s.ctx).unwrap();
        s.reload_state();
        s.exec("/stop".into());
        assert_eq!(crate::events::drain(&s.ctx), vec![Control::Stop]);
        s.app.phase = "proving".into();
        s.exec("try strong induction".into());
        assert_eq!(crate::events::drain(&s.ctx), vec![Control::Guidance { text: "try strong induction".into() }]);
        s.exec("/model worker gemini:gemini-4-argon-high".into());
        assert_eq!(
            crate::events::drain(&s.ctx),
            vec![Control::SetRole { role: "worker".into(), entries: vec!["gemini:gemini-4-argon-high".into()] }]
        );
    }

    #[test]
    fn model_without_engine_edits_config_and_text_goes_to_strategy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut s = session(&tmp);
        s.exec("/model critic gemini".into());
        let cfg = config::load(&s.ctx.campaign.root).unwrap();
        assert_eq!(cfg.entries_for(Role::Critic)[0].to_string(), "gemini");
        s.exec("/model".into());
        assert!(s.app.transcript.last().unwrap().text.contains("critic         gemini"));
        s.app.phase = "approved".into();
        s.exec("prove the permutation lemma first".into());
        let strat = std::fs::read_to_string(s.ctx.campaign.strategy_file()).unwrap();
        assert!(strat.contains("prove the permutation lemma first"));
        s.exec("/frobnicate".into());
        assert_eq!(s.app.transcript.last().unwrap().kind, Kind::Warn);
        assert!(s.exec("/quit".into()));
    }

    #[test]
    fn plain_text_routing() {
        assert_eq!(text_route("init", false, false), TextRoute::Draft);
        assert_eq!(text_route("spec", true, false), TextRoute::Revise);
        assert_eq!(text_route("init", true, false), TextRoute::Revise);
        assert_eq!(text_route("spec", true, true), TextRoute::SpecGuidance);
        assert_eq!(text_route("proving", true, true), TextRoute::Guidance);
        assert_eq!(text_route("approved", true, false), TextRoute::Strategy);
        assert_eq!(text_route("done", false, false), TextRoute::Strategy);
    }

    #[test]
    fn page_keys_scroll() {
        let mut ui = Ui::default();
        handle_key(&mut ui, k(KeyCode::PageUp), &[]);
        assert_eq!(ui.scroll, 10);
        handle_key(&mut ui, k(KeyCode::PageDown), &[]);
        assert_eq!(ui.scroll, 0);
    }
}
