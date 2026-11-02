use cdr_store::schema;
use serde_json::Value;
use std::{ffi::OsString, path::Path};

#[path = "../../../tests/support/async_orphan_fixture.rs"]
mod fixture;

async fn check(path: &Path) -> Result<Value, String> {
    let output = Box::pin(cdr_runtime::admin::run([
        OsString::from("check-recovery-compatibility"),
        OsString::from("--database"),
        path.as_os_str().to_owned(),
    ]))
    .await?;
    serde_json::from_str(&output).map_err(|error| error.to_string())
}

#[tokio::test]
async fn free_text_capability_is_supported_but_newer_formats_remain_held() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.sqlite");
    let db = schema::open_initialized(&path).unwrap();
    db.execute(
        "INSERT INTO cdr_runtime_capability_requirements(component,format_version) VALUES (?,?)",
        rusqlite::params![
            cdr_store::async_question::FREE_TEXT_COMPONENT,
            cdr_store::async_question::FREE_TEXT_FORMAT_VERSION
        ],
    )
    .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    assert_eq!(check(&path).await.unwrap()["compatible"], true);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let db = schema::open_initialized(&path).unwrap();
    db.execute(
        "UPDATE cdr_runtime_capability_requirements SET format_version=2 WHERE component=?",
        [cdr_store::async_question::FREE_TEXT_COMPONENT],
    )
    .unwrap();
    drop(db);
    assert!(check(&path).await.unwrap_err().contains("does not support"));
}

#[tokio::test]
async fn missing_and_unsupported_databases_are_never_created_or_migrated() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing.sqlite");
    assert!(check(&missing).await.is_err());
    assert!(!missing.exists());
    for version in [0, 1, 999] {
        let path = temp.path().join(format!("schema-{version}.sqlite"));
        let db = rusqlite::Connection::open(&path).unwrap();
        db.pragma_update(None, "user_version", version).unwrap();
        drop(db);
        let before = std::fs::read(&path).unwrap();
        assert!(check(&path).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[tokio::test]
async fn schema_two_upgrade_probe_is_read_only_and_keeps_capability_checks() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("legacy.sqlite");
    fixture::dispatching(&path, "resident");
    let db = rusqlite::Connection::open(&path).unwrap();
    let objects = db.prepare("SELECT type,name FROM sqlite_schema WHERE type IN ('trigger','view') AND (name LIKE 'cdr_archived_room_%' OR name LIKE 'cdr_retired_%' OR name LIKE 'cdr_live_%')").unwrap()
        .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap();
    for (kind, name) in objects {
        db.execute_batch(&format!("DROP {kind} \"{name}\""))
            .unwrap();
    }
    db.execute_batch(
        "DROP TABLE cdr_archived_room_records;
        DROP TABLE cdr_archived_room_history; DROP TABLE cdr_archived_room_dispositions;
        DROP TABLE cdr_archived_room_receipt_confirmations; PRAGMA user_version=2;",
    )
    .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    let result = check(&path).await.unwrap();
    assert_eq!(result["compatible"], true);
    assert_eq!(result["store_schema"], 2);
    assert_eq!(result["target_store_schema"], 3);
    assert_eq!(result["migration_required"], true);
    assert_eq!(result["admission_authorized"], false);
    assert_eq!(result["recovery_authorized"], false);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "INSERT INTO cdr_runtime_capability_requirements VALUES('unknown-future-component',1)",
        [],
    )
    .unwrap();
    drop(db);
    let unsupported = std::fs::read(&path).unwrap();
    assert!(check(&path).await.unwrap_err().contains("does not support"));
    assert_eq!(std::fs::read(&path).unwrap(), unsupported);
}

#[tokio::test]
async fn compatible_format_is_read_only_and_never_authorizes_execution() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.sqlite");
    fixture::dispatching(&path, "resident");
    fixture::pending(&path, "next", "thread-b", 1);
    let before = std::fs::read(&path).unwrap();
    let result = check(&path).await.unwrap();
    assert_eq!(result["required_async_resolution_format"], 1);
    assert_eq!(result["supported_async_resolution_format"], 1);
    assert_eq!(result["compatible"], true);
    assert_eq!(result["read_only"], true);
    assert_eq!(result["admission_authorized"], false);
    assert_eq!(result["recovery_authorized"], false);
    assert_eq!(result["special_dispatch_supported"], false);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(cdr_store::async_resolution::admission_held(&path, "thread-b").unwrap());
}

#[tokio::test]
async fn persisted_newer_requirement_cannot_be_downgraded_or_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.sqlite");
    fixture::dispatching(&path, "resident");
    let db = schema::open_initialized(&path).unwrap();
    db.execute("UPDATE cdr_runtime_capability_requirements SET format_version=2 WHERE component='async_resolution'",[]).unwrap();
    assert!(db.execute("UPDATE cdr_runtime_capability_requirements SET format_version=1 WHERE component='async_resolution'",[]).is_err());
    assert!(
        db.execute("DELETE FROM cdr_runtime_capability_requirements", [])
            .is_err()
    );
    drop(db);
    let before = std::fs::read(&path).unwrap();
    assert!(check(&path).await.unwrap_err().contains("does not support"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn absent_requirement_with_existing_evidence_fails_closed_without_backfill() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.sqlite");
    fixture::dispatching(&path, "resident");
    let db = schema::open_initialized(&path).unwrap();
    // Isolate the legacy execution-evidence contract. The newer policy ledger
    // independently requires the same registry and is covered below.
    db.execute_batch(
        "DROP TABLE cdr_async_recovery_policies;
        DROP TABLE cdr_runtime_capability_requirements",
    )
    .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    assert!(
        check(&path)
            .await
            .unwrap_err()
            .contains("without its persisted capability")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn policy_without_registry_is_rejected_before_execution_evidence_inspection() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.sqlite");
    fixture::dispatching(&path, "resident");
    let db = schema::open_initialized(&path).unwrap();
    db.execute_batch("DROP TABLE cdr_runtime_capability_requirements")
        .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    let error = check(&path).await.unwrap_err();
    assert!(
        error.contains("recovery policy schema has no persisted capability"),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
