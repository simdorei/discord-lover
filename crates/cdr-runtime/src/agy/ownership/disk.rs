use super::{AgyError, Result, Snapshot};
use rusqlite::{Connection, OpenFlags};
use std::{io::Write, path::Path, time::Duration};

const MARKER: &str = ".codex_discord_agy.ever-configured.json";
pub(in crate::agy) fn mark_configured(root: &Path, guild: u64) -> Result<()> {
    let path = root.join(MARKER);
    if path.try_exists()? {
        validate_marker(&path, guild)?;
        return Ok(());
    }
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&serde_json::to_vec(
        &serde_json::json!({"version":1,"guild":guild}),
    )?)?;
    file.as_file().sync_all()?;
    #[cfg(windows)]
    cdr_windows_native::atomic_replace(file.path(), &path)?;
    #[cfg(not(windows))]
    std::fs::rename(file.path(), &path)?;
    Ok(())
}
fn validate_marker(path: &Path, guild: u64) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_slice(&super::super::config::read_bounded(path)?)?;
    if value["version"] != 1 || value["guild"].as_u64() != Some(guild) {
        return Err(AgyError::Invalid(
            "AGY custody marker is invalid or belongs to another guild".into(),
        ));
    }
    Ok(())
}
fn read_only(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_millis(50))?;
    Ok(db)
}
pub(in crate::agy) fn read_snapshot(root: &Path, mirror_db: &Path, guild: u64) -> Result<Snapshot> {
    let marker = root.join(MARKER);
    let path = root.join(".codex_discord_agy.sqlite");
    let configured = marker.try_exists()?;
    if configured {
        validate_marker(&marker, guild)?;
    }
    let exists = path.try_exists()?;
    if configured && !exists {
        return Err(AgyError::Invalid(
            "AGY custody database is missing; existing rooms remain protected".into(),
        ));
    }
    let mut snapshot = Snapshot {
        never_configured: !configured && !exists,
        available: true,
        ..Snapshot::default()
    };
    if exists {
        let db = read_only(&path)?;
        let stored: String =
            db.query_row("SELECT guild FROM identity WHERE singleton=1", [], |r| {
                r.get(0)
            })?;
        if stored != guild.to_string() {
            return Err(AgyError::Invalid("AGY custody guild mismatch".into()));
        }
        let mut statement = db.prepare("SELECT channel,parent,key FROM containers")?;
        for row in statement.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (id, parent, receipt) = row?;
            let parent = parent.map(|s| parse(&s)).transpose()?;
            if let Some(id) = id {
                snapshot.agy.insert(parse(&id)?, parent);
            } else {
                snapshot.pending_creations.insert(receipt);
            }
        }
        let mut statement = db.prepare("SELECT channel FROM sessions")?;
        for row in statement.query_map([], |r| r.get::<_, String>(0))? {
            snapshot.agy.entry(parse(&row?)?).or_insert(None);
        }
    }
    if mirror_db.try_exists()? {
        let db = read_only(mirror_db)?;
        for table in ["mirror_projects", "mirror_threads"] {
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
                [table],
                |r| r.get(0),
            )?;
            if !exists {
                continue;
            }
            let query = if table == "mirror_projects" {
                "SELECT discord_channel_id FROM mirror_projects"
            } else {
                "SELECT discord_thread_id FROM mirror_threads UNION SELECT discord_channel_id FROM mirror_threads"
            };
            let mut statement = db.prepare(query)?;
            for row in statement.query_map([], |r| r.get::<_, i64>(0))? {
                let id = u64::try_from(row?)
                    .ok()
                    .filter(|id| *id != 0)
                    .ok_or_else(|| AgyError::Invalid("Invalid Codex mirror ID".into()))?;
                snapshot.codex.insert(id);
            }
        }
    }
    Ok(snapshot)
}
fn parse(id: &str) -> Result<u64> {
    id.parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| AgyError::Invalid("Invalid AGY custody ID".into()))
}
