use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use rusqlite::{Connection, MAIN_DB, TransactionBehavior};
use uuid::Uuid;

use crate::{Result, StoreError};

mod catalog_cache;
pub(crate) mod checked_read;
#[cfg(debug_assertions)]
mod open_profile;
#[cfg(test)]
mod snapshot_tests;

pub const LATEST_STORE_SCHEMA_VERSION: i64 = 3;
pub const STORE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const BACKUP_DIRECTORY: &str = ".codex-discord-backups";

const V1_SCHEMA: [&str; 11] = [
    "CREATE TABLE IF NOT EXISTS mirror_projects (project_key TEXT PRIMARY KEY, project_name TEXT NOT NULL, discord_channel_id INTEGER NOT NULL, updated_at REAL NOT NULL)",
    "CREATE TABLE IF NOT EXISTS mirror_threads (codex_thread_id TEXT PRIMARY KEY, project_key TEXT NOT NULL, thread_title TEXT NOT NULL, discord_channel_id INTEGER NOT NULL, discord_thread_id INTEGER NOT NULL, updated_at REAL NOT NULL)",
    "CREATE TABLE IF NOT EXISTS session_mirror_details (codex_thread_id TEXT PRIMARY KEY, detail_mode TEXT NOT NULL CHECK(detail_mode IN ('send', 'all')))",
    "CREATE TABLE IF NOT EXISTS busy_choices (choice_id TEXT PRIMARY KEY, owner_user_id INTEGER NOT NULL, channel_id INTEGER NOT NULL, target_thread_id TEXT, prompt TEXT NOT NULL, allow_steer INTEGER NOT NULL, created_at REAL NOT NULL, expires_at REAL NOT NULL, claimed_at REAL)",
    "CREATE TABLE IF NOT EXISTS persistent_component_claims (claim_key TEXT PRIMARY KEY, created_at REAL NOT NULL, expires_at REAL NOT NULL)",
    "CREATE TABLE IF NOT EXISTS discord_processed_messages (message_id INTEGER PRIMARY KEY, seen_at REAL NOT NULL)",
    "CREATE TABLE IF NOT EXISTS codex_session_mirror_offsets (codex_thread_id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cursor INTEGER NOT NULL, updated_at REAL NOT NULL)",
    "CREATE TABLE IF NOT EXISTS codex_session_mirror_events (event_digest TEXT PRIMARY KEY, codex_thread_id TEXT NOT NULL, created_at REAL NOT NULL)",
    "CREATE TABLE IF NOT EXISTS codex_turn_queue (job_id TEXT PRIMARY KEY, target_thread_id TEXT NOT NULL, channel_id INTEGER NOT NULL, owner_user_id INTEGER, discord_message_id INTEGER, prompt TEXT NOT NULL, queued INTEGER NOT NULL, ack_sent INTEGER NOT NULL, state TEXT NOT NULL, attempt_count INTEGER NOT NULL, turn_id TEXT, baseline_turn_ids TEXT NOT NULL, last_error TEXT NOT NULL DEFAULT '', created_at REAL NOT NULL, updated_at REAL NOT NULL)",
    "CREATE UNIQUE INDEX IF NOT EXISTS codex_turn_queue_message_id ON codex_turn_queue(discord_message_id) WHERE discord_message_id IS NOT NULL",
    "CREATE INDEX IF NOT EXISTS codex_turn_queue_target_order ON codex_turn_queue(target_thread_id, created_at, job_id)",
];

#[cfg_attr(debug_assertions, track_caller)]
pub fn open_initialized(path: &Path) -> Result<Connection> {
    #[cfg(debug_assertions)]
    let mut profile = open_profile::Span::new(std::panic::Location::caller());
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(STORE_BUSY_TIMEOUT)?;
    #[cfg(debug_assertions)]
    profile.mark(open_profile::Phase::Connection);
    #[cfg(test)]
    checked_read::test_support::boundary(
        checked_read::test_support::Boundary::Opened,
        &connection,
    )?;
    let before = catalog_cache::signature(&connection)?;
    #[cfg(test)]
    checked_read::test_support::boundary(
        checked_read::test_support::Boundary::Catalog,
        &connection,
    )?;
    #[cfg(debug_assertions)]
    profile.mark(open_profile::Phase::Catalog);
    #[cfg(test)]
    snapshot_tests::boundary(snapshot_tests::Boundary::InitialCatalog);
    let cached = catalog_cache::contains(&before);
    #[cfg(debug_assertions)]
    profile.mark(open_profile::Phase::Cache);
    if !cached {
        #[cfg(debug_assertions)]
        profile.miss();
        let _ = initialize(&mut connection, path)?;
        #[cfg(test)]
        snapshot_tests::boundary(snapshot_tests::Boundary::Initialized);
        remember_verified_catalog(&mut connection)?;
    }
    Ok(connection)
}

fn remember_verified_catalog(connection: &mut Connection) -> Result<()> {
    // The exact key, supported version and every structural predicate must be
    // witnessed in one read snapshot. Never publish an earlier catalog key.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let signature = catalog_cache::signature(&transaction)?;
    #[cfg(test)]
    snapshot_tests::boundary(snapshot_tests::Boundary::FinalCatalog);
    let valid = schema_version(&transaction)? == LATEST_STORE_SCHEMA_VERSION
        && rust_extensions_current(&transaction)?;
    transaction.commit()?;
    if valid {
        catalog_cache::remember(signature);
    }
    Ok(())
}

pub fn initialize(connection: &mut Connection, path: &Path) -> Result<Option<PathBuf>> {
    let current = schema_version(connection)?;
    if current > LATEST_STORE_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedVersion {
            found: current,
            supported: LATEST_STORE_SCHEMA_VERSION,
        });
    }
    if current == LATEST_STORE_SCHEMA_VERSION && rust_extensions_current(connection)? {
        return Ok(None);
    }
    if !connection.is_autocommit() {
        return Err(StoreError::ActiveTransaction);
    }

    let backup = backup_before_migration(connection, path, current)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for version in (current + 1)..=LATEST_STORE_SCHEMA_VERSION {
        migrate(&transaction, version)?;
    }
    migrate_rust_extensions(&transaction)?;
    transaction.pragma_update(None, "user_version", LATEST_STORE_SCHEMA_VERSION)?;
    assert_integrity(&transaction)?;
    transaction.commit()?;
    Ok(backup)
}

pub fn schema_version(connection: &Connection) -> Result<i64> {
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

pub fn assert_integrity(connection: &Connection) -> Result<()> {
    let result: String =
        connection.pragma_query_value(None, "integrity_check", |row| row.get(0))?;
    if result.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(StoreError::Integrity(result))
    }
}

fn migrate(connection: &Connection, version: i64) -> Result<()> {
    match version {
        1 => {
            for statement in V1_SCHEMA {
                connection.execute(statement, [])?;
            }
        }
        2 => migrate_queue_generation(connection)?,
        3 => crate::archived_cleanup::migrate_schema(connection)?,
        _ => {
            return Err(StoreError::UnsupportedVersion {
                found: version,
                supported: LATEST_STORE_SCHEMA_VERSION,
            });
        }
    }
    Ok(())
}

fn migrate_rust_extensions(connection: &Connection) -> Result<()> {
    // Older partial stores may predate shared tables referenced by durable guards.
    // initialize() has already created the pre-migration backup and transaction.
    for statement in V1_SCHEMA {
        connection.execute_batch(statement)?;
    }
    crate::goal_progress::migrate_schema(connection)?;
    crate::observed_completion::migrate_schema(connection)?;
    crate::observed_final_answer::migrate_schema(connection)?;
    crate::async_question::migrate_schema(connection)?;
    crate::idle_release::migrate_schema(connection)?;
    crate::observation_gap::migrate_schema(connection)?;
    crate::mutation_attempt::migrate_schema(connection)?;
    crate::control_binding::migrate_schema(connection)?;
    crate::claims::migrate_schema(connection)?;
    crate::delivery_receipt::migrate_schema(connection)?;
    crate::commentary_outbox::migrate_schema(connection)?;
    crate::mirror::migrate_schema(connection)?;
    migrate_delivery_outbox(connection)?;
    migrate_goal_waiting(connection)?;
    migrate_queue_execution_generation(connection)?;
    migrate_turn_observation_generation(connection)?;
    crate::prompt_intake::migrate_schema(connection)?;
    crate::dead_generation::migrate_schema(connection)?;
    crate::ingress::migrate_schema(connection)?;
    crate::new_reply::migrate_schema(connection)?;
    crate::queue::migrate_cancellation_schema(connection)?;
    crate::room_cleanup::migrate_schema(connection)?;
    crate::mapping::creation::migrate_schema(connection)?;
    crate::mapping::container_creation::migrate_schema(connection)?;
    crate::archive_fence::migrate_schema(connection)?;
    crate::reserve_policy::migrate_schema(connection)?;
    crate::execution_hold::migrate_schema(connection)?;
    crate::final_recovery::migrate_schema(connection)?;
    crate::async_resolution::migrate_schema(connection)?;
    crate::async_resolution::publication::migrate_schema(connection)?;
    crate::async_resolution::abandonment::migrate_schema(connection)?;
    crate::async_resolution::admission_order::migrate_schema(connection)?;
    crate::mutation_attempt::response::migrate_schema(connection)?;
    crate::reserve_retirement::migrate_schema(connection)?;
    crate::archived_cleanup::migrate_schema(connection)?;
    crate::archived_cleanup::install_records(connection)
}

fn rust_extensions_current(connection: &Connection) -> Result<bool> {
    let has_outbox: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'codex_delivery_outbox')",
        [],
        |row| row.get(0),
    )?;
    if !has_outbox {
        return Ok(false);
    }
    let mut statement = connection.prepare("PRAGMA table_info(codex_turn_queue)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == "goal_waiting") {
        return Ok(false);
    }
    if !columns
        .iter()
        .any(|column| column == "execution_generation")
    {
        return Ok(false);
    }
    if !columns
        .iter()
        .any(|column| column == "turn_observation_generation")
    {
        return Ok(false);
    }
    Ok(crate::observed_completion::schema_current(connection)?
        && crate::observed_final_answer::schema_current(connection)?
        && crate::async_question::schema_current(connection)?
        && crate::idle_release::schema_current(connection)?
        && crate::observation_gap::schema_current(connection)?
        && crate::mutation_attempt::schema_current(connection)?
        && crate::goal_progress::schema_current(connection)?
        && crate::control_binding::schema_current(connection)?
        && crate::claims::schema_current(connection)?
        && crate::delivery_receipt::schema_current(connection)?
        && crate::commentary_outbox::schema_current(connection)?
        && crate::prompt_intake::schema_current(connection)?
        && crate::dead_generation::schema_current(connection)?
        && crate::ingress::schema_current(connection)?
        && crate::new_reply::schema_current(connection)?
        && crate::queue::cancellation_schema_current(connection)?
        && crate::room_cleanup::schema_current(connection)?
        && crate::mapping::creation::schema_current(connection)?
        && crate::mapping::container_creation::schema_current(connection)?
        && crate::mirror::schema_current(connection)?
        && crate::archive_fence::schema_current(connection)?
        && crate::reserve_policy::schema_current(connection)?
        && crate::execution_hold::schema_current(connection)?
        && crate::final_recovery::schema_current(connection)?
        && crate::async_resolution::schema_current(connection)?
        && crate::async_resolution::publication::schema_current(connection)?
        && crate::async_resolution::abandonment::schema_current(connection)?
        && crate::async_resolution::admission_order::schema_current(connection)?
        && crate::mutation_attempt::response::schema_current(connection)?
        && crate::reserve_retirement::schema_current(connection)?
        && crate::archived_cleanup::schema_current(connection)?
        && crate::archived_cleanup::records_current(connection)?)
}

fn migrate_goal_waiting(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(codex_turn_queue)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == "goal_waiting") {
        connection.execute(
            "ALTER TABLE codex_turn_queue ADD COLUMN goal_waiting INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

fn migrate_delivery_outbox(connection: &Connection) -> Result<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS codex_delivery_outbox (\
            delivery_id TEXT PRIMARY KEY, job_id TEXT NOT NULL UNIQUE, \
            target_thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, \
            channel_id INTEGER NOT NULL, content TEXT NOT NULL, \
            attempt_count INTEGER NOT NULL DEFAULT 0, last_error TEXT NOT NULL DEFAULT '', \
            created_at REAL NOT NULL, updated_at REAL NOT NULL\
        )",
        [],
    )?;
    Ok(())
}

fn migrate_queue_generation(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(codex_turn_queue)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns
        .iter()
        .any(|column| column == "app_server_generation")
    {
        connection.execute(
            "ALTER TABLE codex_turn_queue ADD COLUMN app_server_generation INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

fn migrate_queue_execution_generation(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(codex_turn_queue)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns
        .iter()
        .any(|column| column == "execution_generation")
    {
        connection.execute(
            "ALTER TABLE codex_turn_queue ADD COLUMN execution_generation INTEGER",
            [],
        )?;
    }
    Ok(())
}

// NULL is historical evidence, not permission to invent a new resident owner.
fn migrate_turn_observation_generation(connection: &Connection) -> Result<()> {
    let present: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('codex_turn_queue') WHERE name='turn_observation_generation')",
        [], |row| row.get(0),
    )?;
    if !present {
        connection.execute_batch(
            "ALTER TABLE codex_turn_queue ADD COLUMN turn_observation_generation INTEGER;",
        )?;
    }
    Ok(())
}

fn backup_before_migration(
    connection: &Connection,
    path: &Path,
    from_version: i64,
) -> Result<Option<PathBuf>> {
    if !path.exists() || path.metadata()?.len() == 0 {
        return Ok(None);
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let directory = parent.join(BACKUP_DIRECTORY);
    fs::create_dir_all(&directory)?;
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("store");
    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let unique = &Uuid::new_v4().simple().to_string()[..12];
    let backup = directory.join(format!(
        "{stem}.v{from_version}-to-v{LATEST_STORE_SCHEMA_VERSION}.{timestamp}.{unique}.sqlite"
    ));
    if let Err(error) = connection.backup(MAIN_DB, &backup, None) {
        match fs::remove_file(&backup) {
            Ok(()) => {}
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => {}
            Err(cleanup) => return Err(cleanup.into()),
        }
        return Err(error.into());
    }
    Ok(Some(backup))
}
