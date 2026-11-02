CREATE TABLE IF NOT EXISTS cdr_archived_room_dispositions (
    operation_id TEXT PRIMARY KEY,
    channel_id INTEGER NOT NULL CHECK(channel_id>0),
    target_thread_id TEXT NOT NULL,
    parent_id INTEGER NOT NULL CHECK(parent_id>0),
    guild_id INTEGER NOT NULL CHECK(guild_id>0),
    mapping_json TEXT NOT NULL CHECK(json_valid(mapping_json)),
    source_json TEXT NOT NULL CHECK(json_valid(source_json)),
    origin_json TEXT NOT NULL CHECK(json_valid(origin_json)),
    policy_version INTEGER NOT NULL CHECK(policy_version=1),
    phase TEXT NOT NULL CHECK(phase IN ('prepared','delete_attempted','delete_unknown','delete_rejected','deleted','mapping_retired','aborted_source_changed')),
    created_at REAL NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS cdr_archived_room_current
ON cdr_archived_room_dispositions(channel_id) WHERE phase<>'aborted_source_changed';
CREATE TABLE IF NOT EXISTS cdr_archived_room_history (
    sequence INTEGER PRIMARY KEY,
    operation_id TEXT NOT NULL,
    phase TEXT NOT NULL,
    detail TEXT NOT NULL,
    recorded_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS cdr_archived_room_records (
    operation_id TEXT NOT NULL,
    table_name TEXT NOT NULL,
    row_key TEXT NOT NULL,
    snapshot_json TEXT NOT NULL CHECK(json_valid(snapshot_json)),
    PRIMARY KEY(operation_id,table_name,row_key)
);
CREATE INDEX IF NOT EXISTS cdr_archived_work_identity
ON cdr_archived_room_records(table_name,row_key);
CREATE TRIGGER IF NOT EXISTS cdr_archived_room_identity_immutable
BEFORE UPDATE ON cdr_archived_room_dispositions
WHEN NEW.operation_id IS NOT OLD.operation_id OR NEW.channel_id IS NOT OLD.channel_id
 OR NEW.target_thread_id IS NOT OLD.target_thread_id OR NEW.parent_id IS NOT OLD.parent_id
 OR NEW.guild_id IS NOT OLD.guild_id OR NEW.mapping_json IS NOT OLD.mapping_json
 OR NEW.source_json IS NOT OLD.source_json OR NEW.origin_json IS NOT OLD.origin_json
 OR NEW.policy_version IS NOT OLD.policy_version OR NEW.created_at IS NOT OLD.created_at
BEGIN SELECT RAISE(ABORT,'archived room identity is immutable'); END;
CREATE TRIGGER IF NOT EXISTS cdr_archived_room_dispositions_delete
BEFORE DELETE ON cdr_archived_room_dispositions
BEGIN SELECT RAISE(ABORT,'archived room disposition must be preserved'); END;
CREATE TRIGGER IF NOT EXISTS cdr_archived_room_records_update
BEFORE UPDATE ON cdr_archived_room_records
BEGIN SELECT RAISE(ABORT,'archived room evidence is immutable'); END;
CREATE TRIGGER IF NOT EXISTS cdr_archived_room_records_delete
BEFORE DELETE ON cdr_archived_room_records
BEGIN SELECT RAISE(ABORT,'archived room evidence must be preserved'); END;
CREATE TRIGGER IF NOT EXISTS cdr_archived_room_history_update
BEFORE UPDATE ON cdr_archived_room_history
BEGIN SELECT RAISE(ABORT,'archived room history is immutable'); END;
CREATE TRIGGER IF NOT EXISTS cdr_archived_room_history_delete
BEFORE DELETE ON cdr_archived_room_history
BEGIN SELECT RAISE(ABORT,'archived room history must be preserved'); END;
