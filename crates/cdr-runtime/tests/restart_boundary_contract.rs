#![cfg(windows)]
#[path = "support/maintenance_native.rs"]
mod fixture;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn run(case: &str, variant: &str) {
    let root = tempfile::tempdir().unwrap();
    fixture::run_fixture(
        root.path(),
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/restart_boundary"),
        case,
        variant,
    );
}
#[test]
fn failed_preflight_never_seals_intake() {
    run("preflight.ps1", "");
}
#[test]
fn queue_only_restart_refuses_without_writes() {
    run("queue_refusal.ps1", "");
}
#[test]
fn normal_exit_after_prepare_completes_bound_restart() {
    run("normal_exit.ps1", "");
}
#[test]
fn readiness_invokes_native_arguments_in_repo_not_legacy_bridge() {
    run("native_readiness.ps1", "");
}
#[test]
fn timeout_keeps_prepare_visible() {
    run("drain.ps1", "timeout");
}
#[test]
fn stale_ack_never_authorizes_restart() {
    run("drain.ps1", "stale_ack");
}
#[test]
fn orphan_cleanup_requires_verified_runtime_absence() {
    run("drain.ps1", "orphan");
}
#[test]
fn default_root_is_script_folder_from_unrelated_directory() {
    let root = tempfile::Builder::new()
        .prefix("restart path ")
        .tempdir()
        .unwrap();
    fs::copy(
        repo().join("codex-discord-rust-restart.ps1"),
        root.path().join("restart.ps1"),
    )
    .unwrap();
    fs::create_dir_all(root.path().join("scripts")).unwrap();
    for file in [
        "CdrRuntimeIdentity.psm1",
        "CdrBuildProvenance.psm1",
        "CdrNativeProcess.psm1",
        "CdrAsyncRecoveryCompatibility.ps1",
    ] {
        fs::copy(
            repo().join("scripts").join(file),
            root.path().join("scripts").join(file),
        )
        .unwrap();
    }
    fs::create_dir_all(root.path().join("target/release")).unwrap();
    fs::copy(
        env!("CARGO_BIN_EXE_cdr-runtime"),
        root.path().join("target/release/cdr-runtime.exe"),
    )
    .unwrap();
    fs::write(root.path().join("codex-discord-rust-watchdog.ps1"),"param($RepoRoot,$BinaryPath,[switch]$DryRun,$RestartQuietSeconds)\nif(-not $DryRun){throw 'not a dry run'}\nWrite-Output $RepoRoot\nexit 0\n").unwrap();
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-File"])
        .arg(root.path().join("restart.ps1"))
        .arg("-DryRun")
        .current_dir(root.path().parent().unwrap())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let output_root = String::from_utf8(out.stdout).unwrap();
    let returned_root = Path::new(output_root.trim());
    assert!(returned_root.is_absolute());
    // PowerShell expands Windows short-path aliases in its script directory.
    assert_eq!(
        returned_root.canonicalize().unwrap(),
        root.path().canonicalize().unwrap()
    );
}
#[test]
fn restart_entrypoints_have_no_legacy_runtime_dependency() {
    let source = ["watchdog", "restart", "drain"]
        .map(|name| {
            fs::read_to_string(repo().join(format!("codex-discord-rust-{name}.ps1"))).unwrap()
        })
        .join("\n")
        .to_lowercase();
    for forbidden in [
        ".py",
        "bridgepath",
        "resolvecodexruntimepythonexecutable",
        "codex_desktop_bridge",
        "codex-discord-watchdog-restart-runtime.ps1",
        "pending_codex_desktop_request",
        r"\\.\pipe",
    ] {
        assert!(!source.contains(forbidden), "{forbidden}");
    }
}
#[test]
fn watchdog_delegates_readiness_to_native_cli() {
    let source = fs::read_to_string(repo().join("codex-discord-rust-watchdog.ps1")).unwrap();
    for arg in [
        "--restart-readiness",
        "--restart-quiet-seconds",
        "--restart-wait-timeout-seconds",
    ] {
        assert!(source.contains(arg));
    }
    assert!(!source.contains("Wait-CodexThreadsQuietForRestart"));
}
#[test]
fn watchdog_requires_ack_then_bound_probe_then_restart_marker() {
    let source = fs::read_to_string(repo().join("codex-discord-rust-watchdog.ps1")).unwrap();
    let mut rest = source
        .split_once("if ($CheckRestartReady -or $PrepareRestart)")
        .unwrap()
        .1;
    for token in [
        "Enter-RestartDrain",
        "Assert-RestartDrainBound",
        "Wait-RustThreadsQuietForRestart",
        "Assert-RestartDrainBound",
        "Write-BoundRestartMarker",
    ] {
        rest = rest.split_once(token).unwrap().1;
    }
}
#[test]
fn crashed_prepare_resumes_without_unsealing() {
    let source = fs::read_to_string(repo().join("codex-discord-rust-watchdog.ps1")).unwrap();
    assert!(source.contains("restart_drain_resume"));
    assert!(source.contains("(Test-Path -LiteralPath $DrainPreparePath -PathType Leaf)"));
    assert!(!source.contains("Remove-Item -LiteralPath $DrainPreparePath"));
}
#[test]
fn prepare_failure_has_no_stop_or_force_kill_path() {
    let source = fs::read_to_string(repo().join("codex-discord-rust-watchdog.ps1")).unwrap();
    let branch = source
        .split_once("if ($CheckRestartReady -or $PrepareRestart)")
        .unwrap()
        .1
        .split_once("if (Test-Path -LiteralPath $StopPath)")
        .unwrap()
        .0;
    for token in [
        "Write-AtomicRestartMarker -Path $StopPath",
        "Wait-RustRuntimeExit",
        "Stop-VerifiedRuntime",
    ] {
        assert!(!branch.contains(token));
    }
}
#[test]
fn deferred_restart_logs_actual_failure() {
    let source = fs::read_to_string(repo().join("codex-discord-rust-restart.ps1")).unwrap();
    let branch = source
        .split_once("if ($Deferred)")
        .unwrap()
        .1
        .split_once("$currentIdentity")
        .unwrap()
        .0;
    assert!(branch.contains("$_.Exception.Message") && branch.contains("restart_failed"));
}
#[test]
fn queued_restart_sets_success_exit_code() {
    let source = fs::read_to_string(repo().join("codex-discord-rust-restart.ps1")).unwrap();
    assert!(
        source
            .split_once("$escapedScript =")
            .unwrap()
            .1
            .trim_end()
            .ends_with("exit 0")
    );
}
