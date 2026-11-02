use super::*;

fn legacy(db: &Connection, entry: &Entry) -> Option<Entry> {
    db.query_row(
        &format!("{} SELECT stamp,ordinal,sort_id,id,target,turn,channel,bytes FROM candidates WHERE id=?3 AND target=?4 AND turn=?5", Source::Final.query()),
        params!["runtime", 1, entry.id, entry.target, entry.turn],
        |row| Entry::read(Source::Final, row),
    ).optional().unwrap()
}

#[test]
fn channel_current_preserves_heads_and_reduces_cross_channel_query_work() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE codex_delivery_outbox(delivery_id TEXT PRIMARY KEY,job_id TEXT,target_thread_id TEXT,turn_id TEXT,channel_id INTEGER,content TEXT,created_at REAL);
        CREATE TABLE codex_delivery_receipts(receipt_key TEXT,message_id TEXT,retryable INTEGER,blocked_reason TEXT);
        CREATE TABLE discord_ingress_journal(owner_kind TEXT,owner_id TEXT,kind TEXT,confirmation_delivered INTEGER,created_at REAL,ingress_id TEXT);
        CREATE TABLE cdr_final_recovery(delivery_id TEXT);
        CREATE TABLE codex_commentary_outbox(job_id TEXT,target_thread_id TEXT,turn_id TEXT,channel_id INTEGER);").unwrap();
    crate::archived_cleanup::migrate_schema(&db).unwrap();
    crate::archived_cleanup::install_records(&db).unwrap();
    for n in 0..130 {
        let id = format!("job-{n:03}");
        db.execute(
            "INSERT INTO codex_delivery_outbox VALUES(?1,?1,?1,'turn',?2,'body',1)",
            params![id, n + 100],
        )
        .unwrap();
    }
    let entries = db
        .prepare(&format!(
            "{} SELECT stamp,ordinal,sort_id,id,target,turn,channel,bytes FROM candidates",
            Source::Final.query()
        ))
        .unwrap()
        .query_map(params!["runtime", 1], |row| Entry::read(Source::Final, row))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut work = [0_i64; 2];
    for entry in &entries {
        assert_eq!(
            current(&db, entry, "runtime", 1).unwrap(),
            legacy(&db, entry)
        );
        for (index, scoped) in [false, true].into_iter().enumerate() {
            let mut statement = db.prepare(&format!("{} SELECT stamp,ordinal,sort_id,id,target,turn,channel,bytes FROM candidates WHERE id=?3 AND target=?4 AND turn=?5", Source::Final.query_for_channel(scoped))).unwrap();
            let row = if scoped {
                statement.query_row(
                    params![
                        "runtime",
                        1,
                        entry.id,
                        entry.target,
                        entry.turn,
                        entry.channel
                    ],
                    |row| Entry::read(Source::Final, row),
                )
            } else {
                statement.query_row(
                    params!["runtime", 1, entry.id, entry.target, entry.turn],
                    |row| Entry::read(Source::Final, row),
                )
            }
            .unwrap();
            assert_eq!(&row, entry);
            work[index] += i64::from(statement.get_status(rusqlite::StatementStatus::VmStep));
        }
    }
    eprintln!(
        "final_current_130_heads legacy_vm_steps={} scoped_vm_steps={}",
        work[0], work[1]
    );
    assert!(
        work[1] < work[0],
        "channel scoping must remove cross-channel ranking work"
    );
    assert_stale_and_held_heads(&db, &entries[0]);
}

fn assert_stale_and_held_heads(db: &Connection, entry: &Entry) {
    let equivalent = || {
        let old = legacy(db, entry).filter(|actual| actual == entry);
        let new = current(db, entry, "runtime", 1)
            .unwrap()
            .filter(|actual| actual == entry);
        assert_eq!(new, old);
        new
    };
    // Same-time tie, earlier unknown/blocked/oversized head, stale channel,
    // changed identity and deletion all retain the public load rejection.
    db.execute(
        "INSERT INTO codex_delivery_outbox VALUES('aaa','aaa','earlier','turn',?1,'earlier',1)",
        [entry.channel],
    )
    .unwrap();
    assert!(equivalent().is_none());
    for blocked in [None, Some("blocked")] {
        db.execute("DELETE FROM codex_delivery_receipts", [])
            .unwrap();
        let key = serde_json::to_string(&(entry.channel, "completion/v1", "aaa", 0)).unwrap();
        db.execute(
            "INSERT INTO codex_delivery_receipts VALUES(?1,NULL,0,?2)",
            params![key, blocked],
        )
        .unwrap();
        assert!(equivalent().is_none());
    }
    db.execute(
        "UPDATE codex_delivery_outbox SET content=?1 WHERE delivery_id='aaa'",
        ["x".repeat(MAX_PAYLOAD_BYTES + 1)],
    )
    .unwrap();
    assert!(equivalent().is_none());
    db.execute(
        "DELETE FROM codex_delivery_outbox WHERE delivery_id='aaa'",
        [],
    )
    .unwrap();
    assert!(equivalent().is_some());
    db.execute(
        "UPDATE codex_delivery_outbox SET channel_id=9999 WHERE delivery_id=?1",
        [&entry.id],
    )
    .unwrap();
    assert!(equivalent().is_none());
    db.execute("UPDATE codex_delivery_outbox SET channel_id=?1,target_thread_id='changed' WHERE delivery_id=?2", params![entry.channel, entry.id]).unwrap();
    assert!(equivalent().is_none());
    db.execute(
        "DELETE FROM codex_delivery_outbox WHERE delivery_id=?1",
        [&entry.id],
    )
    .unwrap();
    assert!(equivalent().is_none());
}
