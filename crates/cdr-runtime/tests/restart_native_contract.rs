#![cfg(windows)]
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::create_dir(root.path().join("scripts")).unwrap();
    for file in [
        "codex-discord-rust-watchdog.ps1",
        "codex-discord-rust-drain.ps1",
        "codex-discord-rust-control.ps1",
        "scripts/CdrDeploymentRecovery.ps1",
        "scripts/CdrLaunchJournal.ps1",
        "scripts/CdrRestartTransaction.ps1",
        "scripts/CdrRestartPreparation.ps1",
        "scripts/CdrRuntimeIdentity.psm1",
        "scripts/CdrBuildProvenance.psm1",
        "scripts/CdrNativeProcess.psm1",
        "scripts/CdrForceRestart.ps1",
        "scripts/CdrMaintenanceCompatibility.ps1",
        "scripts/CdrAsyncRecoveryCompatibility.ps1",
        "scripts/CdrRuntimeLaunchCompatibility.ps1",
    ] {
        fs::copy(repo.join(file), root.path().join(file)).unwrap();
    }
    fs::write(
        root.path().join("probe.exe"),
        b"not executable; only the process boundary is substituted",
    )
    .unwrap();
    for name in ["restart", "drain.prepare", "drain.ack"] {
        let mut fence =
            "version=1\nruntime_id=runtime-a\nprocess_identity=42|99\nnonce=current\n".to_owned();
        if name == "drain.ack" {
            fence.push_str("state=sealed\n");
        }
        fs::write(
            root.path().join(format!(".codex_discord_rust.{name}")),
            fence,
        )
        .unwrap();
    }
    root
}

fn run(root: &Path, file: &str, args: &[&str]) -> String {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/restart");
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(fixtures.join(file))
        .args(args)
        .env("ENTRY_ROOT", root)
        .env("RESTART_ENTRY_ROOT", root)
        .env("RESTART_FIXTURES", fixtures)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("restart fixture timed out: {file}");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{file}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn consumed(root: &Path) {
    for name in ["restart", "drain.prepare", "drain.ack"] {
        assert!(
            !root.join(format!(".codex_discord_rust.{name}")).exists(),
            "{name}"
        );
    }
}

#[test]
fn bound_restart_waits_for_old_exit_without_second_probe() {
    let root = fixture();
    let result = run(root.path(), "entry.ps1", &["-OldAlive"]);
    assert!(result.find("WAIT_EXIT").unwrap() < result.find("START_ATTEMPT=1").unwrap());
    assert_eq!(result.matches("START_ATTEMPT=").count(), 1);
    assert!(
        !fs::read_to_string(root.path().join("discord_launcher.log"))
            .unwrap()
            .contains("restart_drain_resume")
    );
    consumed(root.path());
}

#[test]
fn definite_launch_failure_retains_authorization_for_next_watchdog() {
    let root = fixture();
    let result = run(root.path(), "entry.ps1", &["-FailFirst"]);
    assert!(result.contains("START_ATTEMPT=1") && result.contains("START_ATTEMPT=2"));
    consumed(root.path());
}

#[test]
fn uncertain_launch_never_authorizes_a_second_start() {
    let root = fixture();
    let result = run(root.path(), "entry.ps1", &["-FailFirst", "-StartedAlive"]);
    assert_eq!(result.matches("START_ATTEMPT=").count(), 1);
    assert!(!root.path().join(".codex_discord_rust.drain.ack").exists());
    assert!(
        root.path()
            .join(".codex_discord_rust.restart.launch")
            .exists()
    );
}

#[test]
fn failed_launch_does_not_overwrite_another_drain_fence() {
    let root = fixture();
    let path = root.path().join(".codex_discord_rust.drain.prepare");
    let foreign = fs::read_to_string(&path)
        .unwrap()
        .replace("nonce=current", "nonce=another");
    fs::write(&path, &foreign).unwrap();
    run(root.path(), "foreign.ps1", &[]);
    assert_eq!(fs::read_to_string(path).unwrap(), foreign);
}

#[test]
fn receipt_write_failure_next_entry_adopts_same_child() {
    let root = fixture();
    run(
        root.path(),
        "recovery.ps1",
        &[
            "-CaseName",
            "test_receipt_write_failure_next_entry_adopts_same_child",
        ],
    );
}

#[test]
fn prelaunch_interruption_after_claim_cleanup_is_recoverable() {
    let root = fixture();
    run(
        root.path(),
        "recovery.ps1",
        &[
            "-CaseName",
            "test_prelaunch_interruption_after_claim_or_drain_cleanup_is_recoverable",
        ],
    );
}

#[test]
fn prepare_preserves_invalid_restart_without_entering_drain() {
    let root = fixture();
    let path = root.path().join(".codex_discord_rust.restart");
    fs::write(&path, "foreign-invalid").unwrap();
    run(
        root.path(),
        "recovery.ps1",
        &[
            "-Prepare",
            "-CaseName",
            "test_prepare_preserves_existing_invalid_restart_without_entering_drain",
        ],
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "foreign-invalid");
}

#[test]
fn prepare_preserves_foreign_stop_and_disabled_without_entering_drain() {
    for name in [".codex_discord_rust.stop", ".codex_discord_bot.disabled"] {
        let root = fixture();
        fs::remove_file(root.path().join(".codex_discord_rust.restart")).unwrap();
        fs::write(root.path().join(name), "foreign").unwrap();
        run(
            root.path(),
            "recovery.ps1",
            &[
                "-Prepare",
                "-CaseName",
                "test_prepare_preserves_stop_and_disabled_without_entering_drain",
            ],
        );
        assert_eq!(
            fs::read_to_string(root.path().join(name)).unwrap(),
            "foreign"
        );
    }
}

#[test]
fn watchdog_respects_maintenance_owner_before_process_actions() {
    for stage in ["state_only", "disabled_only", "with_legacy_restart"] {
        let root = fixture();
        let marker = root.path().join(".codex_discord_rust.maintenance.v2");
        fs::write(&marker, "owned v2 state").unwrap();
        if stage != "with_legacy_restart" {
            for name in ["restart", "drain.prepare", "drain.ack"] {
                fs::remove_file(root.path().join(format!(".codex_discord_rust.{name}"))).unwrap();
            }
        }
        if stage == "disabled_only" {
            fs::write(root.path().join(".codex_discord_bot.disabled"), "owned").unwrap();
        }
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/restart/entry.ps1"))
            .arg("-OldAlive")
            .env("RESTART_ENTRY_ROOT", root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("maintenance_v2_pending"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("WAIT_EXIT") && !stdout.contains("START_ATTEMPT="));
        assert_eq!(fs::read_to_string(marker).unwrap(), "owned v2 state");
    }
}

#[test]
fn preparing_journal_survives_crashes_before_drain_and_after_restart_marker() {
    let root = fixture();
    run(
        root.path(),
        "recovery.ps1",
        &[
            "-CaseName",
            "test_preparing_journal_recovers_without_unsealed_launch",
        ],
    );
}

#[test]
fn changed_artifact_is_rejected_before_intake_is_sealed() {
    let root = fixture();
    run(
        root.path(),
        "recovery.ps1",
        &["-CaseName", "test_changed_artifact_refuses_before_drain"],
    );
}
