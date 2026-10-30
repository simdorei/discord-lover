use serde_json::Value;
use std::fmt::Write as _;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn run(root: &Path, command: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cdr-runtime"))
        .args(["--admin", command, "--repo-root"])
        .arg(root)
        .env_clear()
        .output()
        .unwrap()
}
fn seed(root: &Path) {
    fs::write(root.join(".env"),"# UTF-8 보존\r\nDISCORD_BOT_TOKEN=private-fixture-token\r\nDISCORD_ALLOWED_CHANNEL_IDS=42\r\n").unwrap();
    drop(cdr_store::schema::open_initialized(&root.join("discord_mirror.sqlite")).unwrap());
}
fn proof(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-fixture-token"));
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn inspection_does_not_migrate_launch_or_write_configuration() {
    let root = tempfile::tempdir().unwrap();
    seed(root.path());
    let env = fs::read(root.path().join(".env")).unwrap();
    let db = fs::read(root.path().join("discord_mirror.sqlite")).unwrap();
    let value = proof(&run(root.path(), "inspect-upgrade"));
    assert_eq!(value["read_only"], true);
    assert_eq!(value["agy_enabled"], false);
    assert_eq!(value["admission_authorized"], false);
    assert_eq!(value["recovery_authorized"], false);
    assert_eq!(fs::read(root.path().join(".env")).unwrap(), env);
    assert_eq!(
        fs::read(root.path().join("discord_mirror.sqlite")).unwrap(),
        db
    );
    assert!(!root.path().join(".codex-discord-backups").exists());
    assert!(
        !root
            .path()
            .join(".codex_discord_rust.runtime.lock")
            .exists()
    );
    assert!(!root.path().join("codex-agy.json").exists());
}

#[test]
fn current_question_answer_capability_is_preserved_and_future_formats_stop_before_backup() {
    let root = tempfile::tempdir().unwrap();
    seed(root.path());
    let path = root.path().join("discord_mirror.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "INSERT INTO cdr_runtime_capability_requirements(component,format_version) VALUES (?,?)",
        rusqlite::params![
            cdr_store::async_question::FREE_TEXT_COMPONENT,
            cdr_store::async_question::FREE_TEXT_FORMAT_VERSION
        ],
    )
    .unwrap();
    drop(db);
    let before = fs::read(&path).unwrap();
    assert_eq!(
        proof(&run(root.path(), "inspect-upgrade"))["compatible"],
        true
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE cdr_runtime_capability_requirements SET format_version=2 WHERE component=?",
        [cdr_store::async_question::FREE_TEXT_COMPONENT],
    )
    .unwrap();
    drop(db);
    let before = fs::read(&path).unwrap();
    let output = run(root.path(), "backup-upgrade");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not support"));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!root.path().join(".codex-discord-backups").exists());
}
#[test]
fn backup_uses_configured_external_database_and_includes_committed_wal() {
    let root = tempfile::tempdir().unwrap();
    seed(root.path());
    let external = tempfile::tempdir().unwrap();
    let path = external.path().join("사용자 기록.sqlite");
    let db = cdr_store::schema::open_initialized(&path).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE upgrade_wal(value TEXT); INSERT INTO upgrade_wal VALUES('committed');").unwrap();
    let env_path = root.path().join(".env");
    let env = fs::read_to_string(&env_path).unwrap();
    fs::write(
        &env_path,
        format!("{env}CODEX_DISCORD_MIRROR_DB={}\r\n", path.display()),
    )
    .unwrap();
    let value = proof(&run(root.path(), "backup-upgrade"));
    assert_eq!(
        Path::new(value["database"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        path.canonicalize().unwrap()
    );
    let snapshot = rusqlite::Connection::open(value["backup"].as_str().unwrap()).unwrap();
    let found: String = snapshot
        .query_row("SELECT value FROM upgrade_wal", [], |r| r.get(0))
        .unwrap();
    assert_eq!(found, "committed");
    assert_eq!(
        fs::read_to_string(&env_path).unwrap(),
        format!("{env}CODEX_DISCORD_MIRROR_DB={}\r\n", path.display())
    );
    assert!(!root.path().join(".codex-discord-backups").exists());
}
#[test]
fn foreign_root_future_schema_and_invalid_agy_never_become_fresh_installs() {
    let root = tempfile::tempdir().unwrap();
    seed(root.path());
    fs::write(root.path().join("codex-agy.json"), "invalid").unwrap();
    assert!(!run(root.path(), "inspect-upgrade").status.success());
    fs::remove_file(root.path().join("codex-agy.json")).unwrap();
    let db = rusqlite::Connection::open(root.path().join("discord_mirror.sqlite")).unwrap();
    db.execute_batch("PRAGMA user_version=999").unwrap();
    drop(db);
    assert!(!run(root.path(), "backup-upgrade").status.success());
    assert!(!root.path().join(".codex-discord-backups").exists());
    let other = tempfile::tempdir().unwrap();
    let env = root.path().join(".env");
    let mut text = fs::read_to_string(&env).unwrap();
    writeln!(text, "CODEX_DISCORD_ROOT={}", other.path().display()).unwrap();
    fs::write(env, text).unwrap();
    assert!(!run(root.path(), "inspect-upgrade").status.success());
    assert_eq!(fs::read_dir(other.path()).unwrap().count(), 0);
}
