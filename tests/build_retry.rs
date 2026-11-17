//! M3-09: a build that fails only with an infrastructure error is rerun. Own test
//! binary: it puts a fake `lean` first on PATH for the whole process.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

use vdash::config::BackendKind;
use vdash::lean;
use vdash::paths::{self, Ctx};

#[test]
fn infra_errors_are_retried_and_real_errors_are_not() {
    require_lean!();
    let real = String::from_utf8(std::process::Command::new("sh").args(["-c", "command -v lean"]).output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    assert!(!real.is_empty());

    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let mark = tmp.path().join("failed-once");
    let script = format!(
        "#!/bin/sh\nif [ ! -e '{m}' ]; then touch '{m}'; echo '{{\"severity\":\"error\",\"pos\":{{\"line\":1,\"column\":0}},\"fileName\":\"\",\"kind\":\"\",\"data\":\"object file /gone/Apc/Spec/Defs.olean of module Apc.Spec.Defs does not exist\"}}'; exit 1; fi\nexec '{real}' \"$@\"\n",
        m = mark.display()
    );
    let fake = bin.join("lean");
    fs::write(&fake, script).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    // SAFETY: this test binary has a single test, so no other thread reads the env.
    unsafe { std::env::set_var("PATH", format!("{}:{path}", bin.display())) };

    let camp = tmp.path().join("camp");
    paths::init_campaign(&camp, common::real_host_project(), Some("isort"), &[BackendKind::Claude]).unwrap();
    let ctx = Ctx::for_test(&camp, tmp.path().join("state")).unwrap();
    let tree = ctx.campaign.lean_dir();
    let build_dir = ctx.campaign.build_work_dir();
    let targets = ["Apc.Challenge".to_string()];

    let rep = lean::build::build(&ctx, &tree, &build_dir, Some(&targets)).unwrap();
    assert!(mark.exists(), "the fake lean was not used");
    assert!(rep.ok, "{:?}", rep.results.values().filter(|r| !r.ok).collect::<Vec<_>>());
    assert_eq!(rep.retries, 1);

    // A real elaboration error is reported at once, not rerun.
    let defs = tree.join("Apc/Spec/Defs.lean");
    let orig = fs::read_to_string(&defs).unwrap();
    fs::write(&defs, format!("{orig}\ndef broken :=\n")).unwrap();
    let rep = lean::build::build(&ctx, &tree, &build_dir, Some(&targets)).unwrap();
    assert!(!rep.ok);
    assert_eq!(rep.retries, 0);
}
