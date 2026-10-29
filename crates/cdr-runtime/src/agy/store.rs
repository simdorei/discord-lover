//! AGY custody is deliberately independent of Codex `mirror_projects`/`mirror_threads`.
use super::{AgyError, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

pub struct Store(Mutex<Connection>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub channel: u64,
    pub project: String,
    pub cwd: PathBuf,
    pub conversation: Option<String>,
    pub blocked: bool,
}

impl Store {
    pub fn open(path: &Path, guild: u64) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_millis(100))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), guild TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS containers (key TEXT PRIMARY KEY, channel TEXT UNIQUE, parent TEXT, kind TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS projects (name TEXT PRIMARY KEY, cwd TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS sessions (channel TEXT PRIMARY KEY, project TEXT NOT NULL, cwd TEXT NOT NULL, conversation TEXT, blocked INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS requests (id TEXT PRIMARY KEY, origin TEXT NOT NULL, target TEXT, state TEXT NOT NULL, result TEXT, generation TEXT NOT NULL DEFAULT '0');
            CREATE TABLE IF NOT EXISTS input_boundaries (channel TEXT PRIMARY KEY, generation TEXT NOT NULL, epoch TEXT NOT NULL, cutoff TEXT NOT NULL);
            INSERT OR IGNORE INTO identity(singleton,guild) VALUES(1,'');")?;
        let has_generation: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('requests') WHERE name='generation')",
            [],
            |r| r.get(0),
        )?;
        if !has_generation {
            connection.execute_batch(
                "ALTER TABLE requests ADD COLUMN generation TEXT NOT NULL DEFAULT '0'",
            )?;
        }
        let previous: String =
            connection.query_row("SELECT guild FROM identity WHERE singleton=1", [], |row| {
                row.get(0)
            })?;
        if !previous.is_empty() && previous != guild.to_string() {
            return Err(AgyError::Invalid(
                "AGY database belongs to another Discord guild".into(),
            ));
        }
        connection.execute(
            "UPDATE identity SET guild=? WHERE singleton=1",
            [guild.to_string()],
        )?;
        connection.execute_batch("COMMIT;")?;
        let store = Self(Mutex::new(connection));
        store.recover()?;
        Ok(store)
    }

    /// Only the lifecycle owner calls this after all previous work has quiesced.
    pub fn recover(&self) -> Result<()> {
        // Never replay an uncertain CLI invocation after a crash. In particular, an
        // interrupted first turn without an init event must not silently start anew.
        self.db()?.execute_batch("BEGIN IMMEDIATE;
            UPDATE sessions SET blocked=1 WHERE conversation IS NULL AND channel IN
                (SELECT target FROM requests WHERE state='running');
            UPDATE requests SET state='interrupted', result='브릿지가 중단되어 실행 완료 여부를 확인할 수 없습니다. 자동 재실행하지 않았습니다.'
                WHERE state IN ('accepted','running');
            COMMIT;")?;
        Ok(())
    }

    fn db(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.0
            .lock()
            .map_err(|_| AgyError::Invalid("AGY store lock poisoned".into()))
    }

    pub fn container(&self, key: &str) -> Result<Option<Option<u64>>> {
        let raw: Option<Option<String>> = self
            .db()?
            .query_row("SELECT channel FROM containers WHERE key=?", [key], |row| {
                row.get(0)
            })
            .optional()?;
        raw.map(|id| id.map(|id| parse_id(&id)).transpose())
            .transpose()
    }

    pub fn begin_container(&self, key: &str, parent: Option<u64>, kind: &str) -> Result<()> {
        self.db()?.execute(
            "INSERT INTO containers(key,parent,kind) VALUES(?,?,?)",
            params![key, parent.map(|p| p.to_string()), kind],
        )?;
        Ok(())
    }

    pub fn confirm_container(&self, key: &str, channel: u64) -> Result<()> {
        if self.db()?.execute(
            "UPDATE containers SET channel=? WHERE key=? AND channel IS NULL",
            params![channel.to_string(), key],
        )? != 1
        {
            return Err(AgyError::Invalid(
                "AGY room receipt changed before confirmation".into(),
            ));
        }
        Ok(())
    }

    pub fn bind_project(&self, name: &str, cwd: &Path) -> Result<()> {
        let db = self.db()?;
        let cwd = cwd.to_string_lossy();
        let old: Option<String> = db
            .query_row("SELECT cwd FROM projects WHERE name=?", [name], |row| {
                row.get(0)
            })
            .optional()?;
        if old.as_deref().is_some_and(|old| old != cwd) {
            return Err(AgyError::Invalid(format!(
                "AGY project {name} has a different saved workspace; use a new project name"
            )));
        }
        db.execute(
            "INSERT OR IGNORE INTO projects(name,cwd) VALUES(?,?)",
            params![name, cwd.as_ref()],
        )?;
        Ok(())
    }

    pub fn session(&self, channel: u64) -> Result<Option<Session>> {
        Ok(self
            .db()?
            .query_row(
                "SELECT project,cwd,conversation,blocked FROM sessions WHERE channel=?",
                [channel.to_string()],
                |row| {
                    Ok(Session {
                        channel,
                        project: row.get(0)?,
                        cwd: PathBuf::from(row.get::<_, String>(1)?),
                        conversation: row.get(2)?,
                        blocked: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let db = self.db()?;
        let mut statement =
            db.prepare("SELECT channel,project,cwd,conversation,blocked FROM sessions")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (id, project, cwd, conversation, blocked) = row?;
            Ok(Session {
                channel: parse_id(&id)?,
                project,
                cwd: cwd.into(),
                conversation,
                blocked,
            })
        })
        .collect()
    }

    pub fn create_session(&self, channel: u64, project: &str, cwd: &Path) -> Result<()> {
        self.db()?.execute(
            "INSERT INTO sessions(channel,project,cwd) VALUES(?,?,?)",
            params![channel.to_string(), project, cwd.to_string_lossy().as_ref()],
        )?;
        Ok(())
    }

    pub fn save_conversation(&self, channel: u64, conversation: &str) -> Result<()> {
        if conversation.is_empty()
            || conversation.len() > 512
            || conversation.chars().any(char::is_control)
        {
            return Err(AgyError::Protocol("invalid AGY conversation id".into()));
        }
        let changed=self.db()?.execute("UPDATE sessions SET conversation=? WHERE channel=? AND (conversation IS NULL OR conversation=?)", params![conversation,channel.to_string(),conversation])?;
        if changed != 1 {
            return Err(AgyError::Protocol(
                "AGY changed the conversation id; continuation stopped".into(),
            ));
        }
        Ok(())
    }

    pub fn claim(&self, id: &str, origin: u64) -> Result<bool> {
        self.claim_generation(id, origin, 0)
    }
    pub fn claim_generation(&self, id: &str, origin: u64, generation: u64) -> Result<bool> {
        Ok(self.db()?.execute(
            "INSERT OR IGNORE INTO requests(id,origin,state,generation) VALUES(?,?,'accepted',?)",
            params![id, origin.to_string(), generation.to_string()],
        )? == 1)
    }

    pub fn activate(
        &self,
        generation: u64,
        epoch: &str,
        cutoff: u64,
        channels: &[u64],
    ) -> Result<()> {
        let mut db = self.db()?;
        let transaction = db.transaction()?;
        let pending: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM requests WHERE state IN ('accepted','running'))",
            [],
            |r| r.get(0),
        )?;
        if pending {
            return Err(AgyError::Invalid(
                "AGY has unsettled accepted requests".into(),
            ));
        }
        for channel in channels {
            transaction.execute("INSERT OR REPLACE INTO input_boundaries(channel,generation,epoch,cutoff) VALUES(?,?,?,?)",params![channel.to_string(),generation.to_string(),epoch,cutoff.to_string()])?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn running(&self, id: &str, target: u64) -> Result<()> {
        if self.db()?.execute(
            "UPDATE requests SET target=?,state='running' WHERE id=? AND state='accepted'",
            params![target.to_string(), id],
        )? != 1
        {
            return Err(AgyError::Invalid(
                "AGY request is no longer accepted".into(),
            ));
        }
        Ok(())
    }

    pub fn finish(&self, id: &str, state: &str, result: &str) -> Result<()> {
        self.db()?.execute(
            "UPDATE requests SET state=?,result=? WHERE id=?",
            params![state, result, id],
        )?;
        Ok(())
    }

    pub fn block_unbound_session(&self, channel: u64) -> Result<()> {
        self.db()?.execute(
            "UPDATE sessions SET blocked=1 WHERE channel=? AND conversation IS NULL",
            [channel.to_string()],
        )?;
        Ok(())
    }

    pub fn last_result(&self, channel: u64) -> Result<Option<(String, String)>> {
        Ok(self.db()?.query_row("SELECT state,COALESCE(result,'') FROM requests WHERE target=?1 OR (target IS NULL AND origin=?1 AND state!='control') ORDER BY rowid DESC LIMIT 1",[channel.to_string()],|row| Ok((row.get(0)?,row.get(1)?))).optional()?)
    }
}

fn parse_id(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| AgyError::Invalid("invalid AGY Discord id in store".into()))
}
