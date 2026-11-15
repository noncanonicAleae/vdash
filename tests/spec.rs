use std::fs;
use std::sync::Arc;

use vdash::backend::fake::FakeBackend;
use vdash::backend::pool::Pool;
use vdash::config::{self, BackendKind, Role};
use vdash::events::EngineLock;
use vdash::paths::{init_campaign, Ctx};
use vdash::spec;
use vdash::util;
use serde_json::json;
use tempfile::TempDir;

fn setup_campaign(tmp: &TempDir, backends: &[BackendKind]) -> Ctx {
    let host = tmp.path().join("host");
    fs::create_dir_all(&host).unwrap();
    fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
    let camp = tmp.path().join("camp");
    init_campaign(&camp, &host, None, backends).unwrap();
    Ctx::for_test(&camp, tmp.path().join("state")).unwrap()
}

fn writer_reply(notes: &str) -> serde_json::Value {
    json!({
        "files_written": ["lean/Apc/Spec/Defs.lean", "lean/Apc/Challenge.lean", "spec/DEPARTURES.md"],
        "targets": [{"name": "Apc.foo_ok", "kind": "property"}],
        "departures_added": 1,
        "notes": notes
    })
}

fn reviewer_ok_reply(lens: &str) -> serde_json::Value {
    json!({
        "lens": lens,
        "findings": [],
        "verdict": "ok"
    })
}

#[test]
fn m5_02_spec_draft_revise_policy_review_file_and_warnings() {
    let tmp = TempDir::new().unwrap();
    // 1. Both backends available -> spec_writer = claude, reviewer = gemini (cross-family)
    let ctx = setup_campaign(&tmp, &[BackendKind::Claude, BackendKind::Gemini]);

    let fb_claude = Arc::new(FakeBackend::new(BackendKind::Claude));
    let fb_gemini = Arc::new(FakeBackend::new(BackendKind::Gemini));
    let pool = Pool::new(&ctx, vec![fb_claude.clone(), fb_gemini.clone()]);

    // Missing BRIEF.md -> exit code 2
    let err = spec::draft(&ctx, &pool).unwrap_err();
    assert_eq!(err.code, 2);
    assert!(err.message.contains("BRIEF.md"));

    // Write BRIEF.md
    let brief_path = ctx.campaign.sources_dir().join("BRIEF.md");
    fs::write(&brief_path, "Verify binary search on sorted lists.\n").unwrap();

    // Engine lock held -> exit code 2
    {
        let _lock = EngineLock::acquire(&ctx).unwrap();
        let err = spec::draft(&ctx, &pool).unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("engine is currently running"));
    }

    // Script spec_writer to write allowed files AND an out-of-scope file
    let root = ctx.campaign.root.clone();
    fb_claude.func(Role::SpecWriter, move |_call| {
        fs::write(
            root.join("lean/Apc/Spec/Defs.lean"),
            "namespace Apc.Spec\ndef Foo : Nat := 1\nend Apc.Spec\n",
        )
        .unwrap();
        fs::write(
            root.join("lean/Apc/Challenge.lean"),
            "import Apc.Spec.Defs\nnamespace Apc\ntheorem foo_ok : Spec.Foo = 1 := by sorry\nend Apc\n",
        )
        .unwrap();
        fs::write(root.join("spec/DEPARTURES.md"), "- None.\n").unwrap();
        // Out-of-scope edits that policy::enforce must revert:
        fs::write(root.join("sources/BRIEF.md"), "TAMPERED BRIEF\n").unwrap();
        fs::write(root.join("lean/Apc/Solution.lean"), "-- unauthorized\n").unwrap();
        writer_reply("Drafted initial Foo specification.")
    });

    // Script all 4 parallel reviewers for `draft`: `strength` returns unsorted findings
    // (minor, blocker, major), the other 3 lenses return ok.
    for _ in 0..4 {
        fb_gemini.func(Role::Reviewer, |call| {
            let lens = if call.prompt.contains("through the 'soundness' lens") {
                "soundness"
            } else if call.prompt.contains("through the 'strength' lens") {
                "strength"
            } else if call.prompt.contains("through the 'fidelity' lens") {
                "fidelity"
            } else {
                "legibility"
            };
            if lens == "strength" {
                json!({
                    "lens": "strength",
                    "findings": [
                        {
                            "severity": "minor",
                            "where": "lean/Apc/Spec/Defs.lean:Foo",
                            "issue": "Doc comment missing",
                            "suggestion": "Add /-- ... -/"
                        },
                        {
                            "severity": "blocker",
                            "where": "lean/Apc/Challenge.lean:foo_ok",
                            "issue": "Missing mutation target",
                            "suggestion": "Add Spec/Mutants.lean and mutation target"
                        },
                        {
                            "severity": "major",
                            "where": "lean/Apc/Challenge.lean:foo_ok",
                            "issue": "No non-vacuity witness",
                            "suggestion": "Add non-vacuity theorem"
                        }
                    ],
                    "verdict": "revise"
                })
            } else {
                reviewer_ok_reply(lens)
            }
        });
    }
    fb_gemini.always(Role::Reviewer, reviewer_ok_reply("soundness"));

    let report1 = spec::draft(&ctx, &pool).unwrap();
    assert_eq!(report1.review_num, 1);
    assert_eq!(report1.blockers, 1);
    assert_eq!(report1.majors, 1);
    assert_eq!(report1.minors, 1);
    // Cross-family is active (claude + gemini), so no cross-family warning:
    assert!(
        !report1
            .warnings
            .iter()
            .any(|w| w.contains("no cross-family review")),
        "unexpected cross-family warning: {:?}",
        report1.warnings
    );
    // Out-of-scope edits were reverted, allowed edits were kept:
    assert_eq!(
        fs::read_to_string(&brief_path).unwrap(),
        "Verify binary search on sorted lists.\n"
    );
    assert!(!ctx.campaign.solution_file().exists());
    assert!(ctx.campaign.root.join("lean/Apc/Spec/Defs.lean").is_file());
    assert!(ctx.campaign.challenge_file().is_file());
    assert!(ctx.campaign.departures_file().is_file());

    // Phase moved to "spec"
    let st = util::load_state(&ctx.campaign).unwrap();
    assert_eq!(st.phase, "spec");

    // Check review-1.md contents and severity ordering (blocker before major before minor)
    let rev1_text = fs::read_to_string(&report1.review_path).unwrap();
    assert!(rev1_text.contains("# Spec Review 1"));
    assert!(rev1_text.contains("## Build Status"));
    let blocker_pos = rev1_text.find("**[blocker]**").unwrap();
    let major_pos = rev1_text.find("**[major]**").unwrap();
    let minor_pos = rev1_text.find("**[minor]**").unwrap();
    assert!(blocker_pos < major_pos && major_pos < minor_pos);
    // Because there is 1 blocker, summary does not have the approve hint:
    assert!(!report1.summary.contains("vdash approve"));

    // Now test `revise`: passes `<user_feedback>` and `<latest_review>`, and when 0 blockers, prints approve hint
    fb_claude.reply(Role::SpecWriter, writer_reply("Added mutation target."));
    fb_gemini.func(Role::Reviewer, |call| {
        for lens in ["soundness", "strength", "fidelity", "legibility"] {
            if call.prompt.contains(&format!("'{lens}' lens")) {
                return reviewer_ok_reply(lens);
            }
        }
        reviewer_ok_reply("soundness")
    });
    fb_gemini.func(Role::Reviewer, |call| {
        for lens in ["soundness", "strength", "fidelity", "legibility"] {
            if call.prompt.contains(&format!("'{lens}' lens")) {
                return reviewer_ok_reply(lens);
            }
        }
        reviewer_ok_reply("soundness")
    });
    fb_gemini.func(Role::Reviewer, |call| {
        for lens in ["soundness", "strength", "fidelity", "legibility"] {
            if call.prompt.contains(&format!("'{lens}' lens")) {
                return reviewer_ok_reply(lens);
            }
        }
        reviewer_ok_reply("soundness")
    });
    fb_gemini.func(Role::Reviewer, |call| {
        for lens in ["soundness", "strength", "fidelity", "legibility"] {
            if call.prompt.contains(&format!("'{lens}' lens")) {
                return reviewer_ok_reply(lens);
            }
        }
        reviewer_ok_reply("soundness")
    });

    let report2 = spec::revise(&ctx, &pool, "Please add the mutation target.").unwrap();
    assert_eq!(report2.review_num, 2);
    assert_eq!(report2.blockers, 0);
    assert!(report2.summary.contains("vdash approve"));

    let claude_calls = fb_claude.calls.lock().unwrap();
    assert_eq!(claude_calls.len(), 2);
    assert!(claude_calls[1].prompt.contains("<user_feedback>"));
    assert!(claude_calls[1]
        .prompt
        .contains("Please add the mutation target."));
    assert!(claude_calls[1].prompt.contains("<latest_review>"));
    assert!(claude_calls[1].prompt.contains("# Spec Review 1"));
    drop(claude_calls);

    // Test cross-family warning when reviewer is set to claude as well
    config::set_role(
        &ctx.campaign.root,
        Role::Reviewer,
        &[config::RoleEntry {
            backend: BackendKind::Claude,
            model: None,
        }],
    )
    .unwrap();
    let mut ctx_same = ctx.clone();
    ctx_same.cfg = config::load(&ctx.campaign.root).unwrap();
    fb_claude.always(Role::SpecWriter, writer_reply("ok"));
    fb_claude.always(Role::Reviewer, reviewer_ok_reply("soundness"));
    let report3 = spec::revise(&ctx_same, &pool, "check warning").unwrap();
    assert!(
        report3
            .warnings
            .iter()
            .any(|w| w.contains("no cross-family review")),
        "expected cross-family warning, got: {:?}",
        report3.warnings
    );
    assert!(report3.summary.contains("no cross-family review"));

    // Test refused after approval
    util::update_state(&ctx.campaign, |s| {
        s.phase = "approved".to_string();
    })
    .unwrap();
    let err = spec::draft(&ctx, &pool).unwrap_err();
    assert_eq!(err.code, 2);
    let err2 = spec::revise(&ctx, &pool, "more").unwrap_err();
    assert_eq!(err2.code, 2);
}

#[test]
fn m5_03_converse_drafts_first_then_revises() {
    let tmp = TempDir::new().unwrap();
    let ctx = setup_campaign(&tmp, &[BackendKind::Claude]);
    let fb = Arc::new(FakeBackend::new(BackendKind::Claude));
    fb.always(Role::SpecWriter, writer_reply("notes from writer"));
    fb.always(Role::Reviewer, reviewer_ok_reply("soundness"));
    let pool = Pool::new(&ctx, vec![fb.clone()]);

    // First message in `init` phase creates sources/BRIEF.md and runs `draft`
    let r1 = spec::converse(&ctx, &pool, "Binary search on a sorted list.").unwrap();
    assert_eq!(r1.review_num, 1);
    assert_eq!(
        fs::read_to_string(ctx.campaign.sources_dir().join("BRIEF.md")).unwrap(),
        "Binary search on a sorted list.\n"
    );
    assert_eq!(util::load_state(&ctx.campaign).unwrap().phase, "spec");

    // Second message in `spec` phase runs `revise`
    let r2 = spec::converse(&ctx, &pool, "Also specify termination.").unwrap();
    assert_eq!(r2.review_num, 2);
    let calls = fb.calls.lock().unwrap();
    let sw_calls: Vec<_> = calls
        .iter()
        .filter(|c| c.role == Role::SpecWriter)
        .collect();
    assert_eq!(sw_calls.len(), 2);
    assert!(!sw_calls[0].prompt.contains("<user_feedback>"));
    assert!(sw_calls[1].prompt.contains("<user_feedback>"));
    assert!(sw_calls[1].prompt.contains("Also specify termination."));
}

#[test]
fn failed_reviewers_make_the_review_incomplete_not_clean() {
    let tmp = TempDir::new().unwrap();
    let ctx = setup_campaign(&tmp, &[BackendKind::Claude, BackendKind::Gemini]);
    fs::write(ctx.campaign.sources_dir().join("BRIEF.md"), "Anything.\n").unwrap();
    let fb_claude = Arc::new(FakeBackend::new(BackendKind::Claude));
    let fb_gemini = Arc::new(FakeBackend::new(BackendKind::Gemini));
    fb_claude.reply(Role::SpecWriter, writer_reply("draft"));
    // Two lenses fail (a cut-off turn), two answer.
    for _ in 0..4 {
        fb_gemini.fail(Role::Reviewer, vdash::backend::Failure::AgentError("stream interrupted".into()));
    }
    fb_gemini.always(Role::Reviewer, reviewer_ok_reply("soundness"));
    let pool = Pool::new(&ctx, vec![fb_claude, fb_gemini]);
    let report = spec::draft(&ctx, &pool).unwrap();
    assert!(report.summary.contains("reviewers failed"), "{}", report.summary);
    assert!(report.summary.contains("incomplete"));
    assert!(!report.summary.contains("No blockers reported"), "{}", report.summary);
}

/// A spec-writer that writes a small valid spec each time it is called, and runs
/// `hook(n)` on its n-th call (1-based).
fn scripted_writer(fb: &FakeBackend, root: std::path::PathBuf, hook: impl Fn(u32) + Send + Sync + 'static) {
    let hook = Arc::new(hook);
    let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
    for _ in 0..20 {
        let (root, hook, n) = (root.clone(), hook.clone(), n.clone());
        fb.func(Role::SpecWriter, move |_| {
            fs::write(root.join("lean/Apc/Spec/Defs.lean"), "namespace Apc.Spec\ndef Foo : Nat := 1\nend Apc.Spec\n").unwrap();
            fs::write(
                root.join("lean/Apc/Challenge.lean"),
                "import Apc.Spec.Defs\nnamespace Apc\ntheorem foo_ok : Spec.Foo = 1 := by sorry\nend Apc\n",
            )
            .unwrap();
            fs::write(root.join("spec/DEPARTURES.md"), "- None.\n").unwrap();
            let k = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            hook(k);
            writer_reply("wrote the spec")
        });
    }
}

fn lens_of(call: &vdash::backend::AgentCall) -> &'static str {
    spec::LENSES.into_iter().find(|l| call.prompt.contains(&format!("'{l}' lens"))).unwrap_or("soundness")
}

fn reviewer_revise_reply(lens: &str) -> serde_json::Value {
    json!({
        "lens": lens,
        "findings": [{"severity": "major", "where": "lean/Apc/Challenge.lean:foo_ok", "issue": "weak", "suggestion": "strengthen"}],
        "verdict": "revise"
    })
}

#[test]
fn spec_loop_revises_until_every_reviewer_says_ok() {
    let tmp = TempDir::new().unwrap();
    let ctx = setup_campaign(&tmp, &[BackendKind::Claude, BackendKind::Gemini]);
    fs::write(ctx.campaign.sources_dir().join("BRIEF.md"), "Foo is one.\n").unwrap();
    let fb_claude = Arc::new(FakeBackend::new(BackendKind::Claude));
    let fb_gemini = Arc::new(FakeBackend::new(BackendKind::Gemini));
    scripted_writer(&fb_claude, ctx.campaign.root.clone(), |_| {});
    // Round 1: strength asks for changes; afterwards every lens is satisfied.
    for _ in 0..4 {
        fb_gemini.func(Role::Reviewer, |c| {
            let l = lens_of(c);
            if l == "strength" { reviewer_revise_reply(l) } else { reviewer_ok_reply(l) }
        });
    }
    for _ in 0..8 {
        fb_gemini.func(Role::Reviewer, |c| reviewer_ok_reply(lens_of(c)));
    }
    let pool = Pool::new(&ctx, vec![fb_claude.clone(), fb_gemini]);
    let (report, end) = spec::iterate(&ctx, &pool, spec::FirstStep::Draft).unwrap();
    assert_eq!(end, spec::LoopEnd::Converged, "{}", report.summary);
    assert_eq!(report.review_num, 2);
    let calls = fb_claude.calls.lock().unwrap();
    let sw: Vec<_> = calls.iter().filter(|c| c.role == Role::SpecWriter).collect();
    assert_eq!(sw.len(), 2);
    assert!(sw[1].prompt.contains("Address the reviewers' findings in spec/review-1.md"));
    assert!(sw[1].prompt.contains("# Spec Review 1"));
    drop(calls);
    // The loop released the engine lock.
    assert!(vdash::events::engine_running(&ctx).is_none());
}

#[test]
fn spec_loop_takes_guidance_between_rounds_and_stops_when_told() {
    let tmp = TempDir::new().unwrap();
    let ctx = setup_campaign(&tmp, &[BackendKind::Claude, BackendKind::Gemini]);
    fs::write(ctx.campaign.sources_dir().join("BRIEF.md"), "Foo is one.\n").unwrap();
    let fb_claude = Arc::new(FakeBackend::new(BackendKind::Claude));
    let fb_gemini = Arc::new(FakeBackend::new(BackendKind::Gemini));
    let c2 = ctx.clone();
    scripted_writer(&fb_claude, ctx.campaign.root.clone(), move |n| {
        use vdash::events::{send, Control};
        // While round 1 runs the user adds guidance; during round 2 they say it is good enough.
        let msg = if n == 1 { Control::Guidance { text: "Use Nat, not Int.".into() } } else { Control::Stop };
        send(&c2, &msg).unwrap();
    });
    for _ in 0..40 {
        fb_gemini.func(Role::Reviewer, |c| reviewer_revise_reply(lens_of(c)));
    }
    let pool = Pool::new(&ctx, vec![fb_claude.clone(), fb_gemini]);
    let (report, end) = spec::iterate(&ctx, &pool, spec::FirstStep::Draft).unwrap();
    assert_eq!(end, spec::LoopEnd::Stopped, "{}", report.summary);
    let calls = fb_claude.calls.lock().unwrap();
    let sw: Vec<_> = calls.iter().filter(|c| c.role == Role::SpecWriter).collect();
    assert_eq!(sw.len(), 2);
    assert!(sw[1].prompt.contains("Use Nat, not Int."));
}

#[test]
fn spec_loop_stops_at_its_round_budget() {
    let tmp = TempDir::new().unwrap();
    let mut ctx = setup_campaign(&tmp, &[BackendKind::Claude, BackendKind::Gemini]);
    ctx.cfg.budget.spec_max_rounds = 2;
    fs::write(ctx.campaign.sources_dir().join("BRIEF.md"), "Foo is one.\n").unwrap();
    let fb_claude = Arc::new(FakeBackend::new(BackendKind::Claude));
    let fb_gemini = Arc::new(FakeBackend::new(BackendKind::Gemini));
    scripted_writer(&fb_claude, ctx.campaign.root.clone(), |_| {});
    for _ in 0..40 {
        fb_gemini.func(Role::Reviewer, |c| reviewer_revise_reply(lens_of(c)));
    }
    let pool = Pool::new(&ctx, vec![fb_claude.clone(), fb_gemini]);
    let (report, end) = spec::iterate(&ctx, &pool, spec::FirstStep::Draft).unwrap();
    assert_eq!(end, spec::LoopEnd::Budget);
    assert!(report.summary.contains("spec_max_rounds"));
    assert_eq!(fb_claude.calls.lock().unwrap().iter().filter(|c| c.role == Role::SpecWriter).count(), 2);
}
