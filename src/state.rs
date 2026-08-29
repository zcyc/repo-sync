use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    error::Error,
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS targets (
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    last_attempt_ms INTEGER NOT NULL DEFAULT 0,
    last_success_ms INTEGER,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'unknown',
    last_error TEXT,
    last_duration_ms INTEGER,
    PRIMARY KEY (source, target)
);
CREATE TABLE IF NOT EXISTS synced_refs (
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    ref_name TEXT NOT NULL,
    sha TEXT NOT NULL,
    PRIMARY KEY (source, target, ref_name),
    FOREIGN KEY (source, target) REFERENCES targets(source, target) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY NOT NULL,
    source TEXT NOT NULL,
    started_ms INTEGER NOT NULL,
    finished_ms INTEGER,
    status TEXT NOT NULL,
    pushed_targets INTEGER NOT NULL DEFAULT 0,
    skipped_branches INTEGER NOT NULL DEFAULT 0,
    skipped_tags INTEGER NOT NULL DEFAULT 0,
    failed_targets INTEGER NOT NULL DEFAULT 0,
    error TEXT
);
CREATE INDEX IF NOT EXISTS runs_source_started_idx ON runs(source, started_ms DESC);
"#;

pub(crate) struct StateDb {
    connection: Connection,
}

pub(crate) struct RunSummary {
    pub(crate) status: String,
    pub(crate) pushed_targets: usize,
    pub(crate) skipped_branches: usize,
    pub(crate) skipped_tags: usize,
    pub(crate) failed_targets: usize,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub workspace: String,
    pub source: String,
    pub initialized: bool,
    pub latest_run: Option<RunStatus>,
    pub targets: Vec<TargetStatus>,
}

#[derive(Debug, Serialize)]
pub struct RunStatus {
    pub run_id: String,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    pub status: String,
    pub pushed_targets: i64,
    pub skipped_branches: i64,
    pub skipped_tags: i64,
    pub failed_targets: i64,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TargetStatus {
    pub target: String,
    pub last_attempt_ms: i64,
    pub last_success_ms: Option<i64>,
    pub consecutive_failures: i64,
    pub status: String,
    pub last_error: Option<String>,
    pub last_duration_ms: Option<i64>,
    pub synced_refs: BTreeMap<String, String>,
}

impl StateDb {
    pub(crate) fn open(workspace: &Path, source: &str) -> Result<Self, Box<dyn Error>> {
        let path = database_path(workspace)?;
        let connection = Connection::open(&path)?;
        configure(&connection)?;
        connection.execute_batch(SCHEMA)?;
        let stored_source: Option<String> = connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'source'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(stored_source) = stored_source {
            if stored_source != source {
                return Err("state database source does not match configuration source".into());
            }
        } else {
            connection.execute(
                "INSERT INTO metadata(key, value) VALUES ('source', ?1)",
                [source],
            )?;
        }
        Ok(Self { connection })
    }

    pub(crate) fn begin_run(
        &self,
        run_id: &str,
        source: &str,
        started_ms: i64,
    ) -> rusqlite::Result<()> {
        self.connection.execute(
            "INSERT INTO runs(run_id, source, started_ms, status) VALUES (?1, ?2, ?3, 'running')",
            params![run_id, source, started_ms],
        )?;
        Ok(())
    }

    pub(crate) fn finish_run(
        &self,
        run_id: &str,
        finished_ms: i64,
        summary: &RunSummary,
    ) -> rusqlite::Result<()> {
        self.connection.execute(
            "UPDATE runs
             SET finished_ms = ?2, status = ?3, pushed_targets = ?4,
                 skipped_branches = ?5, skipped_tags = ?6,
                 failed_targets = ?7, error = ?8
             WHERE run_id = ?1",
            params![
                run_id,
                finished_ms,
                summary.status,
                summary.pushed_targets as i64,
                summary.skipped_branches as i64,
                summary.skipped_tags as i64,
                summary.failed_targets as i64,
                summary.error.as_deref(),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn mark_running(
        &self,
        source: &str,
        target: &str,
        attempted_ms: i64,
    ) -> rusqlite::Result<()> {
        self.connection.execute(
            "INSERT INTO targets(source, target, last_attempt_ms, status, last_error)
             VALUES (?1, ?2, ?3, 'running', NULL)
             ON CONFLICT(source, target) DO UPDATE SET
                 last_attempt_ms = excluded.last_attempt_ms,
                 status = excluded.status,
                 last_error = NULL",
            params![source, target, attempted_ms],
        )?;
        Ok(())
    }

    pub(crate) fn mark_success(
        &mut self,
        source: &str,
        target: &str,
        status: &str,
        duration_ms: i64,
        synced_refs: &BTreeMap<String, String>,
        replace_refs: bool,
    ) -> rusqlite::Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE targets
             SET status = ?3, last_success_ms = ?4, consecutive_failures = 0,
                 last_error = NULL, last_duration_ms = ?5
             WHERE source = ?1 AND target = ?2",
            params![source, target, status, now_ms(), duration_ms],
        )?;
        if replace_refs {
            transaction.execute(
                "DELETE FROM synced_refs WHERE source = ?1 AND target = ?2",
                params![source, target],
            )?;
            for (ref_name, sha) in synced_refs {
                transaction.execute(
                    "INSERT INTO synced_refs(source, target, ref_name, sha) VALUES (?1, ?2, ?3, ?4)",
                    params![source, target, ref_name, sha],
                )?;
            }
        }
        transaction.commit()
    }

    pub(crate) fn mark_failure(
        &self,
        source: &str,
        target: &str,
        duration_ms: i64,
        error: &str,
    ) -> rusqlite::Result<()> {
        self.connection.execute(
            "UPDATE targets
             SET status = 'failed', consecutive_failures = consecutive_failures + 1,
                 last_error = ?3, last_duration_ms = ?4
             WHERE source = ?1 AND target = ?2",
            params![source, target, error, duration_ms],
        )?;
        Ok(())
    }

    pub(crate) fn mark_source_failure(
        &mut self,
        source: &str,
        targets: &[String],
        error: &str,
    ) -> rusqlite::Result<()> {
        let transaction = self.connection.transaction()?;
        for target in targets {
            transaction.execute(
                "INSERT INTO targets(source, target, last_attempt_ms, status, last_error)
                 VALUES (?1, ?2, ?3, 'failed', ?4)
                 ON CONFLICT(source, target) DO UPDATE SET
                     last_attempt_ms = excluded.last_attempt_ms,
                     status = excluded.status,
                     consecutive_failures = targets.consecutive_failures + 1,
                     last_error = excluded.last_error",
                params![source, target, now_ms(), error],
            )?;
        }
        transaction.commit()
    }
}

pub fn status(workspace: &Path, source: &str) -> Result<StatusReport, Box<dyn Error>> {
    let path = database_path(workspace)?;
    let workspace_text = workspace.to_string_lossy().into_owned();
    if !path.exists() {
        return Ok(StatusReport {
            workspace: workspace_text,
            source: source.into(),
            initialized: false,
            latest_run: None,
            targets: Vec::new(),
        });
    }
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let stored_source: Option<String> = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'source'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if stored_source.as_deref() != Some(source) {
        return Err("state database source does not match configuration source".into());
    }

    let latest_run = connection
        .query_row(
            "SELECT run_id, started_ms, finished_ms, status, pushed_targets,
                    skipped_branches, skipped_tags, failed_targets, error
             FROM runs WHERE source = ?1 ORDER BY started_ms DESC LIMIT 1",
            [source],
            |row| {
                Ok(RunStatus {
                    run_id: row.get(0)?,
                    started_ms: row.get(1)?,
                    finished_ms: row.get(2)?,
                    status: row.get(3)?,
                    pushed_targets: row.get(4)?,
                    skipped_branches: row.get(5)?,
                    skipped_tags: row.get(6)?,
                    failed_targets: row.get(7)?,
                    error: row.get(8)?,
                })
            },
        )
        .optional()?;

    let target_rows = {
        let mut statement = connection.prepare(
            "SELECT target, last_attempt_ms, last_success_ms, consecutive_failures,
                    status, last_error, last_duration_ms
             FROM targets WHERE source = ?1 ORDER BY target",
        )?;
        let rows = statement
            .query_map([source], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut targets = Vec::with_capacity(target_rows.len());
    for (
        target,
        last_attempt_ms,
        last_success_ms,
        consecutive_failures,
        target_status,
        last_error,
        last_duration_ms,
    ) in target_rows
    {
        let mut refs = BTreeMap::new();
        let mut statement = connection.prepare(
            "SELECT ref_name, sha FROM synced_refs
             WHERE source = ?1 AND target = ?2 ORDER BY ref_name",
        )?;
        for row in statement.query_map(params![source, target], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (ref_name, sha) = row?;
            refs.insert(ref_name, sha);
        }
        targets.push(TargetStatus {
            target,
            last_attempt_ms,
            last_success_ms,
            consecutive_failures,
            status: target_status,
            last_error,
            last_duration_ms,
            synced_refs: refs,
        });
    }

    Ok(StatusReport {
        workspace: workspace_text,
        source: source.into(),
        initialized: true,
        latest_run,
        targets,
    })
}

pub fn cooldown_active(
    workspace: &Path,
    source: &str,
    targets: &[String],
    cooldown_secs: u64,
) -> Result<bool, Box<dyn Error>> {
    if cooldown_secs == 0 || targets.is_empty() {
        return Ok(false);
    }
    let path = database_path(workspace)?;
    if !path.exists() {
        return Ok(false);
    }
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let cutoff = now_ms().saturating_sub((cooldown_secs.saturating_mul(1000)) as i64);
    for target in targets {
        let row: Option<(i64, i64)> = connection
            .query_row(
                "SELECT last_attempt_ms, consecutive_failures
                 FROM targets WHERE source = ?1 AND target = ?2",
                params![source, target],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((last_attempt_ms, failures)) = row else {
            return Ok(false);
        };
        if failures == 0 || last_attempt_ms < cutoff {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn configure(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;",
    )
}

fn database_path(workspace: &Path) -> io::Result<PathBuf> {
    let name = workspace
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "workspace has no file name"))?;
    let mut path = workspace.to_path_buf();
    path.set_file_name(format!("{}.sqlite3", name.to_string_lossy()));
    Ok(path)
}
