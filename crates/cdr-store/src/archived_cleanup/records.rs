//! Central attribution shared by snapshots, late evidence and runnable views.
//! Direct room/target keys are exact; dependent rows use explicit owner keys.
use super::Intent;
use crate::{Result, StoreError};
use rusqlite::{Connection, OptionalExtension, params};

struct Shape {
    table: String,
    key: String,
    snapshot: String,
    predicate: String,
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn shapes(db: &Connection, row: &str, scope: &str) -> Result<Vec<Shape>> {
    let tables=db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'cdr_archived_room_%'")?
        .query_map([],|r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result = Vec::new();
    for table in tables {
        // These optional legacy routing tables are lazily created by queue
        // compatibility code; they are metadata, never runnable work.
        if matches!(
            table.as_str(),
            "codex_thread_fork_handoffs"
                | "codex_retired_fork_handoffs"
                | "codex_app_server_managed_targets"
        ) {
            continue;
        }
        let columns = db
            .prepare("SELECT name,pk FROM pragma_table_info(?) ORDER BY cid")?
            .query_map([&table], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut terms = Vec::new();
        for (column, _) in &columns {
            if matches!(
                column.as_str(),
                "target_thread_id" | "codex_thread_id" | "thread_id" | "thread"
            ) {
                terms.push(format!("({scope}.phase NOT IN ('mapping_retired','aborted_source_changed') AND {row}.{}={scope}.target_thread_id)",quote(column)));
            }
            if matches!(
                column.as_str(),
                "channel_id" | "channel" | "candidate_channel_id"
            ) {
                terms.push(format!(
                    "({scope}.phase<>'aborted_source_changed' AND {row}.{}={scope}.channel_id)",
                    quote(column)
                ));
            }
        }
        if table == "codex_delivery_receipts" {
            terms.push(format!("({scope}.phase<>'aborted_source_changed' AND CASE WHEN json_valid({row}.receipt_key) THEN json_type({row}.receipt_key,'$[0]')='integer' AND json_extract({row}.receipt_key,'$[0]')={scope}.channel_id ELSE 0 END)"));
        }
        if table == "codex_new_first_replies" {
            terms.push(format!("EXISTS(SELECT 1 FROM discord_ingress_journal p WHERE p.ingress_id={row}.ingress_id AND (({scope}.phase NOT IN ('mapping_retired','aborted_source_changed') AND p.target_thread_id={scope}.target_thread_id) OR ({scope}.phase<>'aborted_source_changed' AND p.channel_id={scope}.channel_id)))"));
        }
        // These dependent families have no independent room authority.
        for (child, foreign, parent, primary) in [
            (
                "cdr_final_recovery",
                "delivery_id",
                "codex_delivery_outbox",
                "delivery_id",
            ),
            (
                "cdr_recovery_abandonment_deliveries",
                "proposal_id",
                "cdr_recovery_abandonment_proposals",
                "id",
            ),
            (
                "cdr_recovery_abandonment_decisions",
                "proposal_id",
                "cdr_recovery_abandonment_proposals",
                "id",
            ),
            (
                "cdr_recovery_publication_deliveries",
                "proposal_id",
                "cdr_recovery_publication_proposals",
                "id",
            ),
            (
                "cdr_recovery_publication_decisions",
                "proposal_id",
                "cdr_recovery_publication_proposals",
                "id",
            ),
        ] {
            if table == child {
                let target = "target_thread_id";
                terms.push(format!("EXISTS(SELECT 1 FROM {parent} p WHERE p.{primary}={row}.{foreign} AND (({scope}.phase NOT IN ('mapping_retired','aborted_source_changed') AND p.{target}={scope}.target_thread_id) OR ({scope}.phase<>'aborted_source_changed' AND p.channel_id={scope}.channel_id)))"));
            }
        }
        if columns.iter().any(|(c, _)| c == "question_id")
            && table != "cdr_async_execution_obligations"
        {
            terms.push(format!("EXISTS(SELECT 1 FROM cdr_async_questions p WHERE p.id={row}.question_id AND (({scope}.phase NOT IN ('mapping_retired','aborted_source_changed') AND p.thread_id={scope}.target_thread_id) OR ({scope}.phase<>'aborted_source_changed' AND p.channel_id={scope}.channel_id)))"));
        }
        if terms.is_empty() {
            continue;
        }
        let (key, snapshot) = identity(&columns, row);
        result.push(Shape {
            table,
            key,
            snapshot,
            predicate: terms.join(" OR "),
        });
    }
    Ok(result)
}

fn identity(columns: &[(String, i64)], row: &str) -> (String, String) {
    let mut pk = columns.iter().filter(|(_, pk)| *pk > 0).collect::<Vec<_>>();
    pk.sort_by_key(|(_, pk)| pk);
    let key = if pk.is_empty() {
        format!("json_array({row}.rowid)")
    } else {
        format!(
            "json_array({})",
            pk.iter()
                .map(|(name, _)| format!("{row}.{}", quote(name)))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let snapshot=format!("json_object({})",columns.iter().map(|(name,_)|format!("'{}',CASE WHEN typeof({row}.{q})='blob' THEN json_object('blob_hex',hex({row}.{q})) ELSE {row}.{q} END",name.replace('\'',"''"),q=quote(name))).collect::<Vec<_>>().join(","));
    (key, snapshot)
}

pub(super) fn capture(db: &Connection, intent: &Intent) -> Result<()> {
    for shape in shapes(db, "r", "d")? {
        db.execute(&format!("INSERT OR IGNORE INTO cdr_archived_room_records(operation_id,table_name,row_key,snapshot_json) SELECT d.operation_id,?2,{},{} FROM {} r JOIN cdr_archived_room_dispositions d ON d.operation_id=?1 WHERE ({})",shape.key,shape.snapshot,quote(&shape.table),shape.predicate), params![intent.operation_id,shape.table])?;
    }
    Ok(())
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    for (name, sql) in definitions(db)? {
        if let Some(actual) = definition(db, &name)? {
            if normalize(&actual) != normalize(&sql) {
                return Err(StoreError::Integrity(format!(
                    "archived routing guard {name} is malformed"
                )));
            }
        } else {
            db.execute_batch(&sql)?;
        }
    }
    Ok(())
}

pub(crate) fn schema_current(db: &Connection) -> Result<bool> {
    for (name, sql) in definitions(db)? {
        if definition(db, &name)?.is_none_or(|actual| normalize(&actual) != normalize(&sql)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn definition(db: &Connection, name: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("SELECT sql FROM sqlite_schema WHERE name=?", [name], |r| {
            r.get(0)
        })
        .optional()?)
}
fn normalize(sql: &str) -> String {
    sql.replace(" IF NOT EXISTS", "")
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn definitions(db: &Connection) -> Result<Vec<(String, String)>> {
    let mut result = Vec::new();
    for shape in shapes(db, "NEW", "d")? {
        let table = quote(&shape.table);
        let name = format!("cdr_retired_capture_{}", shape.table);
        let trigger = quote(&name);
        let sql=format!("CREATE TRIGGER IF NOT EXISTS {trigger} AFTER INSERT ON {table} BEGIN
            INSERT OR IGNORE INTO cdr_archived_room_records(operation_id,table_name,row_key,snapshot_json)
            SELECT d.operation_id,'{}',{},{} FROM cdr_archived_room_dispositions d WHERE ({}); END;",shape.table,shape.key,shape.snapshot,shape.predicate);
        result.push((name, sql));
        // REPLACE can bypass DELETE triggers when recursive_triggers is off.
        // Reject a replacement of preserved work before SQLite deletes it.
        if preserve_original(&shape.table) {
            let name = format!("cdr_retired_no_replace_{}", shape.table);
            result.push((name.clone(),format!("CREATE TRIGGER IF NOT EXISTS {} BEFORE INSERT ON {table} WHEN EXISTS(SELECT 1 FROM cdr_archived_room_records e WHERE e.table_name='{}' AND e.row_key={}) BEGIN SELECT RAISE(IGNORE); END;",quote(&name),shape.table,shape.key)));
        }
    }
    for shape in shapes(db, "r", "d")? {
        // Views remove retired work from schedulers without rewriting its state.
        // Both recorded identities and late direct attribution remain excluded.
        let name = format!("cdr_live_{}", shape.table);
        let view = quote(&name);
        result.push((name,format!("CREATE VIEW IF NOT EXISTS {view} AS SELECT r.rowid AS rowid,r.* FROM {} r WHERE NOT EXISTS(SELECT 1 FROM cdr_archived_room_dispositions d WHERE ({})) AND NOT EXISTS(SELECT 1 FROM cdr_archived_room_records e WHERE e.table_name='{}' AND e.row_key={});",quote(&shape.table),shape.predicate,shape.table,shape.key)));
    }
    // Pruning must not erase preserved payloads. Mapping/cursor/configuration
    // rows are not work and remain governed by their own compare-and-swap.
    for shape in shapes(db, "OLD", "d")?
        .into_iter()
        .filter(|s| preserve_original(&s.table))
    {
        let name = format!("cdr_retired_preserve_{}", shape.table);
        result.push((name.clone(),format!("CREATE TRIGGER IF NOT EXISTS {} BEFORE DELETE ON {} WHEN EXISTS(SELECT 1 FROM cdr_archived_room_records e WHERE e.table_name='{}' AND e.row_key={}) BEGIN SELECT RAISE(IGNORE); END;",quote(&name),quote(&shape.table),shape.table,shape.key)));
        let name = format!("cdr_retired_freeze_{}", shape.table);
        // Already-admitted effects may record a truthful result while prepared.
        // Once drained (or aborted), stale recovery/pruning cannot rewrite it.
        result.push((name.clone(),format!("CREATE TRIGGER IF NOT EXISTS {} BEFORE UPDATE ON {} WHEN EXISTS(SELECT 1 FROM cdr_archived_room_records e JOIN cdr_archived_room_dispositions d ON d.operation_id=e.operation_id WHERE e.table_name='{}' AND e.row_key={} AND d.phase<>'prepared') {} BEGIN SELECT RAISE(IGNORE); END;",quote(&name),quote(&shape.table),shape.table,shape.key,if shape.table=="discord_ingress_journal" {"AND NOT(NEW.state='held' AND NEW.phase='cleanup_retired' AND NEW.payload_json=OLD.payload_json)"} else {""})));
    }
    Ok(result)
}

fn preserve_original(table: &str) -> bool {
    matches!(
        table,
        "discord_ingress_journal"
            | "discord_ingress_owner_receipts"
            | "busy_choices"
            | "codex_turn_queue"
            | "codex_prompt_intakes"
            | "codex_delivery_outbox"
            | "codex_delivery_receipts"
            | "codex_commentary_outbox"
            | "codex_goal_progress"
            | "codex_new_first_replies"
            | "cdr_idle_release"
            | "codex_mutation_attempts"
            | "cdr_server_responses"
            | "codex_dead_generation_holds"
            | "cdr_execution_holds"
            | "cdr_stop_controls"
    ) || table.starts_with("cdr_async_")
        || table.starts_with("cdr_recovery_")
}

pub(super) fn check_conflicts(db: &Connection, channel: i64, target: &str) -> Result<()> {
    for shape in shapes(db, "r", "d")? {
        let columns = db
            .prepare("SELECT name FROM pragma_table_info(?)")?
            .query_map([&shape.table], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let targets = columns
            .iter()
            .filter(|c| {
                matches!(
                    c.as_str(),
                    "target_thread_id" | "codex_thread_id" | "thread_id" | "thread"
                )
            })
            .collect::<Vec<_>>();
        let channels = columns
            .iter()
            .filter(|c| {
                matches!(
                    c.as_str(),
                    "channel_id" | "channel" | "candidate_channel_id"
                )
            })
            .collect::<Vec<_>>();
        for t in &targets {
            for c in &channels {
                let sql = format!(
                    "SELECT EXISTS(SELECT 1 FROM {} r WHERE (r.{c}=?1 AND r.{t} IS NOT NULL AND r.{t}<>'' AND r.{t}<>?2) OR (r.{t}=?2 AND EXISTS(SELECT 1 FROM mirror_threads m WHERE m.discord_thread_id=r.{c} AND m.codex_thread_id<>?2)))",
                    quote(&shape.table),
                    c = quote(c),
                    t = quote(t)
                );
                if db.query_row(&sql, params![channel, target], |r| r.get::<_, bool>(0))? {
                    return Err(StoreError::Integrity(format!(
                        "conflicting archived record ownership in {}; room preserved",
                        shape.table
                    )));
                }
            }
        }
    }
    Ok(())
}
