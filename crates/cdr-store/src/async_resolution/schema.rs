use crate::Result;
use rusqlite::Connection;

pub(super) fn claim_json(q: &str) -> String {
    let choice = format!("json_object('id',{q}.id,'runtime_id',{q}.runtime_id,'generation',{q}.generation,
        'thread_id',{q}.thread_id,'turn_id',{q}.turn_id,'item_id',{q}.item_id,
        'origin_job_id',{q}.origin_job_id,'channel_id',{q}.channel_id,'owner_user_id',{q}.owner_user_id,
        'body',{q}.body,'chosen',{q}.chosen,'message_id',{q}.message_id,'dispatch_mode',{q}.dispatch_mode)");
    format!(
        "CASE WHEN {q}.answer_text IS NULL THEN {choice} ELSE json_set({choice},'$.answer_text',{q}.answer_text) END"
    )
}

pub(super) fn owner_json(j: &str) -> String {
    format!("CASE WHEN {j}.job_id IS NULL THEN NULL ELSE json_object('job_id',{j}.job_id,
        'target_thread_id',{j}.target_thread_id,'channel_id',{j}.channel_id,'owner_user_id',{j}.owner_user_id,
        'app_server_generation',{j}.app_server_generation,'execution_generation',{j}.execution_generation,
        'turn_observation_generation',{j}.turn_observation_generation,'attempt_count',{j}.attempt_count,
        'turn_id',{j}.turn_id,'created_at',{j}.created_at,'baseline_turn_ids',{j}.baseline_turn_ids) END")
}

fn capture(predicate: &str) -> String {
    let claim = claim_json("q");
    let owner = owner_json("j");
    let policy = super::policy::capture_case("q.thread_id");
    let bounded = format!(
        "COALESCE(length(CAST(q.preparation_json AS BLOB)),0)<=131072
        AND length(CAST(({claim}) AS BLOB))<=131072
        AND COALESCE(length(CAST(({owner}) AS BLOB)),0)<=131072"
    );
    format!(
        "INSERT INTO cdr_async_execution_obligations
        (question_id,thread_id,origin_job_id,turn_id,channel_id,format_version,revision,
         answer_state,execution_state,admission_state,policy,original_seal,claim_json,
         owner_json,original_error,receipt_turn,created_at,updated_at)
        SELECT q.id,q.thread_id,q.origin_job_id,q.turn_id,q.channel_id,1,0,
        CASE WHEN q.state='submitted' AND q.accepted_turn_id=q.turn_id
            THEN 'exact_receipt_confirmed' ELSE 'unresolved' END,
        'unresolved','held',{policy},
        CASE WHEN {bounded} THEN q.preparation_json ELSE NULL END,
        CASE WHEN length(CAST(({claim}) AS BLOB))<=131072 THEN {claim}
             ELSE json_object('oversized_legacy_evidence',1,'source_question_id',q.id) END,
        CASE WHEN COALESCE(length(CAST(({owner}) AS BLOB)),0)<=131072 THEN {owner} ELSE NULL END,
        q.error,q.accepted_turn_id,q.created_at,q.updated_at
        FROM cdr_async_questions q LEFT JOIN codex_turn_queue j ON j.job_id=q.origin_job_id
        WHERE q.dispatch_mode='steer' AND q.state IN ('dispatching','submitted') AND ({predicate})
        AND NOT EXISTS(SELECT 1 FROM cdr_async_execution_obligations o WHERE o.question_id=q.id);"
    )
}

pub(crate) fn migrate_schema(db: &Connection) -> Result<()> {
    db.execute_batch("SAVEPOINT cdr_async_resolution_schema")?;
    let result = migrate_inner(db);
    if result.is_ok() {
        db.execute_batch("RELEASE cdr_async_resolution_schema")?;
    } else {
        db.execute_batch(
            "ROLLBACK TO cdr_async_resolution_schema; RELEASE cdr_async_resolution_schema",
        )?;
    }
    result
}

fn migrate_inner(db: &Connection) -> Result<()> {
    db.execute_batch(
        "DROP TRIGGER IF EXISTS cdr_async_obligation_question;
        DROP TRIGGER IF EXISTS cdr_async_obligation_queue_delete;
        DROP TRIGGER IF EXISTS cdr_async_obligation_no_forget;
        DROP TRIGGER IF EXISTS cdr_async_obligation_attempt;
        DROP TRIGGER IF EXISTS cdr_async_obligation_retention;",
    )?;
    db.execute_batch(include_str!("schema.sql"))?;
    // A successful SQL batch may still have lost INSERT to RAISE(IGNORE).
    // Persist the fallback's compatibility requirement before any commit;
    // retain a higher requirement for the compatibility gate to reject.
    db.query_row(
        "SELECT format_version FROM cdr_runtime_capability_requirements
         WHERE component=? AND format_version>=?",
        rusqlite::params![
            super::RECOVERY_POLICY_COMPONENT,
            super::RECOVERY_POLICY_FORMAT_VERSION
        ],
        |row| row.get::<_, i64>(0),
    )?;
    let on_question = capture(
        "q.id=NEW.id AND q.preparation_json IS NOT NULL AND (
        (OLD.state='open' AND NEW.state='dispatching') OR
        (OLD.state='dispatching' AND NEW.state='submitted') OR
        (NEW.state='dispatching' AND OLD.preparation_json IS NULL))",
    );
    let on_delete = capture("q.origin_job_id=OLD.job_id");
    db.execute_batch(&format!(
        "
        CREATE TRIGGER IF NOT EXISTS cdr_async_obligation_question
        AFTER UPDATE ON cdr_async_questions
        WHEN NEW.dispatch_mode='steer' AND NEW.state IN ('dispatching','submitted')
        BEGIN
            {on_question}
            UPDATE cdr_async_execution_obligations
            SET answer_state='exact_receipt_confirmed',receipt_turn=NEW.accepted_turn_id,
                updated_at=NEW.updated_at
            WHERE question_id=NEW.id AND answer_state='unresolved' AND NEW.state='submitted'
                AND NEW.accepted_turn_id=turn_id AND original_seal=NEW.preparation_json;
            UPDATE cdr_async_execution_obligations SET original_error=NEW.error
            WHERE question_id=NEW.id AND original_error='' AND NEW.error!='';
        END;
        CREATE TRIGGER IF NOT EXISTS cdr_async_obligation_queue_delete
        BEFORE DELETE ON codex_turn_queue BEGIN {on_delete} END;
    "
    ))?;
    // Only unresolved legacy dispatches are backfilled. Historical submitted
    // questions are not retroactively treated as unresolved ordinary executions.
    db.execute_batch(&capture("q.state='dispatching'"))?;
    db.execute_batch(
        "INSERT OR IGNORE INTO cdr_runtime_capability_requirements(component,format_version)
        SELECT 'async_resolution',1 WHERE EXISTS(SELECT 1 FROM cdr_async_execution_obligations);",
    )?;
    Ok(())
}

pub(crate) fn schema_current(db: &Connection) -> Result<bool> {
    let present:bool=db.query_row(
        "SELECT COUNT(*)=30 FROM sqlite_schema WHERE name IN (
         'cdr_async_execution_obligations','cdr_async_obligation_target',
         'cdr_async_obligation_question','cdr_async_obligation_queue_delete',
         'cdr_async_obligation_claim_immutable','cdr_async_obligation_no_forget',
         'cdr_async_obligation_attempt','cdr_async_obligation_retention',
         'cdr_async_terminal_settlements','cdr_async_unsettled_obligations',
         'cdr_async_settlement_insert','cdr_async_settlement_immutable','cdr_async_settlement_no_delete',
         'cdr_async_execution_handoffs','cdr_async_handoff_immutable','cdr_async_handoff_no_delete',
         'cdr_runtime_capability_requirements','cdr_capability_no_downgrade',
         'cdr_capability_no_delete','cdr_async_require_capability',
         'cdr_async_terminal_candidates','cdr_async_candidate_immutable','cdr_async_candidate_no_delete',
         'cdr_async_source_no_delete','cdr_async_source_seal_immutable','cdr_async_uncopied_origin_guard',
         'cdr_async_recovery_policies','cdr_async_recovery_policy_immutable',
         'cdr_async_recovery_policy_no_delete','cdr_async_recovery_policy_capability')", [], |r| r.get(0),
    )?;
    if !present {
        return Ok(false);
    }
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='trigger' AND name='cdr_async_obligation_question' AND instr(sql,'answer_text')>0)", [], |r|r.get::<_,bool>(0))? {
        return Ok(false);
    }
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM cdr_runtime_capability_requirements
        WHERE component='async_recovery_policy' AND format_version>=1)",
        [],
        |r| r.get(0),
    )?)
}
