#![cfg(windows)]
#[path = "support/upgrade.rs"]
mod support;
use std::{fs, thread, time::Duration};
use support::{Install, bounded, hash, success};

#[test]
fn replacement_restarts_exact_candidate_and_keeps_settings_history_and_guards() {
    for guarded in [false, true] {
        let install = Install::new(guarded);
        let db = rusqlite::Connection::open(install.root.join("discord_mirror.sqlite")).unwrap();
        db.execute(
            "INSERT INTO cdr_runtime_capability_requirements(component,format_version) VALUES (?,?)",
            rusqlite::params![
                cdr_store::async_question::FREE_TEXT_COMPONENT,
                cdr_store::async_question::FREE_TEXT_FORMAT_VERSION
            ],
        )
        .unwrap();
        drop(db);
        let result = install.run();
        success(&result);
        assert_eq!(
            hash(&install.root.join("target/release/cdr-runtime.exe")),
            hash(&install.candidate)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("upgrade_verified"));
        assert!(install.root.join("codex-discord-tray-agy.ps1").is_file());
        assert!(!install.root.join("codex-agy.json").exists());
        assert!(
            !install
                .root
                .join(".codex_discord_rust.upgrade.json")
                .exists()
        );
        assert!(!install.root.join(".codex_discord_bot.disabled").exists());
        install.assert_preserved();
        let owner = fs::read(install.root.join(".codex_discord_rust.runtime.lock")).unwrap();
        let repeated = install.run();
        success(&repeated);
        assert!(String::from_utf8_lossy(&repeated.stdout).contains("upgrade_already_installed"));
        assert_eq!(
            fs::read(install.root.join(".codex_discord_rust.runtime.lock")).unwrap(),
            owner
        );
    }
}
#[test]
fn busy_work_is_not_stopped_and_interrupted_updater_resumes_same_operation() {
    let install = Install::new(false);
    fs::write(install.root.join("busy"), b"active job").unwrap();
    let mut updater = install.waiting_upgrade();
    assert!(!install.root.join(".codex_discord_rust.stop").exists());
    assert_eq!(
        fs::read(install.root.join("target/release/cdr-runtime.exe")).unwrap(),
        install.previous
    );
    let operation = install.state()["Operation"].clone();
    updater.kill().unwrap();
    updater.wait().unwrap();
    fs::remove_file(install.root.join("busy")).unwrap();
    let mut resume = install.command();
    resume.arg("-Resume");
    let result = bounded(resume);
    success(&result);
    assert!(String::from_utf8_lossy(&result.stdout).contains(operation.as_str().unwrap()));
    install.assert_preserved();
}
#[test]
fn failed_readiness_restores_previous_program_and_restarts_without_database_restore() {
    let install = Install::new(true);
    let original_guard =
        fs::read(install.root.join(".codex_discord_rust.compatibility.json")).unwrap();
    fs::write(install.root.join("fail-readiness"), b"reject candidate").unwrap();
    let result = install.run();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("upgrade_rolled_back"),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(install.root.join("target/release/cdr-runtime.exe")).unwrap(),
        install.previous
    );
    assert_eq!(
        fs::read(install.root.join(".codex_discord_rust.compatibility.json")).unwrap(),
        original_guard
    );
    assert!(!install.root.join("codex-discord-tray-agy.ps1").exists());
    install.assert_preserved();
}
#[test]
fn failed_candidate_start_is_not_replayed_or_rolled_back_after_launch() {
    let install = Install::new(false);
    fs::write(install.root.join("fail-start"), b"fail").unwrap();
    let result = install.run();
    assert!(!result.status.success());
    assert_eq!(install.state()["Phase"], "launching");
    assert!(install.root.join(".codex_discord_bot.disabled").exists());
    fs::remove_file(install.root.join("fail-start")).unwrap();
    let mut resume = install.command();
    resume.arg("-Resume");
    let result = bounded(resume);
    assert!(!result.status.success());
    assert!(
        !install
            .root
            .join(".codex_discord_rust.runtime.lock")
            .exists()
    );
    assert_eq!(
        hash(&install.root.join("target/release/cdr-runtime.exe")),
        hash(&install.candidate)
    );
    install.assert_preserved();
}
#[test]
fn preview_is_read_only_and_unknown_schema_fails_before_drain() {
    let install = Install::new(false);
    let mut preview = install.command();
    preview.arg("-Preview");
    success(&bounded(preview));
    assert!(!install.root.join(".codex-discord-backups").exists());
    let db = rusqlite::Connection::open(install.root.join("discord_mirror.sqlite")).unwrap();
    db.execute_batch("PRAGMA user_version=999").unwrap();
    drop(db);
    let output = install.run();
    assert!(!output.status.success());
    assert!(
        !install
            .root
            .join(".codex_discord_rust.drain.prepare")
            .exists()
    );
    assert!(!install.root.join(".codex_discord_bot.disabled").exists());
    install.assert_preserved();
}
#[test]
fn active_agy_is_not_silently_disabled_and_foreign_markers_are_preserved() {
    let install = Install::new(false);
    fs::write(install.root.join("codex-agy.json"),r#"{"version":1,"revision":2,"enabled":true,"config":{"executable":"Z:/missing.exe","projects":[]}}"#).unwrap();
    assert!(!install.run().status.success());
    assert!(
        !install
            .root
            .join(".codex_discord_rust.drain.prepare")
            .exists()
    );
    fs::remove_file(install.root.join("codex-agy.json")).unwrap();
    fs::write(
        install.root.join(".codex_discord_bot.disabled"),
        b"another maintenance",
    )
    .unwrap();
    assert!(!install.run().status.success());
    assert_eq!(
        fs::read(install.root.join(".codex_discord_bot.disabled")).unwrap(),
        b"another maintenance"
    );
    thread::sleep(Duration::from_millis(100));
    install.assert_preserved();
}

#[test]
fn locked_destination_rolls_back_a_partially_replaced_payload() {
    use std::os::windows::fs::OpenOptionsExt;
    let install = Install::new(false);
    // Allow backup reads, but deny atomic replacement of the tray file. The
    // runtime is published first, so recovery must reverse a partial update.
    let tray = install.root.join("codex-discord-tray.ps1");
    // Ensure it differs from the candidate; otherwise replacement is a no-op.
    let mut old_tray = fs::read(&tray).unwrap();
    old_tray.extend_from_slice(b"\r\n# old tray\r\n");
    fs::write(&tray, &old_tray).unwrap();
    let pin = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&tray)
        .unwrap();
    let result = install.run();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("upgrade_rolled_back"),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(install.root.join("target/release/cdr-runtime.exe")).unwrap(),
        install.previous
    );
    assert_eq!(fs::read(&tray).unwrap(), old_tray);
    assert!(!fs::read_dir(&install.root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".upgrade.")
    }));
    drop(pin);
    install.assert_preserved();
}

#[test]
fn changed_staged_artifact_blocks_resume_without_stopping_original_runtime() {
    let install = Install::new(false);
    fs::write(install.root.join("busy"), b"active").unwrap();
    let mut updater = install.waiting_upgrade();
    updater.kill().unwrap();
    updater.wait().unwrap();
    let state = install.state();
    let operation = state["Operation"].as_str().unwrap();
    let staged = install.root.join(format!(
        ".codex-discord-backups/upgrade-{operation}/candidate/target/release/cdr-runtime.exe"
    ));
    fs::write(staged, b"changed staged artifact").unwrap();
    let mut resume = install.command();
    resume.arg("-Resume");
    let result = bounded(resume);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("upgrade_backup_or_candidate_changed")
    );
    assert!(!install.root.join(".codex_discord_rust.stop").exists());
    assert_eq!(
        fs::read(install.root.join("target/release/cdr-runtime.exe")).unwrap(),
        install.previous
    );
    install.assert_preserved();
}

#[test]
fn changed_environment_blocks_resume_and_preserves_the_users_new_settings() {
    let install = Install::new(false);
    fs::write(install.root.join("busy"), b"active").unwrap();
    let mut updater = install.waiting_upgrade();
    updater.kill().unwrap();
    updater.wait().unwrap();
    let mut changed = install.environment.clone();
    changed.extend_from_slice(b"# user changed settings while paused\r\n");
    fs::write(install.root.join(".env"), &changed).unwrap();
    let mut resume = install.command();
    resume.arg("-Resume");
    let result = bounded(resume);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("upgrade_environment_changed"));
    assert_eq!(fs::read(install.root.join(".env")).unwrap(), changed);
    assert!(!install.root.join(".codex_discord_rust.stop").exists());
    assert_eq!(
        fs::read(install.root.join("target/release/cdr-runtime.exe")).unwrap(),
        install.previous
    );
}
