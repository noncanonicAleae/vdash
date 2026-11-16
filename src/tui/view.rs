//! Rendering (ROADMAP M4-03). Colour always comes with a symbol, so the screen
//! still reads under `NO_COLOR`.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::app::{human_secs, seconds_since, App, Kind};
use super::commands;
use super::Ui;

pub struct Palette {
    pub accent: Color,
    pub good: Color,
    pub bad: Color,
    pub warn: Color,
    pub info: Color,
    pub muted: Color,
    pub faint: Color,
    pub claude: Color,
    pub gemini: Color,
    pub text: Color,
}

pub fn palette() -> Palette {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        let r = Color::Reset;
        return Palette { accent: r, good: r, bad: r, warn: r, info: r, muted: r, faint: r, claude: r, gemini: r, text: r };
    }
    Palette {
        accent: Color::Rgb(160, 140, 255),
        good: Color::Rgb(115, 201, 145),
        bad: Color::Rgb(240, 113, 120),
        warn: Color::Rgb(232, 190, 110),
        info: Color::Rgb(125, 200, 245),
        muted: Color::Rgb(128, 134, 156),
        faint: Color::Rgb(78, 84, 104),
        claude: Color::Rgb(222, 128, 94),
        gemini: Color::Rgb(96, 150, 250),
        text: Color::Reset,
    }
}

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(tick: u64) -> &'static str {
    SPIN[(tick as usize) % SPIN.len()]
}

fn backend_color(p: &Palette, b: &str) -> Color {
    match b {
        "claude" => p.claude,
        "gemini" => p.gemini,
        _ => p.muted,
    }
}

fn who_color(p: &Palette, who: &str) -> Color {
    let base = who.split('-').next().unwrap_or(who);
    match base {
        "claude" => p.claude,
        "gemini" => p.gemini,
        "gate" => p.accent,
        "policy" | "escalation" => p.warn,
        "engine" | "vdash" => p.muted,
        "you" => p.text,
        w if w.starts_with('L') && w[1..].chars().all(|c| c.is_ascii_digit()) => p.info,
        _ => p.muted,
    }
}

fn icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Rule => "─",
        Kind::Info => "•",
        Kind::Good => "✓",
        Kind::Bad => "✗",
        Kind::Warn => "!",
        Kind::User => "›",
        Kind::Output => "│",
        Kind::Voice => "❝",
        Kind::Muted => "·",
    }
}

fn kind_color(p: &Palette, kind: Kind) -> Color {
    match kind {
        Kind::Rule => p.faint,
        Kind::Info => p.info,
        Kind::Good => p.good,
        Kind::Bad => p.bad,
        Kind::Warn => p.warn,
        Kind::User => p.accent,
        Kind::Output => p.faint,
        Kind::Voice => p.info,
        Kind::Muted => p.faint,
    }
}

/// Greedy word wrap by character count.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut cur = String::new();
        let mut len = 0usize;
        for word in raw.split(' ') {
            let wl = word.chars().count();
            if len > 0 && len + 1 + wl > width {
                out.push(std::mem::take(&mut cur));
                len = 0;
            }
            if wl > width {
                // Hard-break very long words.
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(width) {
                    if len > 0 {
                        out.push(std::mem::take(&mut cur));
                    }
                    cur = chunk.iter().collect();
                    len = chunk.len();
                }
                continue;
            }
            if len > 0 {
                cur.push(' ');
                len += 1;
            }
            cur.push_str(word);
            len += wl;
        }
        out.push(cur);
    }
    out
}

const WHO_W: usize = 13;

fn transcript_lines<'a>(app: &'a App, p: &Palette, width: usize) -> Vec<TLine<'a>> {
    let mut out: Vec<TLine> = Vec::new();
    let text_w = width.saturating_sub(6 + 3 + WHO_W + 1).max(10);
    for l in &app.transcript {
        if l.kind == Kind::Rule {
            let label = format!(" {} ", l.text);
            let left = 2usize;
            let right = width.saturating_sub(left + label.chars().count() + 1);
            out.push(TLine::from(vec![
                Span::styled("─".repeat(left), Style::default().fg(p.faint)),
                Span::styled(label, Style::default().fg(p.muted).add_modifier(Modifier::BOLD)),
                Span::styled("─".repeat(right), Style::default().fg(p.faint)),
            ]));
            continue;
        }
        let kc = kind_color(p, l.kind);
        let text_style = match l.kind {
            Kind::User => Style::default().add_modifier(Modifier::BOLD),
            Kind::Muted => Style::default().fg(p.muted),
            Kind::Output => Style::default(),
            Kind::Voice => Style::default().add_modifier(Modifier::ITALIC),
            Kind::Bad => Style::default().fg(p.bad),
            Kind::Warn => Style::default().fg(p.warn),
            Kind::Good => Style::default().fg(p.good),
            _ => Style::default(),
        };
        let mut who: String = l.who.chars().take(WHO_W).collect();
        let pad = WHO_W.saturating_sub(who.chars().count());
        who.push_str(&" ".repeat(pad));
        for (i, chunk) in wrap(&l.text, text_w).into_iter().enumerate() {
            if i == 0 {
                out.push(TLine::from(vec![
                    Span::styled(format!(" {:>5} ", l.time), Style::default().fg(p.faint)),
                    Span::styled(format!(" {} ", icon(l.kind)), Style::default().fg(kc).add_modifier(Modifier::BOLD)),
                    Span::styled(who.clone(), Style::default().fg(who_color(p, &l.who))),
                    Span::raw(" "),
                    Span::styled(chunk, text_style),
                ]));
            } else {
                let lead = if l.kind == Kind::Output { " │ " } else { "   " };
                out.push(TLine::from(vec![
                    Span::raw(" ".repeat(7)),
                    Span::styled(lead, Style::default().fg(p.faint)),
                    Span::raw(" ".repeat(WHO_W + 1)),
                    Span::styled(chunk, text_style),
                ]));
            }
        }
    }
    out
}

pub const VDASH_BANNER: &[(&str, &str)] = &[
    ("  ████╗       ", "         "),
    ("  ██╔═╝       ", "         "),
    ("  ██║        ", "██╗   ██╗ "),
    ("  █████████╗ ", "██║   ██║ "),
    ("  ██╔══════╝ ", "╚██╗ ██╔╝ "),
    ("  ██║        ", " ╚████╔╝  "),
    ("  ██║        ", "  ╚═══╝   "),
    ("  ╚═╝        ", "          "),
    ("             ", "          "),
];

pub fn banner_text() -> String {
    let info = [
        "vdash@lean4",
        "───────────",
        "App:      AlphaProof Code, rebuilt for Claude and Gemini",
        "Backends: claude · gemini (agy)",
        "Verify:   Lean 4 kernel · leanchecker · held-out spec",
        "Search:   Mathlib + Loogle (/search, /model, /login)",
        "Trust:    only the gatekeeper accepts a proof",
        "          ███ ███ ███ ███ ███ ███ ███ ███",
        "",
    ];
    let mut lines = Vec::new();
    for ((l, r), rhs) in VDASH_BANNER.iter().zip(info.iter()) {
        lines.push(format!("{l}{r}{rhs}"));
    }
    lines.join("\n").trim_end().to_string()
}

fn welcome<'a>(app: &App, p: &Palette, width: usize) -> Vec<TLine<'a>> {
    let next = match app.phase.as_str() {
        "init" | "spec" => "Describe what you want proved, in plain words. The spec-writer drafts\na Lean spec and four reviewers critique it. Type /help for commands.",
        "approved" => "The spec is approved. Type /prove to start the agent team.",
        "proving" => "The team is at work. Guide it in plain words, or ask with /btw.",
        "done" => "Every target is accepted. /gate reruns the gatekeeper; /tcb shows what to trust.",
        _ => "Type /help for commands.",
    };
    let acc = app.targets.values().filter(|t| t.0).count();
    let total = app.targets.len();
    let camp_line = format!("{} ({} · {acc}/{total} targets)", app.campaign, app.phase);

    let key_style = Style::default().fg(p.info).add_modifier(Modifier::BOLD);
    let rhs: Vec<Vec<Span<'a>>> = vec![
        vec![
            Span::styled("vdash", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("@", Style::default().fg(p.muted)),
            Span::styled("lean4", Style::default().fg(p.good).add_modifier(Modifier::BOLD)),
        ],
        vec![Span::styled("───────────", Style::default().fg(p.faint))],
        vec![
            Span::styled("App:      ", key_style),
            Span::styled("AlphaProof Code, rebuilt for Claude and Gemini", Style::default()),
        ],
        vec![
            Span::styled("Campaign: ", key_style),
            Span::styled(camp_line, Style::default()),
        ],
        vec![
            Span::styled("Backends: ", key_style),
            Span::styled("claude", Style::default().fg(p.claude).add_modifier(Modifier::BOLD)),
            Span::styled(" · ", Style::default().fg(p.muted)),
            Span::styled("gemini (agy)", Style::default().fg(p.gemini).add_modifier(Modifier::BOLD)),
        ],
        vec![
            Span::styled("Verify:   ", key_style),
            Span::styled("Lean 4 kernel · leanchecker · held-out spec", Style::default()),
        ],
        vec![
            Span::styled("Commands: ", key_style),
            Span::styled("/prove · /model · /login · /search · /btw · /help", Style::default().fg(p.muted)),
        ],
        vec![
            Span::styled("Trust:    ", key_style),
            Span::styled("only the gatekeeper accepts a proof, and only against the approved spec.", Style::default().fg(p.muted)),
        ],
        vec![
            Span::styled("███ ", Style::default().fg(p.accent)),
            Span::styled("███ ", Style::default().fg(p.claude)),
            Span::styled("███ ", Style::default().fg(p.gemini)),
            Span::styled("███ ", Style::default().fg(p.good)),
            Span::styled("███ ", Style::default().fg(p.info)),
            Span::styled("███ ", Style::default().fg(p.warn)),
            Span::styled("███ ", Style::default().fg(p.bad)),
            Span::styled("███", Style::default().fg(p.muted)),
        ],
    ];

    let mut v = vec![TLine::from("")];
    for ((left_t, right_v), info_spans) in VDASH_BANNER.iter().zip(rhs) {
        let mut row = vec![
            Span::styled((*left_t).to_string(), Style::default().fg(p.good).add_modifier(Modifier::BOLD)),
            Span::styled((*right_v).to_string(), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
        ];
        row.extend(info_spans);
        v.push(TLine::from(row));
    }
    v.push(TLine::from(""));
    for l in wrap(&next.replace('\n', " "), width.saturating_sub(4)) {
        v.push(TLine::from(Span::styled(format!("  {l}"), Style::default())));
    }
    v
}

fn phase_badge(app: &App, p: &Palette) -> Span<'static> {
    let (label, color) = match app.phase.as_str() {
        "init" => ("NEW", p.muted),
        "spec" => ("SPEC", p.info),
        "approved" => ("APPROVED", p.accent),
        "proving" if app.paused => ("PAUSED", p.warn),
        "proving" => ("PROVING", p.claude),
        "done" => ("DONE", p.good),
        _ => ("?", p.muted),
    };
    Span::styled(format!(" {label} "), Style::default().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD))
}

fn progress(acc: usize, total: usize, p: &Palette) -> Vec<Span<'static>> {
    let n = total.min(12);
    let filled = if total == 0 { 0 } else { (acc * n + total - 1) / total.max(1) };
    let filled = if acc == total { n } else { filled.min(n.saturating_sub(1)) };
    vec![
        Span::styled("▰".repeat(filled), Style::default().fg(p.good)),
        Span::styled("▱".repeat(n - filled), Style::default().fg(p.faint)),
        Span::styled(format!(" {acc}/{total}"), Style::default().add_modifier(Modifier::BOLD)),
    ]
}

fn header(f: &mut Frame, area: Rect, app: &App, ui: &Ui, p: &Palette) {
    let acc = app.targets.values().filter(|t| t.0).count();
    let total = app.targets.len();
    let stage = if app.round == 0 { "not started".to_string() } else { format!("round {} · {}", app.round, app.stage.replace('_', " ")) };
    let mut l1 = vec![
        Span::styled(" ⊢ ◆ vdash ", Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}  ", app.campaign), Style::default().add_modifier(Modifier::BOLD)),
        phase_badge(app, p),
        Span::styled(format!("  {stage}"), Style::default().fg(p.muted)),
        Span::raw("    targets "),
    ];
    l1.extend(progress(acc, total, p));
    l1.push(Span::styled(format!("    ${:.2} · {} calls", app.cost, app.calls), Style::default().fg(p.muted)));
    if let Some(t) = app.engine_started.as_deref().and_then(seconds_since).filter(|_| app.engine.is_some()) {
        l1.push(Span::styled(format!(" · {}", human_secs(t)), Style::default().fg(p.muted)));
    }
    let mut l2 = vec![Span::raw(" ")];
    for b in ["claude", "gemini"] {
        let color = backend_color(p, b);
        let running = app.running_on(b);
        let unavailable = app.backends.get(b).is_some_and(|h| h.unavailable);
        let (dot, state) = if unavailable {
            ("○", "unavailable".to_string())
        } else if let Some(left) = app.is_paused(b) {
            ("‖", format!("paused {}", human_secs(left)))
        } else if running > 0 {
            (spinner(ui.tick), format!("{running} running"))
        } else {
            ("●", "idle".to_string())
        };
        l2.push(Span::styled(format!("{dot} {b}"), Style::default().fg(color).add_modifier(Modifier::BOLD)));
        l2.push(Span::styled(format!(" {state}   "), Style::default().fg(p.muted)));
    }
    match app.engine {
        Some(pid) => {
            l2.push(Span::styled("engine ", Style::default().fg(p.muted)));
            l2.push(Span::styled(if app.paused { "paused" } else { "running" }, Style::default().fg(if app.paused { p.warn } else { p.good })));
            l2.push(Span::styled(format!(" (pid {pid})"), Style::default().fg(p.faint)));
        }
        None => l2.push(Span::styled("engine stopped", Style::default().fg(p.faint))),
    }
    let block = Block::default().borders(Borders::BOTTOM).border_style(Style::default().fg(p.faint));
    f.render_widget(Paragraph::new(vec![TLine::from(l1), TLine::from(l2)]).block(block), area);
}

/// First row of the targets panel. The list stays in name order; when it does
/// not fit in `rows`, the window starts at the first unaccepted target (so the
/// view moves on to the remaining work each time a gate result arrives at the
/// end of a round), clamped so the window stays full.
fn targets_window(accepted: &[bool], rows: usize) -> usize {
    if accepted.len() <= rows {
        return 0;
    }
    let first_open = accepted.iter().position(|ok| !ok).unwrap_or(0);
    first_open.min(accepted.len() - rows.max(1))
}

fn side(f: &mut Frame, area: Rect, app: &App, ui: &Ui, p: &Palette) {
    let n_agents = app.agents.len().max(1) as u16;
    let n_targets = app.targets.len().max(1) as u16;
    let n_goals = app.goals.values().filter(|n| **n > 0).count().max(1) as u16;
    let [a_area, t_area, g_area, _rest] = Layout::vertical([
        Constraint::Length((n_agents + 2).min(area.height / 2)),
        Constraint::Length((n_targets + 2).min(area.height / 3)),
        Constraint::Length(n_goals + 2),
        Constraint::Min(0),
    ])
    .areas(area);
    let titled = |t: &str| {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(p.faint))
            .title(Span::styled(format!(" {t} "), Style::default().fg(p.muted).add_modifier(Modifier::BOLD)))
    };
    let w = a_area.width.saturating_sub(4) as usize;
    let mut agents: Vec<TLine> = Vec::new();
    if app.agents.is_empty() {
        agents.push(TLine::from(Span::styled("no agents running", Style::default().fg(p.faint))));
    }
    for a in app.agents.values() {
        let el = seconds_since(&a.started).map(human_secs).unwrap_or_default();
        let goal: String = a.goal.chars().take(w.saturating_sub(a.id.len() + el.len() + 6)).collect();
        agents.push(TLine::from(vec![
            Span::styled(format!("{} ", spinner(ui.tick)), Style::default().fg(backend_color(p, &a.backend))),
            Span::styled(a.id.clone(), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {goal}"), Style::default().fg(p.muted)),
            Span::styled(format!(" {el}"), Style::default().fg(p.faint)),
        ]));
    }
    f.render_widget(Paragraph::new(agents).block(titled(&format!("agents {}", app.agents.len()))), a_area);

    let tw = t_area.width.saturating_sub(4) as usize;
    let mut targets: Vec<TLine> = Vec::new();
    if app.targets.is_empty() {
        targets.push(TLine::from(Span::styled("none until the spec is approved", Style::default().fg(p.faint))));
    }
    let rows = t_area.height.saturating_sub(2) as usize;
    let oks: Vec<bool> = app.targets.values().map(|(ok, _)| *ok).collect();
    let start = targets_window(&oks, rows);
    let below = oks.len().saturating_sub(start + rows);
    for (name, (ok, reasons)) in app.targets.iter().skip(start).take(rows.max(1)) {
        let short = name.strip_prefix("Apc.").unwrap_or(name);
        let mut spans = vec![if *ok {
            Span::styled("✓ ", Style::default().fg(p.good).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("✗ ", Style::default().fg(p.bad))
        }];
        let room = tw.saturating_sub(2);
        let nm: String = short.chars().take(room).collect();
        let used = nm.chars().count() + 2;
        spans.push(Span::raw(nm));
        if !ok && !reasons.is_empty() && used + 2 < tw {
            let r: String = reasons.join(",").chars().take(tw - used - 1).collect();
            spans.push(Span::styled(format!(" {r}"), Style::default().fg(p.faint)));
        }
        targets.push(TLine::from(spans));
    }
    let acc = oks.iter().filter(|o| **o).count();
    let mut t_title = if oks.is_empty() { "targets".to_string() } else { format!("targets {acc}/{}", oks.len()) };
    if start > 0 {
        t_title.push_str(&format!(" ↑{start}"));
    }
    if below > 0 {
        t_title.push_str(&format!(" ↓{below}"));
    }
    f.render_widget(Paragraph::new(targets).block(titled(&t_title)), t_area);

    let mut goals: Vec<TLine> = Vec::new();
    let total: usize = app.goals.values().sum();
    if total == 0 {
        goals.push(TLine::from(Span::styled("no plan yet", Style::default().fg(p.faint))));
    }
    let gw = g_area.width.saturating_sub(16) as usize;
    for (status, color) in [("proved", p.good), ("claimed", p.info), ("open", p.muted), ("blocked", p.warn), ("refuted", p.bad), ("stale", p.faint)] {
        let n = app.goals.get(status).copied().unwrap_or(0);
        if n == 0 {
            continue;
        }
        let bar = ((n * gw) / total.max(1)).max(1);
        goals.push(TLine::from(vec![
            Span::styled(format!("{status:<8}"), Style::default().fg(p.muted)),
            Span::styled(format!("{n:>3} "), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("█".repeat(bar), Style::default().fg(color)),
        ]));
    }
    f.render_widget(Paragraph::new(goals).block(titled("goals")), g_area);
}

fn placeholder(app: &App) -> &'static str {
    match app.phase.as_str() {
        "init" | "spec" => "Describe what to prove, or type / for commands",
        "approved" => "Type /prove to start the team, or / for commands",
        "proving" => "Guide the team, ask with /btw, or type / for commands",
        _ => "Type / for commands",
    }
}

pub fn draw(f: &mut Frame, app: &App, ui: &Ui) {
    let p = palette();
    let area = f.area();
    if area.width < 30 || area.height < 8 {
        f.render_widget(Paragraph::new("vdash: enlarge the terminal"), area);
        return;
    }
    let input_h = 3u16;
    let [head, body, input, foot] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3), Constraint::Length(input_h), Constraint::Length(1)]).areas(area);
    header(f, head, app, ui, &p);

    let has_panel_content = !app.targets.is_empty() || !app.agents.is_empty() || app.engine.is_some();
    let (main, panel) = if body.width >= 100 && has_panel_content {
        let [m, s] = Layout::horizontal([Constraint::Min(60), Constraint::Length(38)]).areas(body);
        (m, Some(s))
    } else {
        (body, None)
    };
    // Transcript, bottom-anchored, scrollable.
    let width = main.width.saturating_sub(1) as usize;
    let lines = if app.transcript.is_empty() { welcome(app, &p, width) } else { transcript_lines(app, &p, width) };
    let h = main.height as usize;
    let max_scroll = lines.len().saturating_sub(h);
    let scroll = ui.scroll.min(max_scroll);
    let start = lines.len().saturating_sub(h + scroll);
    let end = (start + h).min(lines.len());
    let visible: Vec<TLine> = lines[start..end].to_vec();
    f.render_widget(Paragraph::new(visible), main);
    if scroll > 0 {
        let tag = format!(" ↑ {scroll} more below ");
        let r = Rect { x: main.x + main.width.saturating_sub(tag.chars().count() as u16 + 1), y: main.y + main.height - 1, width: tag.chars().count() as u16, height: 1 };
        f.render_widget(Paragraph::new(Span::styled(tag, Style::default().fg(Color::Black).bg(p.muted))), r);
    }
    if let Some(s) = panel {
        side(f, s, app, ui, &p);
    }

    // Input.
    let busy = if ui.busy.is_empty() { String::new() } else { format!(" {} {} ", spinner(ui.tick), ui.busy.join(", ")) };
    let title = if ui.confirm_quit {
        Span::styled(" press ctrl-c again to quit (the engine keeps running) ", Style::default().fg(p.warn))
    } else {
        Span::styled(busy, Style::default().fg(p.accent))
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if ui.input.starts_with('/') { p.accent } else { p.faint }))
        .title(title);
    let inner_w = input.width.saturating_sub(4) as usize;
    let chars: Vec<char> = ui.input.chars().collect();
    let cur = ui.cursor.min(chars.len());
    let offset = cur.saturating_sub(inner_w.saturating_sub(1));
    let shown: String = chars[offset..].iter().take(inner_w).collect();
    let content = if ui.input.is_empty() {
        TLine::from(vec![Span::styled("› ", Style::default().fg(p.accent)), Span::styled(placeholder(app), Style::default().fg(p.faint))])
    } else {
        let mut spans = vec![Span::styled("› ", Style::default().fg(p.accent))];
        if let Some(rest) = shown.strip_prefix('/') {
            let (name, args) = rest.split_once(' ').map(|(a, b)| (a, Some(b))).unwrap_or((rest, None));
            spans.push(Span::styled(format!("/{name}"), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)));
            match args {
                Some(a) => spans.push(Span::raw(format!(" {a}"))),
                None => {
                    let hint = commands::args_hint(name);
                    if !hint.is_empty() && offset == 0 {
                        spans.push(Span::styled(format!(" {hint}"), Style::default().fg(p.faint)));
                    }
                }
            }
        } else {
            spans.push(Span::raw(shown.clone()));
        }
        TLine::from(spans)
    };
    f.render_widget(Paragraph::new(content).block(block), input);
    f.set_cursor_position(Position { x: input.x + 3 + (cur - offset) as u16, y: input.y + 1 });

    // Completion popup above the input.
    if !ui.completions.is_empty() {
        let n = ui.completions.len().min(8) as u16;
        let w = main.width.min(70);
        let r = Rect { x: input.x, y: input.y.saturating_sub(n + 2), width: w, height: n + 2 };
        let first = ui.completion_idx.saturating_sub(7);
        let rows: Vec<TLine> = ui
            .completions
            .iter()
            .enumerate()
            .skip(first)
            .take(8)
            .map(|(i, c)| {
                let sel = i == ui.completion_idx;
                let st = if sel { Style::default().fg(Color::Black).bg(p.accent) } else { Style::default() };
                let desc = commands::describe(c);
                TLine::from(vec![
                    Span::styled(format!(" {c:<18}"), st.add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {desc} "), if sel { st } else { Style::default().fg(p.muted) }),
                ])
            })
            .collect();
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(rows).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::default().fg(p.accent))),
            r,
        );
    }

    let keys = [("enter", "send"), ("tab", "complete"), ("↑↓", "history"), ("pgup/pgdn", "scroll"), ("ctrl-c", "quit")];
    let mut spans = vec![Span::raw(" ")];
    for (k, v) in keys {
        spans.push(Span::styled(k, Style::default().fg(p.muted).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(" {v}   "), Style::default().fg(p.faint)));
    }
    f.render_widget(Paragraph::new(TLine::from(spans)), foot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn fixture() -> App {
        let mut a = App::new("isort-demo");
        a.phase = "proving".into();
        for (k, d) in [
            ("engine_started", json!({"pid": 4242, "mode": "team"})),
            ("stage", json!({"stage": "leads"})),
            ("agent_started", json!({"agent": "worker-3", "role": "worker", "goal": "t1"})),
            ("agent_backend", json!({"agent": "worker-3", "backend": "gemini"})),
            ("gate", json!({"accepted": 3, "total": 4, "targets": {"Apc.isort_correct": {"accepted": true, "reasons": []}, "Apc.isort_fixes_sorted": {"accepted": false, "reasons": ["bad_axioms"]}, "Apc.sorted_nonvacuous": {"accepted": true, "reasons": []}, "Apc.isortBad_incorrect": {"accepted": true, "reasons": []}}})),
            ("lead_summary", json!({"lead": "L1", "summary": "isort_fixes_sorted still uses sorry in the cons case; a helper lemma about insert on sorted lists should close it."})),
            ("violation", json!({"path": "lean/Apc/Spec/Defs.lean", "change": "modified"})),
        ] {
            a.apply(&Event::new(2, k, d));
        }
        a.goals.insert("open".into(), 2);
        a.goals.insert("proved".into(), 3);
        a
    }

    fn render(w: u16, h: u16, app: &App, ui: &Ui) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app, ui)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn wide_layout_shows_header_panel_and_transcript() {
        let app = fixture();
        let s = render(120, 30, &app, &Ui::default());
        assert!(s.contains("◆ vdash"), "{s}");
        assert!(s.contains("PROVING"));
        assert!(s.contains("3/4"));
        assert!(s.contains("round 2 · leads"));
        assert!(s.contains("agents 1"));
        assert!(s.contains("worker-3"));
        assert!(s.contains("✗ isort_fixes_sorted"));
        assert!(s.contains("reverted an out-of-scope edit"));
        assert!(s.contains("Guide the team"));
        assert!(s.contains("gemini 1 running") || s.contains("gemini"));
    }

    #[test]
    fn narrow_layout_drops_the_panel_and_tiny_does_not_panic() {
        let app = fixture();
        let s = render(80, 24, &app, &Ui::default());
        assert!(!s.contains("agents 1"));
        assert!(s.contains("L1"));
        let _ = render(40, 10, &app, &Ui::default());
        let _ = render(20, 5, &app, &Ui::default());
    }

    #[test]
    fn welcome_and_completion_popup() {
        let app = App::new("fresh");
        let ui = Ui { input: "/st".into(), cursor: 3, completions: vec!["/status".into(), "/stop".into()], ..Default::default() };
        let s = render(100, 24, &app, &ui);
        assert!(s.contains("AlphaProof Code, rebuilt"));
        assert!(!s.contains("agents 0"), "no side panel on a fresh campaign");
        assert!(s.contains("/status"));
        assert!(s.contains("campaign summary"));
    }

    /// `VDASH_PRINT_SCREENS=1 cargo test print_screens -- --nocapture` prints the
    /// fixture screens, for eyeballing the layout.
    #[test]
    fn print_screens() {
        if std::env::var_os("VDASH_PRINT_SCREENS").is_none() {
            return;
        }
        let app = fixture();
        println!("{}", render(120, 30, &app, &Ui::default()));
        println!("{}", render(80, 24, &app, &Ui { input: "/model wo".into(), cursor: 9, completions: vec!["/model worker".into()], ..Default::default() }));
        println!("{}", render(100, 20, &App::new("fresh"), &Ui::default()));
    }

    #[test]
    fn targets_window_follows_the_first_open_target() {
        assert_eq!(targets_window(&[true, false, true], 5), 0, "fits: no scroll");
        let mut v = vec![true; 30];
        v[20] = false;
        v[25] = false;
        assert_eq!(targets_window(&v, 8), 20);
        v[20] = true;
        v[25] = true;
        v[28] = false;
        assert_eq!(targets_window(&v, 8), 22, "clamped so the window stays full");
        assert_eq!(targets_window(&[true; 30], 8), 0, "all accepted: back to the top");
        assert_eq!(targets_window(&v, 0), 28, "zero rows: no panic");
    }

    #[test]
    fn targets_panel_scrolls_to_remaining_targets() {
        let mut app = fixture();
        app.targets.clear();
        for i in 0..40 {
            app.targets.insert(format!("Apc.t{i:02}"), (i < 30, if i < 30 { vec![] } else { vec!["sorry".into()] }));
        }
        let s = render(120, 40, &app, &Ui::default());
        assert!(s.contains("✗ t30"), "{s}");
        assert!(!s.contains("✓ t00"), "{s}");
        assert!(s.contains("targets 30/40 ↑"), "{s}");
    }

    #[test]
    fn wrap_breaks_on_words_and_long_tokens() {
        assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("x\ny", 10), vec!["x", "y"]);
        assert_eq!(wrap(&"z".repeat(20), 8).len(), 3);
    }
}
