//! Prompt assembly with per-section budgets (ROADMAP M2-07).
//!
//! A prompt is a sequence of `<name>…</name>` blocks. Budgets keep any one section
//! from flooding an agent's context.

/// (character budget, keep the tail rather than the head)
pub fn budget(section: &str) -> (usize, bool) {
    match section {
        "bulletin" => (2000, true),
        "lead_notes" => (6000, true),
        "summary" | "lead_summaries" => (1500 * 4, false),
        "diagnostics" => (4000, false),
        "file" | "spec" | "spec_files" | "challenge" => (30000, false),
        "guidance" => (3000, true),
        "events" => (6000, true),
        "import_graph" => (8000, false),
        _ => (12000, false),
    }
}

fn clip_lines(s: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        return s.to_string();
    }
    let mut out = lines[..max_lines].join("\n");
    out.push_str(&format!("\n… [{} more lines]", lines.len() - max_lines));
    out
}

/// Render `sections` in the given order, applying each section's budget.
pub fn build(task: &str, sections: &[(&str, String)]) -> String {
    let mut out = String::new();
    out.push_str("<task>\n");
    out.push_str(task.trim());
    out.push_str("\n</task>\n");
    for (name, body) in sections {
        if body.trim().is_empty() {
            continue;
        }
        let body = if *name == "goals" {
            clip_lines(body, 60)
        } else {
            body.clone()
        };
        let (max, tail) = budget(name);
        let body = crate::util::clip(&body, max, tail);
        out.push_str(&format!("\n<{name}>\n{}\n</{name}>\n", body.trim_end()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_is_stable_and_empty_sections_skipped() {
        let p = build(
            "do it",
            &[("goal", "g1".into()), ("empty", "  ".into()), ("file", "x".into())],
        );
        let g = p.find("<goal>").unwrap();
        let f = p.find("<file>").unwrap();
        assert!(g < f);
        assert!(!p.contains("<empty>"));
        assert!(p.starts_with("<task>\ndo it\n</task>"));
    }

    #[test]
    fn budgets_enforced_tail_kept_for_bulletin() {
        let bulletin: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let p = build("t", &[("bulletin", bulletin)]);
        assert!(p.contains("line 999"));
        assert!(!p.contains("line 1\n"));
        assert!(p.contains("characters omitted"));
        let goals: String = (0..100).map(|i| format!("g{i}\n")).collect();
        let p = build("t", &[("goals", goals)]);
        assert!(p.contains("40 more lines"));
    }

    #[test]
    fn multibyte_cut_is_safe() {
        let s = "é".repeat(5000);
        let p = build("t", &[("diagnostics", s)]);
        assert!(p.contains("characters omitted"));
    }
}
