CREATE VIEW IF NOT EXISTS cdr_archive_inspections_v1 AS
SELECT ingress_id FROM discord_ingress_journal AS d WHERE
    (kind='message' AND json_extract(payload_json,'$.version')=1 AND (
        json_extract(payload_json,'$.plan.Execute') IN ('Help','Runners','Doctor','Where','Identity','Resources')
        OR json_type(payload_json,'$.plan.Execute.SavedRequest')='object'))
    OR (kind='interaction' AND json_extract(payload_json,'$.version')=1
        AND json_extract(payload_json,'$.work.Slash.name') IN ('help','runners','doctor','where'))
    OR (kind='message' AND application_id IS NULL
        AND owner_user_id>0 AND channel_id>0 AND event_id>0 AND source_message_id=event_id
        AND target_thread_id IS NOT NULL AND length(target_thread_id)>0
        AND json_type(payload_json,'$.version')='integer' AND json_extract(payload_json,'$.version')=1
        AND json_type(payload_json,'$.author_is_bot')='false'
        AND (json_type(payload_json,'$.processing_mode') IS NULL
            OR json_extract(payload_json,'$.processing_mode')='normal')
        AND (json_type(payload_json,'$.settings_binding') IS NULL
            OR json_type(payload_json,'$.settings_binding')='null')
        AND (json_type(payload_json,'$.lifecycle_binding') IS NULL
            OR json_type(payload_json,'$.lifecycle_binding')='null')
        AND json_type(payload_json,'$.work') IS NULL
        AND json_type(payload_json,'$.plan.Execute.DiscardRequest')='object'
        AND (SELECT count(*) FROM json_each(payload_json,'$.plan'))=1
        AND (SELECT count(*) FROM json_each(payload_json,'$.plan.Execute'))=1
        AND (SELECT count(*) FROM json_each(payload_json,'$.plan.Execute.DiscardRequest'))=1
        AND json_type(payload_json,'$.plan.Execute.DiscardRequest.job_id')='text'
        AND EXISTS(SELECT 1 FROM codex_turn_queue q
            WHERE q.job_id=json_extract(d.payload_json,'$.plan.Execute.DiscardRequest.job_id')
            AND q.target_thread_id=d.target_thread_id AND q.channel_id=d.channel_id
            AND q.owner_user_id=d.owner_user_id AND q.state='pending' AND q.turn_id IS NULL))
    OR (kind='interaction' AND application_id>0 AND event_id>0
        AND owner_user_id>0 AND channel_id>0 AND source_message_id>0
        AND target_thread_id IS NOT NULL AND length(target_thread_id)>0
        AND json_type(payload_json,'$.version')='integer' AND json_extract(payload_json,'$.version')=1
        AND (
            (SELECT count(*) FROM json_each(payload_json))=2
            OR ((SELECT count(*) FROM json_each(payload_json))=5
                AND json_extract(payload_json,'$.processing_mode')='normal'
                AND json_type(payload_json,'$.settings_binding')='null'
                AND json_type(payload_json,'$.request_rejection')='null'))
        AND (SELECT count(*) FROM json_each(payload_json,'$.work'))=1
        AND (SELECT count(*) FROM json_each(payload_json,'$.work.Component'))=1
        AND json_type(payload_json,'$.work.Component.RecoveryAbandonDecision')='object'
        AND (SELECT count(*) FROM json_each(payload_json,'$.work.Component.RecoveryAbandonDecision'))=3
        AND json_type(payload_json,'$.work.Component.RecoveryAbandonDecision.proposal_id')='text'
        AND length(json_extract(payload_json,'$.work.Component.RecoveryAbandonDecision.proposal_id'))=32
        AND json_extract(payload_json,'$.work.Component.RecoveryAbandonDecision.proposal_id') NOT GLOB '*[^0-9a-f]*'
        AND json_type(payload_json,'$.work.Component.RecoveryAbandonDecision.revision')='integer'
        AND json_extract(payload_json,'$.work.Component.RecoveryAbandonDecision.revision')>0
        AND json_extract(payload_json,'$.work.Component.RecoveryAbandonDecision.decision') IN ('AbandonOnly','KeepHeld'));
