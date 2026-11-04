use std::fs;
#[cfg(windows)]
use std::path::Path;

use cdr_runtime::runtime_instance::{RuntimeInstanceError, RuntimeInstanceGuard};
#[cfg(windows)]
use cdr_windows_native::{NativeError, SingleInstance};
#[cfg(windows)]
use sha2::{Digest, Sha256};

#[test]
fn duplicate_runtime_is_rejected_and_stale_marker_is_recoverable() {
    let temp = tempfile::tempdir().unwrap();
    let lock_path = temp.path().join(".codex_discord_rust.runtime.lock");
    fs::write(&lock_path, b"pid=999999\n").unwrap();

    let first = RuntimeInstanceGuard::acquire(temp.path()).unwrap();
    let marker = fs::read_to_string(&lock_path).unwrap();
    assert!(marker.contains(&format!("pid={}", std::process::id())));
    let startup: serde_json::Value = serde_json::from_str(
        marker
            .lines()
            .find_map(|line| line.strip_prefix("startup="))
            .unwrap(),
    )
    .unwrap();
    let executable = std::env::current_exe().unwrap();
    assert_eq!(
        startup["artifact_sha256"],
        cdr_runtime::build_info::artifact_sha256(&executable).unwrap()
    );
    assert_eq!(
        startup["executable_path"],
        executable.to_string_lossy().as_ref()
    );
    assert_eq!(
        startup["build_info"],
        serde_json::from_str::<serde_json::Value>(cdr_runtime::build_info::JSON).unwrap()
    );
    #[cfg(windows)]
    assert_eq!(
        startup["process_identity"],
        cdr_windows_native::current_process_identity().unwrap()
    );
    assert!(matches!(
        RuntimeInstanceGuard::acquire(temp.path()),
        Err(RuntimeInstanceError::AlreadyRunning)
    ));

    drop(first);
    assert!(!lock_path.exists());
    RuntimeInstanceGuard::acquire(temp.path()).unwrap();
}

#[cfg(windows)]
#[test]
fn python_compatible_mutex_excludes_rust_runtime_and_recovers_after_release() {
    let temp = tempfile::tempdir().unwrap();
    let mutex_name = python_runtime_mutex_name(temp.path());

    let python_equivalent = SingleInstance::acquire(&mutex_name).unwrap();
    assert!(matches!(
        RuntimeInstanceGuard::acquire(temp.path()),
        Err(RuntimeInstanceError::AlreadyRunning)
    ));

    drop(python_equivalent);
    let rust = RuntimeInstanceGuard::acquire(temp.path()).unwrap();
    assert!(matches!(
        SingleInstance::acquire(&mutex_name),
        Err(NativeError::AlreadyRunning)
    ));

    drop(rust);
    SingleInstance::acquire(&mutex_name).unwrap();
}

#[cfg(windows)]
fn python_runtime_mutex_name(root: &Path) -> String {
    let normalized_root = root.to_string_lossy().to_lowercase();
    let digest = Sha256::digest(normalized_root.as_bytes());
    format!("Local\\CodexDiscordBot_{}", &hex::encode(digest)[..16])
}
