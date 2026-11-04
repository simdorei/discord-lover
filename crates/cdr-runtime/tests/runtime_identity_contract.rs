#![cfg(windows)]
use std::{path::Path, process::Command};

#[test]
fn startup_and_completion_proofs_refuse_wrong_artifacts_and_reused_process_ids() {
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/Test-CdrRuntimeIdentity.ps1"),
        )
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("runtime_identity_tests_passed"));
}
