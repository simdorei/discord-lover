#![cfg(windows)]

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn run_watchdog(
    watchdog: &std::path::Path,
    root: &std::path::Path,
    binary: &std::path::Path,
    extra: &[&str],
) -> std::process::Output {
    // The production watchdog loads its drain helper from the selected root.
    // Install the real dependency in the isolated fixture, never the live root.
    let repo = watchdog.parent().unwrap();
    for support in [
        "codex-discord-rust-drain.ps1",
        "codex-discord-rust-control.ps1",
    ] {
        fs::copy(repo.join(support), root.join(support)).unwrap();
    }
    fs::create_dir_all(root.join("scripts")).unwrap();
    for entry in fs::read_dir(repo.join("scripts")).unwrap() {
        let entry = entry.unwrap();
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "ps1" || extension == "psm1")
        {
            fs::copy(entry.path(), root.join("scripts").join(entry.file_name())).unwrap();
        }
    }
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(watchdog)
        .arg("-RepoRoot")
        .arg(root)
        .arg("-BinaryPath")
        .arg(binary)
        .arg("-DryRun")
        .args(extra);
    command.output().unwrap()
}

#[test]
fn rust_watchdog_dry_run_detects_missing_and_verified_running_processes() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let watchdog = repo.join("codex-discord-rust-watchdog.ps1");
    let temp = tempfile::tempdir().unwrap();
    let binary = std::env::current_exe().unwrap();

    let missing = run_watchdog(&watchdog, temp.path(), &binary, &[]);
    assert!(
        missing.status.success(),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(String::from_utf8_lossy(&missing.stdout).contains("would_start"));

    fs::write(
        temp.path().join(".codex_discord_rust.runtime.lock"),
        format!("pid={}\n", std::process::id()),
    )
    .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(
        temp.path().join(".codex_discord_rust.heartbeat"),
        format!("pid={}\nupdated_at={now}\n", std::process::id()),
    )
    .unwrap();
    let running = run_watchdog(&watchdog, temp.path(), &binary, &[]);
    assert!(running.status.success());
    assert!(String::from_utf8_lossy(&running.stdout).contains("running"));
}

#[test]
fn heartbeat_must_match_the_lock_pid_but_bootstrap_grace_prevents_early_restart() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let watchdog = repo.join("codex-discord-rust-watchdog.ps1");
    let temp = tempfile::tempdir().unwrap();
    let binary = std::env::current_exe().unwrap();
    fs::write(
        temp.path().join(".codex_discord_rust.runtime.lock"),
        format!("pid={}\n", std::process::id()),
    )
    .unwrap();
    fs::write(
        temp.path().join(".codex_discord_rust.heartbeat"),
        b"pid=999999\nupdated_at=1\n",
    )
    .unwrap();

    let within_grace = run_watchdog(&watchdog, temp.path(), &binary, &[]);
    assert!(within_grace.status.success());
    assert!(String::from_utf8_lossy(&within_grace.stdout).contains("running_bootstrap"));

    let grace_expired = run_watchdog(
        &watchdog,
        temp.path(),
        &binary,
        &["-HealthHeartbeatStartupGraceSeconds", "0"],
    );
    assert!(grace_expired.status.success());
    assert!(String::from_utf8_lossy(&grace_expired.stdout).contains("would_restart_unhealthy"));
}

#[test]
fn stale_restart_marker_is_not_allowed_to_restart_a_replacement_process() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let watchdog = repo.join("codex-discord-rust-watchdog.ps1");
    let temp = tempfile::tempdir().unwrap();
    let binary = std::env::current_exe().unwrap();
    fs::write(
        temp.path().join(".codex_discord_rust.runtime.lock"),
        format!("pid={}\n", std::process::id()),
    )
    .unwrap();
    fs::write(
        temp.path().join(".codex_discord_rust.restart"),
        b"version=1\nruntime_id=old-runtime\nprocess_identity=999999|0\nnonce=old-nonce\n",
    )
    .unwrap();

    let output = run_watchdog(&watchdog, temp.path(), &binary, &[]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("would_ignore_stale_restart"));
    assert!(temp.path().join(".codex_discord_rust.restart").exists());
}

#[test]
fn unsupported_resource_health_thresholds_fail_loudly() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let watchdog = repo.join("codex-discord-rust-watchdog.ps1");
    let temp = tempfile::tempdir().unwrap();
    let binary = std::env::current_exe().unwrap();

    for arguments in [
        ["-HealthCpuPercent", "1"],
        ["-HealthFreeMemoryMb", "1"],
        ["-HealthBadSampleLimit", "1"],
    ] {
        let output = run_watchdog(&watchdog, temp.path(), &binary, &arguments);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("Rust watchdog supports heartbeat health only")
        );
    }
}

#[test]
fn shared_launcher_and_watchdog_route_by_the_rollback_mode_marker() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let launcher = fs::read_to_string(repo.join("codex-discord-bot.cmd")).unwrap();
    let watchdog = fs::read_to_string(repo.join("codex-discord-watchdog.ps1")).unwrap();
    for text in [&launcher, &watchdog] {
        assert!(text.contains(".codex_discord_runtime"));
        assert!(text.contains("rust"));
    }
    assert!(launcher.contains("cdr-runtime.exe"));
    assert!(watchdog.contains("codex-discord-rust-watchdog.ps1"));
}

#[test]
fn deliberate_stop_and_restart_wait_for_rust_graceful_exit() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let watchdog = fs::read_to_string(repo.join("codex-discord-rust-watchdog.ps1")).unwrap();

    assert!(watchdog.contains("Wait-RustRuntimeExit"));
    assert!(watchdog.contains("graceful_exit_timeout"));
    assert!(watchdog.contains("stale_heartbeat"));
}

#[test]
fn requested_restart_only_sets_remote_mcp_resume_for_the_replacement_process() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let watchdog = fs::read_to_string(repo.join("codex-discord-rust-watchdog.ps1")).unwrap();

    assert!(watchdog.contains("CODEX_REMOTE_MCP_RESTART_RESUME"));
    assert!(watchdog.contains("ResumeRemoteMcp"));
    assert!(watchdog.contains("finally"));
}

#[test]
fn rust_restart_binds_all_safety_inputs_to_the_original_process() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let restart = fs::read_to_string(repo.join("codex-discord-rust-restart.ps1")).unwrap();

    assert!(restart.contains("Get-VerifiedRustIdentity"));
    assert!(restart.contains("ExpectedBotIdentity"));
    assert!(restart.contains("restart no longer matches the requested Rust process"));
    assert!(restart.contains("Rust process changed during restart preflight; no drain requested"));
    assert!(restart.contains("-PrepareRestart"));
    assert!(restart.contains("-ExpectedRuntimeIdentity $Identity"));
    assert!(restart.contains("-RestartQuietSeconds $EffectiveQuietSeconds"));
    assert!(restart.contains("-RestartWaitTimeoutSeconds $WaitTimeoutSeconds"));
}

#[test]
fn legacy_identity_only_restart_cannot_authorize_the_new_drain_protocol() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".codex_discord_rust.restart"),
        b"identity=999999|0\n",
    )
    .unwrap();
    let output = run_watchdog(
        &repo.join("codex-discord-rust-watchdog.ps1"),
        temp.path(),
        &std::env::current_exe().unwrap(),
        &[],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing version"));
    assert!(temp.path().join(".codex_discord_rust.restart").exists());
}
