//! Role skills (ROADMAP M2-06): embedded Markdown, overridable per campaign.

use std::fs;

use crate::config::Role;
use crate::error::{VdashError, Result};
use crate::paths::{Campaign, Ctx};

pub const SECTIONS: [&str; 6] = [
    "Role",
    "You may",
    "You must not",
    "Procedure",
    "Checks before replying",
    "Reply",
];

pub fn embedded(role: Role) -> &'static str {
    match role {
        Role::Worker => include_str!("../skills/worker.md"),
        Role::Falsifier => include_str!("../skills/falsifier.md"),
        Role::TeamLead => include_str!("../skills/team_lead.md"),
        Role::Orchestrator => include_str!("../skills/orchestrator.md"),
        Role::Architect => include_str!("../skills/architect.md"),
        Role::Integrator => include_str!("../skills/integrator.md"),
        Role::SpecWriter => include_str!("../skills/spec_writer.md"),
        Role::Reviewer => include_str!("../skills/reviewer.md"),
        Role::Critic => include_str!("../skills/critic.md"),
        Role::Monitor => include_str!("../skills/monitor.md"),
    }
}

/// The campaign's override in `<campaign>/skills/<role>.md`, else the embedded text.
pub fn load(c: &Campaign, role: Role) -> Result<String> {
    let p = c.skills_dir().join(format!("{role}.md"));
    if p.is_file() {
        return fs::read_to_string(&p).map_err(|e| VdashError::io_path(&p, e));
    }
    Ok(embedded(role).to_string())
}

/// Load a role skill with `{{package}}` substituted for the campaign's Lean package name.
pub fn load_for(ctx: &Ctx, role: Role) -> Result<String> {
    let text = load(&ctx.campaign, role)?;
    Ok(text.replace("{{package}}", ctx.pkg()))
}

/// H2 section titles in order of appearance.
pub fn section_titles(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("## "))
        .map(|s| s.trim().to_string())
        .collect()
}

/// Body of one H2 section.
pub fn section<'a>(text: &'a str, title: &str) -> Option<&'a str> {
    let head = format!("## {title}\n");
    let start = text.find(&head)? + head.len();
    let rest = &text[start..];
    let end = rest.find("\n## ").map(|i| i + 1).unwrap_or(rest.len());
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::{required_keys, Reply};

    fn replies_for(role: Role) -> Vec<Reply> {
        match role {
            Role::Worker => vec![Reply::Worker],
            Role::Falsifier => vec![Reply::Falsifier],
            Role::TeamLead => vec![Reply::LeadPlan, Reply::LeadSummary],
            Role::Orchestrator => vec![Reply::Orchestrator],
            Role::Architect => vec![Reply::Architect],
            Role::Integrator => vec![Reply::Integrator],
            Role::SpecWriter => vec![Reply::SpecWriter],
            Role::Reviewer => vec![Reply::Reviewer],
            Role::Critic => vec![Reply::Critic],
            Role::Monitor => vec![Reply::Monitor],
        }
    }

    #[test]
    fn every_skill_has_sections_and_names_its_reply_keys() {
        for role in Role::ALL {
            let text = embedded(role);
            assert_eq!(section_titles(text), SECTIONS.to_vec(), "{role}");
            let reply = section(text, "Reply").unwrap();
            for r in replies_for(role) {
                for k in required_keys(r) {
                    assert!(reply.contains(&format!("`{k}`")), "{role}: Reply section lacks `{k}`");
                }
            }
        }
    }

    #[test]
    fn campaign_override_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let c = Campaign::new(tmp.path());
        fs::create_dir_all(c.skills_dir()).unwrap();
        fs::write(c.skills_dir().join("worker.md"), "custom").unwrap();
        assert_eq!(load(&c, Role::Worker).unwrap(), "custom");
        assert!(load(&c, Role::Critic).unwrap().contains("# Critic"));
    }
}
