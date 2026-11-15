//! `/btw` and `vdash btw` (ROADMAP M4-06): a read-only side question to the
//! monitor agent. It runs in the asking process and takes no engine lock, so a
//! running campaign is undisturbed.

use crate::agents::{self, Outcome, Request};
use crate::backend::pool::Pool;
use crate::config::Role;
use crate::error::Result;
use crate::events;
use crate::paths::Ctx;
use crate::schemas::Reply;

pub fn ask(ctx: &Ctx, pool: &Pool, question: &str) -> Result<String> {
    let status = crate::status::render(ctx).unwrap_or_default();
    let (evs, _) = events::tail(&ctx.campaign.events_file(), 40);
    let recent: String = evs
        .iter()
        .map(|e| {
            let data = serde_json::Value::Object(e.data.clone()).to_string();
            format!("{} r{} {} {}", e.ts, e.round, e.kind, crate::util::clip(&data, 300, false))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let round = crate::util::load_state(&ctx.campaign).map(|s| s.round).unwrap_or(0);
    let req = Request::new(Role::Monitor, Reply::Monitor, format!("Answer the user's question about this campaign: {question}"))
        .section("status", status)
        .section("events", recent)
        .section("question", question.to_string())
        .label("btw");
    let (_, out) = agents::run(ctx, pool, round, req)?;
    Ok(match out {
        Outcome::Ok(v) => {
            let mut s = v["answer"].as_str().unwrap_or("").trim().to_string();
            s.push('\n');
            let sugg = v["skill_suggestions"].as_array().cloned().unwrap_or_default();
            if !sugg.is_empty() {
                s.push_str("\nSuggested skill changes (apply them yourself in skills/):\n");
                for x in sugg {
                    s.push_str(&format!("- {}: {}\n", x["skill"].as_str().unwrap_or("?"), x["change"].as_str().unwrap_or("")));
                }
            }
            s
        }
        Outcome::Failed(f) => format!("The monitor could not answer: {f}\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;
    use crate::config::BackendKind;
    use crate::paths;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn prompt_has_status_and_question_and_lock_is_untouched() {
        let tmp = tempfile::TempDir::new().unwrap();
        let host = tmp.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
        let camp = tmp.path().join("c");
        paths::init_campaign(&camp, &host, None, &[BackendKind::Claude]).unwrap();
        let ctx = Ctx::for_test(&camp, tmp.path().join("state")).unwrap();
        let _engine = events::EngineLock::acquire(&ctx).unwrap(); // an engine "runs"
        let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
        fb.reply(Role::Monitor, json!({"answer": "L1 waits on a lemma.", "skill_suggestions": [{"skill": "team_lead", "change": "pin lemma statements earlier"}]}));
        let pool = Pool::new(&ctx, vec![fb.clone()]);
        let out = ask(&ctx, &pool, "what is L1 doing?").unwrap();
        assert!(out.starts_with("L1 waits on a lemma."));
        assert!(out.contains("team_lead: pin lemma statements earlier"));
        let calls = fb.calls.lock().unwrap();
        assert!(calls[0].prompt.contains("<question>\nwhat is L1 doing?"));
        assert!(calls[0].prompt.contains("<status>"));
    }
}
