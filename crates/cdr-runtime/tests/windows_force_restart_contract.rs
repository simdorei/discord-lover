#![cfg(windows)]
//! Isolated force restart checks: no Discord connection or live bot processes.
use std::{path::Path, process::Command};

#[test]
fn force_restart_interrupts_busy_work_and_preserves_process_ownership() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(repo.join("scripts/Test-CdrForceRestart.ps1"))
        .arg("-RuntimeBinary")
        .arg(env!("CARGO_BIN_EXE_cdr-runtime"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("force_restart_tests_passed"));
}
