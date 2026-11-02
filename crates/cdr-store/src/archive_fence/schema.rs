use crate::{Result, StoreError};
use rusqlite::{Connection, OptionalExtension};

const VIEW: &str = include_str!("inspections.sql");
const PREVIOUS_VIEW: &str = include_str!("inspections_before_explicit_archive.sql");
const LEGACY_VIEW: &str = include_str!("inspections_legacy.sql");

// Only known earlier inspection views may be upgraded. Unknown definitions
// remain untouched. The initializer owns the backup and IMMEDIATE transaction.
pub(crate) fn migrate_schema(db: &Connection) -> Result<()> {
    match installed_view(db)? {
        None => db.execute_batch(VIEW)?,
        Some((kind, sql)) if kind == "view" && same_definition(&sql, VIEW) => {}
        Some((kind, sql))
            if kind == "view"
                && (same_definition(&sql, LEGACY_VIEW) || same_definition(&sql, PREVIOUS_VIEW)) =>
        {
            if db.is_autocommit() {
                return Err(StoreError::ActiveTransaction);
            }
            db.execute_batch("DROP VIEW cdr_archive_inspections_v1;")?;
            db.execute_batch(VIEW)?;
        }
        Some(_) => {
            return Err(StoreError::Integrity(
                "archive inspection definition is unknown; no automatic replacement".into(),
            ));
        }
    }
    db.execute_batch(include_str!("schema.sql"))?;
    Ok(())
}

pub(crate) fn schema_current(db: &Connection) -> Result<bool> {
    let complete: bool = db.query_row(
        "SELECT COUNT(*)=9 FROM sqlite_schema WHERE
         (type='table' AND name='codex_archive_fences') OR
         (type='view' AND name='cdr_archive_inspections_v1') OR
         (type='trigger' AND name IN ('cdr_archive_admission_v1','cdr_archive_execution_v1',
          'cdr_archive_held_v1','cdr_archive_queue_insert_v1','cdr_archive_queue_update_v1',
          'cdr_archive_intake_insert_v1','cdr_archive_intake_update_v1'))",
        [],
        |row| row.get(0),
    )?;
    Ok(complete
        && installed_view(db)?
            .is_some_and(|(kind, sql)| kind == "view" && same_definition(&sql, VIEW)))
}

fn installed_view(db: &Connection) -> Result<Option<(String, String)>> {
    Ok(db
        .query_row(
            "SELECT type,sql FROM sqlite_schema WHERE name='cdr_archive_inspections_v1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

fn same_definition(actual: &str, expected: &str) -> bool {
    // SQLite removes IF NOT EXISTS and the trailing terminator. Do not fold
    // case or whitespace within SQL literals, identifiers or the view body.
    fn body(sql: &str) -> &str {
        let sql = sql.trim().trim_end_matches(';').trim_end();
        sql.strip_prefix("CREATE VIEW IF NOT EXISTS ")
            .or_else(|| sql.strip_prefix("CREATE VIEW "))
            .unwrap_or(sql)
    }
    body(actual) == body(expected)
}
