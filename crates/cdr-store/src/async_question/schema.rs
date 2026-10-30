use crate::Result;
use rusqlite::Connection;

const CANDIDATE_COLUMNS: [&str; 3] = [
    "candidate_generation",
    "candidate_execution_generation",
    "candidate_attempt_count",
];

pub(crate) fn migrate_schema(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS cdr_async_question_inbox (
        id TEXT PRIMARY KEY, runtime_id TEXT NOT NULL, generation INTEGER NOT NULL,
        thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, item_id TEXT NOT NULL,
        candidate_job_id TEXT NOT NULL, candidate_channel_id INTEGER NOT NULL,
        candidate_owner_id INTEGER NOT NULL, body TEXT NOT NULL,
        state TEXT NOT NULL DEFAULT 'waiting', created_at REAL NOT NULL);
        CREATE INDEX IF NOT EXISTS cdr_async_question_inbox_pending ON cdr_async_question_inbox(runtime_id,generation,state);")?;
    for column in CANDIDATE_COLUMNS {
        let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('cdr_async_question_inbox') WHERE name=?)", [column], |r| r.get(0))?;
        if !present {
            // NULL is retained for legacy candidates: migration fabricates no
            // cross-generation execution or observation authority.
            db.execute(
                &format!("ALTER TABLE cdr_async_question_inbox ADD COLUMN {column} INTEGER"),
                [],
            )?;
        }
    }
    db.execute_batch("CREATE TABLE IF NOT EXISTS cdr_async_questions (
        id TEXT PRIMARY KEY, runtime_id TEXT NOT NULL, generation INTEGER NOT NULL,
        thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, item_id TEXT NOT NULL,
        origin_job_id TEXT NOT NULL, channel_id INTEGER NOT NULL, owner_user_id INTEGER NOT NULL,
        body TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'observed', message_id TEXT, chosen INTEGER,
        dispatch_mode TEXT, reply_job_id TEXT, accepted_turn_id TEXT, error TEXT NOT NULL DEFAULT '',
        owner_confirmed INTEGER NOT NULL DEFAULT 0,
        created_at REAL NOT NULL, updated_at REAL NOT NULL);
        CREATE INDEX IF NOT EXISTS cdr_async_question_pending ON cdr_async_questions(runtime_id,state);
        CREATE UNIQUE INDEX IF NOT EXISTS cdr_async_question_reply_job ON cdr_async_questions(reply_job_id) WHERE reply_job_id IS NOT NULL;")?;
    if !has_preparation(db)? {
        db.execute(
            "ALTER TABLE cdr_async_questions ADD COLUMN preparation_json TEXT",
            [],
        )?;
    }
    if !has_answer_text(db)? {
        db.execute(
            "ALTER TABLE cdr_async_questions ADD COLUMN answer_text TEXT",
            [],
        )?;
    }
    Ok(())
}

pub(crate) fn schema_current(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT COUNT(*)=2 FROM sqlite_schema WHERE type='table' AND name IN ('cdr_async_questions','cdr_async_question_inbox')", [], |r|r.get::<_,bool>(0))?
        && has_preparation(db)?
        && has_answer_text(db)?
        && db.query_row("SELECT COUNT(*)=3 FROM pragma_table_info('cdr_async_question_inbox') WHERE name IN ('candidate_generation','candidate_execution_generation','candidate_attempt_count')", [], |r|r.get::<_,bool>(0))?)
}

fn has_answer_text(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('cdr_async_questions') WHERE name='answer_text')", [], |r|r.get(0))?)
}

fn has_preparation(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('cdr_async_questions') WHERE name='preparation_json')", [], |r|r.get(0))?)
}
