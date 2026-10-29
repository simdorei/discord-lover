use super::*;

#[test]
fn preflight_needs_both_readiness_and_close_and_preserves_primary_failures() {
    assert!(finish_preflight(Ok(RestartReadinessState::Ready), Ok(())).is_ok());
    assert_eq!(
        finish_preflight(
            Ok(RestartReadinessState::Ready),
            Err("close fixture".into())
        )
        .unwrap_err()
        .to_string(),
        "close fixture"
    );
    for read_error in [false, true] {
        for close_error in [false, true] {
            let result = if read_error {
                Err("read fixture".into())
            } else {
                Ok(RestartReadinessState::Blocked {
                    reason: "busy fixture".into(),
                })
            };
            let close = if close_error {
                Err("close fixture".into())
            } else {
                Ok(())
            };
            let error = finish_preflight(result, close).unwrap_err().to_string();
            assert!(error.starts_with(if read_error {
                "read fixture"
            } else {
                "busy fixture"
            }));
            assert_eq!(error.contains("close fixture"), close_error);
        }
    }
}

#[test]
fn update_operator_accepts_only_two_modes_and_exact_argument_shape() {
    for mode in ["preflight", "cleanup"] {
        let args = [mode, "--env", "fixture.env"].map(OsString::from);
        assert!(parse(&args).is_ok());
    }
    for args in [
        vec![],
        vec!["cleanup"],
        vec!["delete", "--env", "fixture.env"],
        vec!["cleanup", "--backup-store", "fixture.env"],
        vec!["cleanup", "--env", "fixture.env", "--help"],
    ] {
        assert!(parse(&args.into_iter().map(OsString::from).collect::<Vec<_>>()).is_err());
    }
}

#[test]
fn cleanup_without_seal_cannot_take_a_runtime_lock_or_change_data() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("discord_mirror.sqlite");
    std::fs::write(&db, b"not SQLite; must not be opened").unwrap();
    assert!(verify_noop_cleanup(temp.path()).is_err());
    assert!(
        !temp
            .path()
            .join(".codex_discord_rust.runtime.lock")
            .exists()
    );
    assert_eq!(
        std::fs::read(&db).unwrap(),
        b"not SQLite; must not be opened"
    );
}

#[test]
fn cleanup_never_overwrites_a_running_instance_marker() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join(".codex_discord_bot.disabled"),
        b"owned fixture",
    )
    .unwrap();
    let owner = RuntimeInstanceGuard::acquire(temp.path()).unwrap();
    let lock = temp.path().join(".codex_discord_rust.runtime.lock");
    let before = std::fs::read(&lock).unwrap();
    assert!(
        verify_noop_cleanup(temp.path())
            .unwrap_err()
            .to_string()
            .contains("already running")
    );
    assert_eq!(std::fs::read(&lock).unwrap(), before);
    drop(owner);
}

#[test]
fn sealed_cleanup_preserves_all_data_and_markers_without_opening_a_store() {
    let temp = tempfile::tempdir().unwrap();
    let names = [
        "discord_mirror.sqlite",
        "discord_mirror.sqlite-wal",
        "state.sqlite",
        ".codex_discord_bot.disabled",
        ".codex_discord_rust.stop",
        ".codex_discord_rust.heartbeat",
    ];
    for name in names {
        std::fs::write(temp.path().join(name), name.as_bytes()).unwrap();
    }
    verify_noop_cleanup(temp.path()).unwrap();
    for name in names {
        assert_eq!(
            std::fs::read(temp.path().join(name)).unwrap(),
            name.as_bytes()
        );
    }
    assert!(
        !temp
            .path()
            .join(".codex_discord_rust.runtime.lock")
            .exists()
    );
    // The cross-session lock must persist so concurrent openers share the same
    // inode. It is empty coordination metadata, not a store or runtime marker.
    let exclusive_name = ".codex_discord_rust.exclusive.lock";
    assert!(
        std::fs::read(temp.path().join(exclusive_name))
            .unwrap()
            .is_empty()
    );
    let expected = names
        .into_iter()
        .chain([exclusive_name])
        .map(OsString::from)
        .collect::<std::collections::BTreeSet<_>>();
    let actual = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(actual, expected);
    // Leaving the file behind must not leave its exclusive OS lock held.
    drop(RuntimeInstanceGuard::acquire(temp.path()).unwrap());
}
