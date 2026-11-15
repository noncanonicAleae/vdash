use std::path::PathBuf;
use std::process::Command;

#[macro_export]
macro_rules! require_lean {
    () => {
        if $crate::common::should_skip_lean() {
            return;
        }
    };
}

pub fn should_skip_lean() -> bool {
    if std::env::var("APC_SKIP_LEAN").as_deref() == Ok("1") {
        return true;
    }
    let ok = Command::new("lean")
        .env("ELAN_TOOLCHAIN", "leanprover/lean4:v4.33.0")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        panic!(
            "lean (leanprover/lean4:v4.33.0) is missing or failed and APC_SKIP_LEAN!=1"
        );
    }
    false
}

#[allow(dead_code)]
pub fn real_host_project() -> PathBuf {
    PathBuf::from(std::env::var("VDASH_TEST_HOST").unwrap_or_else(|_| "/nonexistent-lean-host".into()))
}
