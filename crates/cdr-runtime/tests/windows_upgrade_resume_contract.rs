#![cfg(windows)]
#[path = "support/upgrade_pause.rs"]
mod pause;
#[path = "support/upgrade.rs"]
mod support;
use pause::{PausedUpgrade, starts};
use std::fs;
use support::{Install, bounded, success};

fn other_database(install: &Install) -> std::path::PathBuf {
    let directory = install.temp.path().join("other database");
    fs::create_dir(&directory).unwrap();
    let path = directory.join("other.sqlite");
    fs::copy(install.root.join("discord_mirror.sqlite"), &path).unwrap();
    path
}

#[test]
fn changed_inherited_database_is_held_before_stopping_or_backing_up_during_resume() {
    let install = Install::new(false);
    fs::write(install.root.join("busy"), b"active").unwrap();
    let original = fs::read(install.root.join(".codex_discord_rust.runtime.lock")).unwrap();
    let mut updater = install.waiting_upgrade();
    updater.kill().unwrap();
    updater.wait().unwrap();
    fs::remove_file(install.root.join("busy")).unwrap();
    let other = other_database(&install);
    let mut resume = install.command();
    resume.arg("-Resume").env("CODEX_DISCORD_MIRROR_DB", &other);
    let rejected = bounded(resume);
    assert!(!rejected.status.success());
    assert_eq!(
        starts(&install),
        1,
        "a changed environment must never launch a rollback"
    );
    assert_eq!(
        fs::read(install.root.join(".codex_discord_rust.runtime.lock")).unwrap(),
        original
    );
    assert!(
        !other
            .parent()
            .unwrap()
            .join(".codex-discord-backups")
            .exists()
    );
    assert!(!install.root.join(".codex_discord_rust.stop").exists());
    assert_eq!(install.state()["Phase"], "draining");
    install.assert_preserved();
    let mut restored = install.command();
    restored.arg("-Resume");
    success(&bounded(restored));
    assert_eq!(starts(&install), 2);
    install.assert_preserved();
}

#[test]
fn launch_ready_and_prepared_launch_require_original_effective_settings() {
    for phase in ["launch_ready", "launching"] {
        for guarded in [false, true] {
            let install = Install::new(guarded);
            let paused = PausedUpgrade::at(&install, phase);
            let mut resume = paused.resume(&install);
            let other = other_database(&install);
            if guarded {
                resume.env("CODEX_HOME", install.temp.path().join("different home"));
            } else {
                resume.env("CODEX_DISCORD_MIRROR_DB", &other);
            }
            let rejected = bounded(resume);
            assert!(
                !rejected.status.success(),
                "changed settings must hold {phase}"
            );
            assert_eq!(starts(&install), 1, "no candidate child at {phase}");
            assert_eq!(install.state()["Phase"], phase);
            assert!(
                !other
                    .parent()
                    .unwrap()
                    .join(".codex-discord-backups")
                    .exists()
            );
            install.assert_preserved();
            success(&bounded(paused.resume(&install)));
            assert_eq!(
                starts(&install),
                2,
                "exactly one launch after restoring settings"
            );
            install.assert_preserved();
        }
    }
}

#[test]
fn stopped_installed_and_rollback_resume_never_launch_with_changed_settings() {
    for phase in ["stopped", "installed", "rolling_back"] {
        let install = Install::new(false);
        let paused = PausedUpgrade::at(&install, phase);
        let other = other_database(&install);
        let before = fs::read(install.root.join("target/release/cdr-runtime.exe")).unwrap();
        let mut resume = paused.resume(&install);
        resume.env("CODEX_DISCORD_MIRROR_DB", &other);
        let rejected = bounded(resume);
        assert!(!rejected.status.success());
        assert_eq!(
            starts(&install),
            1,
            "binding rejection must not restart old bot at {phase}"
        );
        assert_eq!(install.state()["Phase"], phase);
        assert_eq!(
            fs::read(install.root.join("target/release/cdr-runtime.exe")).unwrap(),
            before
        );
        assert!(
            !other
                .parent()
                .unwrap()
                .join(".codex-discord-backups")
                .exists()
        );
        install.assert_preserved();
        success(&bounded(paused.resume(&install)));
        assert_eq!(starts(&install), 2);
        install.assert_preserved();
    }
}

#[test]
fn changed_agy_settings_path_cannot_enable_agy_on_prepared_launch_resume() {
    let install = Install::new(true);
    let paused = PausedUpgrade::at(&install, "launching");
    let agy = install.temp.path().join("enabled-agy.json");
    fs::write(&agy, r#"{"version":1,"revision":2,"enabled":true,"config":{"executable":"Z:/missing.exe","projects":[]}}"#).unwrap();
    let mut resume = paused.resume(&install);
    resume.env("DISCORD_AGY_CONFIG", &agy);
    assert!(!bounded(resume).status.success());
    assert_eq!(starts(&install), 1);
    assert_eq!(install.state()["Phase"], "launching");
    success(&bounded(paused.resume(&install)));
    assert_eq!(starts(&install), 2);
    install.assert_preserved();
}
