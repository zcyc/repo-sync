use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    error::Error,
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

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
CREATE TABLE IF NOT EXISTS webhook_events (
    event_id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    provider TEXT NOT NULL,
    delivery_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    refs_json TEXT NOT NULL,
    received_ms INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued',
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_ms INTEGER NOT NULL,
    started_ms INTEGER,
    lease_token TEXT,
    finished_ms INTEGER,
    last_error TEXT,
    UNIQUE(source, provider, delivery_id)
);
CREATE INDEX IF NOT EXISTS webhook_events_queue_idx
    ON webhook_events(source, status, next_attempt_ms, received_ms);
CREATE INDEX IF NOT EXISTS webhook_events_history_idx
    ON webhook_events(source, received_ms DESC);
"#;
const SCHEMA_VERSION: i64 = 2;
const MIN_WEBHOOK_DEDUP_RETENTION_DAYS: u64 = 7;

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
    pub recent_runs: Vec<RunStatus>,
    pub targets: Vec<TargetStatus>,
}

#[derive(Debug, Serialize, Clone)]
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

#[derive(Debug, Serialize, Clone)]
pub struct WebhookEventStatus {
    pub workspace: String,
    pub source: String,
    pub event_id: i64,
    pub provider: String,
    pub delivery_id: String,
    pub event_type: String,
    pub refs: Vec<WebhookRefChange>,
    pub received_ms: i64,
    pub status: String,
    pub attempts: i64,
    pub next_attempt_ms: i64,
    pub started_ms: Option<i64>,
    pub finished_ms: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WebhookRefChange {
    pub reference: String,
    pub deleted: bool,
    pub new_sha: Option<String>,
}

pub(crate) struct QueuedEvent {
    pub(crate) event_id: i64,
    pub(crate) attempts: i64,
    pub(crate) lease_token: String,
}

pub(crate) enum WebhookEnqueue {
    Enqueued,
    Duplicate,
    Full,
}

pub(crate) struct WebhookEventInput<'a> {
    pub(crate) source: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) delivery_id: &'a str,
    pub(crate) event_type: &'a str,
    pub(crate) refs_json: &'a str,
    pub(crate) received_ms: i64,
}

impl StateDb {
    pub(crate) fn open(workspace: &Path, source: &str) -> Result<Self, Box<dyn Error>> {
        let path = database_path(workspace)?;
        let mut connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        configure(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let user_version: i64 =
            transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let has_user_schema: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master
                 WHERE type IN ('table', 'index', 'view', 'trigger')
                   AND name NOT LIKE 'sqlite_%'
             )",
            [],
            |row| row.get::<_, i64>(0).map(|value| value != 0),
        )?;
        if user_version == 0 && has_user_schema {
            return Err(
                "state database has no supported schema version; remove it and resync".into(),
            );
        }
        if user_version != 0 && user_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported state database schema version {user_version}; expected {SCHEMA_VERSION}"
            )
            .into());
        }
        transaction.execute_batch(SCHEMA)?;
        if user_version == 0 {
            transaction.execute_batch("PRAGMA user_version = 2;")?;
        }
        transaction.execute(
            "INSERT OR IGNORE INTO metadata(key, value) VALUES ('source', ?1)",
            [source],
        )?;
        let stored_source: Option<String> = transaction
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
        }
        transaction.commit()?;
        Ok(Self { connection })
    }

    fn enqueue_trigger_event(
        &mut self,
        source: &str,
        provider: &str,
        event_type: &str,
        delivery_prefix: &str,
        received_ms: i64,
        max_pending_events: u64,
    ) -> rusqlite::Result<WebhookEnqueue> {
        let delivery_id = format!("{delivery_prefix}-{}", Uuid::new_v4());
        self.enqueue_webhook_event(
            WebhookEventInput {
                source,
                provider,
                delivery_id: &delivery_id,
                event_type,
                refs_json: "[]",
                received_ms,
            },
            max_pending_events,
        )
    }

    pub(crate) fn enqueue_manual_event(
        &mut self,
        source: &str,
        received_ms: i64,
        max_pending_events: u64,
    ) -> rusqlite::Result<WebhookEnqueue> {
        self.enqueue_trigger_event(
            source,
            "manual",
            "manual",
            "manual",
            received_ms,
            max_pending_events,
        )
    }

    pub(crate) fn enqueue_scheduled_event(
        &mut self,
        source: &str,
        received_ms: i64,
        max_pending_events: u64,
    ) -> rusqlite::Result<WebhookEnqueue> {
        self.enqueue_trigger_event(
            source,
            "schedule",
            "cron",
            "schedule",
            received_ms,
            max_pending_events,
        )
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

    pub(crate) fn enqueue_webhook_event(
        &mut self,
        event: WebhookEventInput<'_>,
        max_pending_events: u64,
    ) -> rusqlite::Result<WebhookEnqueue> {
        let max_pending_events = i64::try_from(max_pending_events).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "webhook_max_pending_events is too large",
            )))
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate: i64 = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM webhook_events
                 WHERE source = ?1 AND provider = ?2 AND delivery_id = ?3
             )",
            params![event.source, event.provider, event.delivery_id],
            |row| row.get(0),
        )?;
        if duplicate != 0 {
            transaction.commit()?;
            return Ok(WebhookEnqueue::Duplicate);
        }
        let pending: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM webhook_events
             WHERE source = ?1 AND status IN ('queued', 'failed', 'running')",
            [event.source],
            |row| row.get(0),
        )?;
        if pending >= max_pending_events {
            transaction.commit()?;
            return Ok(WebhookEnqueue::Full);
        }
        transaction.execute(
            "INSERT OR IGNORE INTO webhook_events(
                 source, provider, delivery_id, event_type, refs_json,
                 received_ms, next_attempt_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                event.source,
                event.provider,
                event.delivery_id,
                event.event_type,
                event.refs_json,
                event.received_ms
            ],
        )?;
        transaction.commit()?;
        Ok(WebhookEnqueue::Enqueued)
    }

    pub(crate) fn claim_webhook_event(
        &mut self,
        source: &str,
        now_ms: i64,
        lease_ms: i64,
        event_id: Option<i64>,
    ) -> rusqlite::Result<Option<QueuedEvent>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE webhook_events
             SET status = 'queued', started_ms = NULL, next_attempt_ms = ?2,
                 lease_token = NULL, last_error = 'worker lease expired'
             WHERE source = ?1 AND status = 'running' AND next_attempt_ms <= ?2",
            params![source, now_ms],
        )?;
        let candidate: Option<(i64, i64)> = match event_id {
            Some(event_id) => transaction
                .query_row(
                    "SELECT event_id, attempts FROM webhook_events
                     WHERE source = ?1 AND event_id = ?2 AND status IN ('queued', 'failed')
                       AND next_attempt_ms <= ?3
                     LIMIT 1",
                    params![source, event_id, now_ms],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?,
            None => transaction
                .query_row(
                    "SELECT event_id, attempts FROM webhook_events
                     WHERE source = ?1 AND status IN ('queued', 'failed')
                       AND next_attempt_ms <= ?2
                     ORDER BY received_ms, event_id LIMIT 1",
                    params![source, now_ms],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?,
        };
        let Some((event_id, attempts)) = candidate else {
            transaction.commit()?;
            return Ok(None);
        };
        let lease_token = Uuid::new_v4().to_string();
        transaction.execute(
            "UPDATE webhook_events
             SET status = 'running', attempts = attempts + 1,
                 started_ms = ?2, next_attempt_ms = ?3, lease_token = ?4,
                 last_error = NULL
             WHERE event_id = ?1 AND status IN ('queued', 'failed')",
            params![
                event_id,
                now_ms,
                now_ms.saturating_add(lease_ms.max(1)),
                lease_token
            ],
        )?;
        transaction.commit()?;
        Ok(Some(QueuedEvent {
            event_id,
            attempts: attempts + 1,
            lease_token,
        }))
    }

    pub(crate) fn finish_webhook_event(
        &self,
        event: &QueuedEvent,
        max_attempts: i64,
        error: Option<&str>,
        finished_ms: i64,
        retry_after_ms: i64,
    ) -> rusqlite::Result<bool> {
        let cancelled = error == Some("sync cancelled");
        let (status, next_attempt_ms) = if cancelled {
            ("cancelled", finished_ms)
        } else if error.is_none() {
            ("succeeded", finished_ms)
        } else if event.attempts >= max_attempts {
            ("dead", finished_ms)
        } else {
            ("failed", retry_after_ms)
        };
        let changed = self.connection.execute(
            "UPDATE webhook_events
             SET status = ?2, finished_ms = ?3, next_attempt_ms = ?4,
                 lease_token = NULL, last_error = ?5
             WHERE event_id = ?1 AND status = 'running' AND attempts = ?6
               AND lease_token = ?7",
            params![
                event.event_id,
                status,
                finished_ms,
                next_attempt_ms,
                error,
                event.attempts,
                event.lease_token
            ],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn renew_webhook_event(
        &self,
        event_id: i64,
        lease_token: &str,
        lease_until_ms: i64,
    ) -> rusqlite::Result<bool> {
        let changed = self.connection.execute(
            "UPDATE webhook_events
             SET next_attempt_ms = ?3
             WHERE event_id = ?1 AND status = 'running' AND lease_token = ?2",
            params![event_id, lease_token, lease_until_ms],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn webhook_event_is_running(&self, event_id: i64) -> rusqlite::Result<bool> {
        let running: i64 = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM webhook_events
                 WHERE event_id = ?1 AND status = 'running'
             )",
            [event_id],
            |row| row.get(0),
        )?;
        Ok(running != 0)
    }

    pub(crate) fn cancel_webhook_events(&self, source: &str) -> rusqlite::Result<usize> {
        self.connection.execute(
            "UPDATE webhook_events
             SET status = 'cancelled', finished_ms = ?2, next_attempt_ms = ?2,
                 lease_token = NULL, last_error = 'cancelled by operator'
             WHERE source = ?1 AND status IN ('queued', 'failed', 'running')",
            params![source, now_ms()],
        )
    }

    pub(crate) fn retry_webhook_event(
        &self,
        source: &str,
        event_id: i64,
    ) -> rusqlite::Result<bool> {
        let changed = self.connection.execute(
            "UPDATE webhook_events
             SET status = 'queued', attempts = 0, next_attempt_ms = ?3,
                 started_ms = NULL, lease_token = NULL, finished_ms = NULL,
                 last_error = NULL
             WHERE source = ?1 AND event_id = ?2 AND status IN ('failed', 'dead')",
            params![source, event_id, now_ms()],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn has_retryable_webhook_event(
        &self,
        source: &str,
        event_id: i64,
    ) -> rusqlite::Result<bool> {
        let exists: i64 = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM webhook_events
                 WHERE source = ?1 AND event_id = ?2 AND status IN ('failed', 'dead')
             )",
            params![source, event_id],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    pub(crate) fn coalesce_webhook_events(
        &self,
        source: &str,
        completed_event_id: i64,
        finished_ms: i64,
    ) -> rusqlite::Result<usize> {
        self.connection.execute(
            "UPDATE webhook_events
             SET status = 'coalesced', finished_ms = ?3, next_attempt_ms = ?3,
                 last_error = NULL
             WHERE source = ?1 AND event_id != ?2 AND status IN ('queued', 'failed')",
            params![source, completed_event_id, finished_ms],
        )
    }

    pub(crate) fn prune_history(
        &mut self,
        source: &str,
        before_ms: i64,
    ) -> rusqlite::Result<usize> {
        let transaction = self.connection.transaction()?;
        let events = transaction.execute(
            "DELETE FROM webhook_events
             WHERE source = ?1 AND status IN ('succeeded', 'coalesced', 'dead', 'cancelled')
               AND COALESCE(finished_ms, received_ms) < ?2",
            params![source, before_ms],
        )?;
        let runs = transaction.execute(
            "DELETE FROM runs
             WHERE source = ?1 AND finished_ms IS NOT NULL AND finished_ms < ?2",
            params![source, before_ms],
        )?;
        transaction.commit()?;
        Ok(events + runs)
    }

    pub(crate) fn backup_into(&self, destination: &Path) -> rusqlite::Result<()> {
        self.connection
            .execute("VACUUM INTO ?1", [destination.to_string_lossy().as_ref()])?;
        Ok(())
    }
}

fn verify_existing_database(connection: &Connection, source: &str) -> Result<(), Box<dyn Error>> {
    let user_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported state database schema version {user_version}; expected {SCHEMA_VERSION}"
        )
        .into());
    }
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
    Ok(())
}

pub fn check_state(workspace: &Path, source: &str) -> Result<(), Box<dyn Error>> {
    let path = database_path(workspace)?;
    if !path.exists() {
        return Ok(());
    }
    check_state_database(&path, source)
}

fn check_state_database(path: &Path, source: &str) -> Result<(), Box<dyn Error>> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    verify_existing_database(&connection, source)?;
    let result: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(format!("state database integrity check failed: {result}").into());
    }
    Ok(())
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
            recent_runs: Vec::new(),
            targets: Vec::new(),
        });
    }
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    verify_existing_database(&connection, source)?;

    let recent_runs = {
        let mut statement = connection.prepare(
            "SELECT run_id, started_ms, finished_ms, status, pushed_targets,
                    skipped_branches, skipped_tags, failed_targets, error
             FROM runs WHERE source = ?1 ORDER BY started_ms DESC LIMIT 20",
        )?;
        let rows = statement
            .query_map([source], |row| {
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
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let latest_run = recent_runs.first().cloned();

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
        recent_runs,
        targets,
    })
}

pub fn webhook_events(
    workspace: &Path,
    source: &str,
    limit: usize,
) -> Result<Vec<WebhookEventStatus>, Box<dyn Error>> {
    let path = database_path(workspace)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    verify_existing_database(&connection, source)?;
    let mut statement = connection.prepare(
        "SELECT event_id, provider, delivery_id, event_type, refs_json,
                received_ms, status, attempts, next_attempt_ms, started_ms,
                finished_ms, last_error
         FROM webhook_events WHERE source = ?1
         ORDER BY received_ms DESC, event_id DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(params![source, limit as i64], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<i64>>(10)?,
            row.get::<_, Option<String>>(11)?,
        ))
    })?;
    let mut events = Vec::new();
    for row in rows {
        let (
            event_id,
            provider,
            delivery_id,
            event_type,
            refs_json,
            received_ms,
            event_status,
            attempts,
            next_attempt_ms,
            started_ms,
            finished_ms,
            last_error,
        ) = row?;
        events.push(WebhookEventStatus {
            workspace: workspace.to_string_lossy().into_owned(),
            source: source.into(),
            event_id,
            provider,
            delivery_id,
            event_type,
            refs: serde_json::from_str(&refs_json)?,
            received_ms,
            status: event_status,
            attempts,
            next_attempt_ms,
            started_ms,
            finished_ms,
            last_error,
        });
    }
    Ok(events)
}

pub(crate) struct WebhookQueueStats {
    pub(crate) counts: BTreeMap<String, i64>,
    pub(crate) oldest_pending_ms: Option<i64>,
    pub(crate) next_attempt_ms: Option<i64>,
}

pub(crate) fn webhook_queue_stats(
    workspace: &Path,
    source: &str,
) -> Result<WebhookQueueStats, Box<dyn Error>> {
    let path = database_path(workspace)?;
    if !path.exists() {
        return Ok(WebhookQueueStats {
            counts: BTreeMap::new(),
            oldest_pending_ms: None,
            next_attempt_ms: None,
        });
    }
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    verify_existing_database(&connection, source)?;
    let mut statement = connection.prepare(
        "SELECT status, COUNT(*) FROM webhook_events
         WHERE source = ?1 GROUP BY status ORDER BY status",
    )?;
    let counts = statement
        .query_map([source], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<BTreeMap<String, i64>>>()?;
    let oldest_pending_ms = connection
        .query_row(
            "SELECT MIN(received_ms) FROM webhook_events
             WHERE source = ?1 AND status IN ('queued', 'failed', 'running')",
            [source],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let next_attempt_ms = connection.query_row(
        "SELECT MIN(next_attempt_ms) FROM webhook_events
         WHERE source = ?1 AND status IN ('queued', 'failed', 'running')",
        [source],
        |row| row.get(0),
    )?;
    Ok(WebhookQueueStats {
        counts,
        oldest_pending_ms,
        next_attempt_ms,
    })
}

pub fn retry_webhook_event(
    workspace: &Path,
    source: &str,
    event_id: i64,
) -> Result<bool, Box<dyn Error>> {
    let path = database_path(workspace)?;
    if !path.exists() {
        return Ok(false);
    }
    let db = StateDb::open(workspace, source)?;
    Ok(db.retry_webhook_event(source, event_id)?)
}

pub fn cancel_webhook_events(workspace: &Path, source: &str) -> Result<usize, Box<dyn Error>> {
    let path = database_path(workspace)?;
    if !path.exists() {
        return Ok(0);
    }
    let db = StateDb::open(workspace, source)?;
    Ok(db.cancel_webhook_events(source)?)
}

pub fn prune_history(
    workspace: &Path,
    source: &str,
    older_than_days: u64,
) -> Result<usize, Box<dyn Error>> {
    if older_than_days < MIN_WEBHOOK_DEDUP_RETENTION_DAYS {
        return Err(format!(
            "webhook history retention must be at least {MIN_WEBHOOK_DEDUP_RETENTION_DAYS} days"
        )
        .into());
    }
    let path = database_path(workspace)?;
    if !path.exists() {
        return Ok(0);
    }
    let cutoff = now_ms().saturating_sub(
        older_than_days
            .saturating_mul(24 * 60 * 60 * 1000)
            .try_into()
            .unwrap_or(i64::MAX),
    );
    let mut db = StateDb::open(workspace, source)?;
    Ok(db.prune_history(source, cutoff)?)
}

pub fn backup_state(
    workspace: &Path,
    source: &str,
    destination: &Path,
) -> Result<(), Box<dyn Error>> {
    let path = database_path(workspace)?;
    if !path.exists() {
        return Err("state database does not exist".into());
    }
    if destination.exists() {
        return Err("backup destination already exists".into());
    }
    if !destination
        .parent()
        .is_some_and(|parent| parent.as_os_str().is_empty() || parent.exists())
    {
        return Err("backup destination parent does not exist".into());
    }
    let db = StateDb::open(workspace, source)?;
    db.backup_into(destination)?;
    check_state_database(destination, source)?;
    Ok(())
}

pub(crate) fn has_retryable_webhook_event(
    workspace: &Path,
    source: &str,
    event_id: i64,
) -> Result<bool, Box<dyn Error>> {
    let path = database_path(workspace)?;
    if !path.exists() {
        return Ok(false);
    }
    let db = StateDb::open(workspace, source)?;
    Ok(db.has_retryable_webhook_event(source, event_id)?)
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
    verify_existing_database(&connection, source)?;
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
    let workspace = crate::config::workspace_identity(workspace)?;
    let name = workspace
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "workspace has no file name"))?
        .to_string_lossy()
        .into_owned();
    let mut path = workspace;
    path.set_file_name(format!("{name}.sqlite3"));
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{
        backup_state, check_state, prune_history, webhook_events, StateDb, WebhookEnqueue,
        WebhookEventInput, WebhookRefChange,
    };
    use rusqlite::Connection;
    use std::{
        fs,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Barrier,
        },
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT_STATE_TEST: AtomicU64 = AtomicU64::new(0);

    fn enqueue(
        db: &mut StateDb,
        source: &str,
        delivery_id: &str,
        refs_json: &str,
        received_ms: i64,
        max_pending_events: u64,
    ) -> WebhookEnqueue {
        db.enqueue_webhook_event(
            WebhookEventInput {
                source,
                provider: "github",
                delivery_id,
                event_type: "push",
                refs_json,
                received_ms,
            },
            max_pending_events,
        )
        .unwrap()
    }

    #[test]
    fn webhook_delivery_is_idempotent_and_retriable() {
        let sequence = NEXT_STATE_TEST.fetch_add(1, Ordering::Relaxed);
        let workspace = std::env::temp_dir().join(format!(
            "repo-sync-state-test-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = "https://github.com/example/source.git";
        let refs = serde_json::to_string(&vec![WebhookRefChange {
            reference: "refs/heads/main".into(),
            deleted: false,
            new_sha: Some("abc".into()),
        }])
        .unwrap();
        let mut db = StateDb::open(&workspace, source).unwrap();
        assert!(matches!(
            enqueue(&mut db, source, "delivery-1", &refs, 1, 100),
            WebhookEnqueue::Enqueued
        ));
        assert!(matches!(
            enqueue(&mut db, source, "delivery-1", &refs, 1, 100),
            WebhookEnqueue::Duplicate
        ));
        assert!(matches!(
            enqueue(&mut db, source, "delivery-full", &refs, 1, 1),
            WebhookEnqueue::Full
        ));
        assert!(matches!(
            enqueue(&mut db, source, "delivery-2", &refs, 1, 100),
            WebhookEnqueue::Enqueued
        ));
        let now = super::now_ms();
        let claimed = db
            .claim_webhook_event(source, now, 1_000, None)
            .unwrap()
            .unwrap();
        db.finish_webhook_event(&claimed, 1, Some("failed"), now + 1, now + 1)
            .unwrap();
        assert!(db
            .has_retryable_webhook_event(source, claimed.event_id)
            .unwrap());
        assert!(db.retry_webhook_event(source, claimed.event_id).unwrap());
        let retry_now = super::now_ms();
        let claimed = db
            .claim_webhook_event(source, retry_now, 1_000, Some(claimed.event_id))
            .unwrap()
            .unwrap();
        db.finish_webhook_event(&claimed, 1, None, retry_now + 1, retry_now + 1)
            .unwrap();
        assert_eq!(
            db.coalesce_webhook_events(source, claimed.event_id, retry_now + 1)
                .unwrap(),
            1
        );
        drop(db);
        assert!(check_state(&workspace, source).is_ok());

        let history = webhook_events(&workspace, source, 10).unwrap();
        assert_eq!(history.len(), 2);
        assert!(history.iter().any(|event| event.status == "succeeded"));
        assert!(history.iter().any(|event| event.status == "coalesced"));
        assert!(history
            .iter()
            .all(|event| event.refs[0].reference == "refs/heads/main"));

        let database = workspace.with_file_name(format!(
            "{}.sqlite3",
            workspace.file_name().unwrap().to_string_lossy()
        ));
        let _ = fs::remove_file(&database);
        let _ = fs::remove_file(database.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(database.with_extension("sqlite3-shm"));
    }

    #[test]
    fn concurrent_state_initialization_is_idempotent() {
        let sequence = NEXT_STATE_TEST.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repo-sync-state-race-test-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace");
        let source = "https://example.test/source.git";
        let barrier = Arc::new(Barrier::new(16));
        let handles = (0..16)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let workspace = workspace.clone();
                thread::spawn(move || {
                    barrier.wait();
                    StateDb::open(&workspace, source)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let database = workspace.with_file_name("workspace.sqlite3");
        let _ = fs::remove_file(&database);
        let _ = fs::remove_file(database.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(database.with_extension("sqlite3-shm"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn trigger_events_are_persistent_and_cancellable() {
        let sequence = NEXT_STATE_TEST.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repo-sync-manual-event-test-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace");
        let source = "https://example.test/source.git";
        let mut db = StateDb::open(&workspace, source).unwrap();
        assert!(matches!(
            db.enqueue_scheduled_event(source, super::now_ms(), 10),
            Ok(WebhookEnqueue::Enqueued)
        ));
        assert!(matches!(
            db.enqueue_manual_event(source, super::now_ms(), 10),
            Ok(WebhookEnqueue::Enqueued)
        ));
        let claimed = db
            .claim_webhook_event(source, super::now_ms(), 1_000, None)
            .unwrap()
            .unwrap();
        assert_eq!(db.cancel_webhook_events(source).unwrap(), 2);
        assert!(!db.webhook_event_is_running(claimed.event_id).unwrap());
        drop(db);
        let events = webhook_events(&workspace, source, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].provider, "manual");
        assert_eq!(events[0].status, "cancelled");
        assert_eq!(events[1].provider, "schedule");
        let database = workspace.with_file_name("workspace.sqlite3");
        let _ = fs::remove_file(&database);
        let _ = fs::remove_file(database.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(database.with_extension("sqlite3-shm"));
    }

    #[test]
    fn maintenance_prunes_finished_history_and_creates_backup() {
        let sequence = NEXT_STATE_TEST.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repo-sync-maintenance-test-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace");
        let source = "https://github.com/example/source.git";
        let mut db = StateDb::open(&workspace, source).unwrap();
        assert!(matches!(
            enqueue(&mut db, source, "old-success", "[]", 1, 100),
            WebhookEnqueue::Enqueued
        ));
        let now = super::now_ms();
        let claimed = db
            .claim_webhook_event(source, now, 1_000, None)
            .unwrap()
            .unwrap();
        db.finish_webhook_event(&claimed, 1, None, 2, 2).unwrap();
        assert!(matches!(
            enqueue(&mut db, source, "old-dead", "[]", 1, 100),
            WebhookEnqueue::Enqueued
        ));
        let claimed = db
            .claim_webhook_event(source, now, 1_000, None)
            .unwrap()
            .unwrap();
        db.finish_webhook_event(&claimed, 1, Some("failed"), 2, 2)
            .unwrap();
        assert!(matches!(
            enqueue(&mut db, source, "old-cancelled", "[]", 1, 100),
            WebhookEnqueue::Enqueued
        ));
        let claimed = db
            .claim_webhook_event(source, now, 1_000, None)
            .unwrap()
            .unwrap();
        db.finish_webhook_event(&claimed, 1, Some("sync cancelled"), 2, 2)
            .unwrap();
        assert!(matches!(
            enqueue(&mut db, source, "pending", "[]", now, 100),
            WebhookEnqueue::Enqueued
        ));
        drop(db);

        assert!(prune_history(&workspace, source, 1).is_err());
        assert_eq!(prune_history(&workspace, source, 7).unwrap(), 3);
        assert_eq!(webhook_events(&workspace, source, 10).unwrap().len(), 1);
        let backup = root.join("backup.sqlite3");
        backup_state(&workspace, source, &backup).unwrap();
        assert!(backup.exists());
        assert!(backup_state(&workspace, source, &backup).is_err());

        let database = workspace.with_file_name("workspace.sqlite3");
        let _ = fs::remove_file(&database);
        let _ = fs::remove_file(database.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(database.with_extension("sqlite3-shm"));
        let _ = fs::remove_file(backup);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn expired_webhook_lease_is_requeued() {
        let sequence = NEXT_STATE_TEST.fetch_add(1, Ordering::Relaxed);
        let workspace = std::env::temp_dir().join(format!(
            "repo-sync-lease-test-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = "https://github.com/example/source.git";
        let mut db = StateDb::open(&workspace, source).unwrap();
        assert!(matches!(
            enqueue(&mut db, source, "lease-1", "[]", 1, 10),
            WebhookEnqueue::Enqueued
        ));
        let now = super::now_ms();
        let claimed = db
            .claim_webhook_event(source, now, 10, None)
            .unwrap()
            .unwrap();
        let reclaimed = db
            .claim_webhook_event(source, now + 11, 10, None)
            .unwrap()
            .unwrap();
        assert_eq!(reclaimed.event_id, claimed.event_id);

        let database = workspace.with_file_name("workspace.sqlite3");
        let _ = fs::remove_file(&database);
        let _ = fs::remove_file(database.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(database.with_extension("sqlite3-shm"));
    }

    #[test]
    fn webhook_lease_renewal_requires_current_owner() {
        let sequence = NEXT_STATE_TEST.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repo-sync-lease-renewal-test-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace");
        let source = "https://github.com/example/source.git";
        let mut db = StateDb::open(&workspace, source).unwrap();
        assert!(matches!(
            enqueue(&mut db, source, "lease-renewal", "[]", 1, 10),
            WebhookEnqueue::Enqueued
        ));
        let now = super::now_ms();
        let claimed = db
            .claim_webhook_event(source, now, 10, None)
            .unwrap()
            .unwrap();
        assert!(db
            .renew_webhook_event(claimed.event_id, &claimed.lease_token, now + 5_000)
            .unwrap());
        assert!(!db
            .renew_webhook_event(claimed.event_id, "stale-owner", now + 10_000)
            .unwrap());
        let events = webhook_events(&workspace, source, 10).unwrap();
        assert_eq!(events[0].next_attempt_ms, now + 5_000);

        let database = workspace.with_file_name("workspace.sqlite3");
        let _ = fs::remove_file(&database);
        let _ = fs::remove_file(database.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(database.with_extension("sqlite3-shm"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_webhook_lease_owner_cannot_finish_event() {
        let sequence = NEXT_STATE_TEST.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repo-sync-stale-lease-test-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace");
        let source = "https://github.com/example/source.git";
        let mut db = StateDb::open(&workspace, source).unwrap();
        assert!(matches!(
            enqueue(&mut db, source, "stale-lease", "[]", 1, 10),
            WebhookEnqueue::Enqueued
        ));

        let now = super::now_ms();
        let first = db
            .claim_webhook_event(source, now, 1, None)
            .unwrap()
            .unwrap();
        let second = db
            .claim_webhook_event(source, now + 2, 1_000, None)
            .unwrap()
            .unwrap();
        assert_eq!(first.event_id, second.event_id);
        assert!(second.attempts > first.attempts);

        assert!(!db
            .finish_webhook_event(&first, 1, None, now + 3, now + 3,)
            .unwrap());
        let events = webhook_events(&workspace, source, 10).unwrap();
        assert_eq!(events[0].status, "running");
        assert_eq!(events[0].attempts, second.attempts);

        assert!(db
            .finish_webhook_event(&second, 10, Some("failed"), now + 4, now + 4)
            .unwrap());
        assert!(db.retry_webhook_event(source, second.event_id).unwrap());
        let third = db
            .claim_webhook_event(source, now + 5, 1_000, None)
            .unwrap()
            .unwrap();
        assert_eq!(third.event_id, first.event_id);
        assert!(!db
            .finish_webhook_event(&first, 10, None, now + 6, now + 6,)
            .unwrap());
        let events = webhook_events(&workspace, source, 10).unwrap();
        assert_eq!(events[0].status, "running");

        assert!(db
            .finish_webhook_event(&third, 10, None, now + 7, now + 7,)
            .unwrap());
        drop(db);
        let events = webhook_events(&workspace, source, 10).unwrap();
        assert_eq!(events[0].status, "succeeded");

        let database = workspace.with_file_name("workspace.sqlite3");
        let _ = fs::remove_file(&database);
        let _ = fs::remove_file(database.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(database.with_extension("sqlite3-shm"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_unversioned_state_database() {
        let sequence = NEXT_STATE_TEST.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repo-sync-schema-test-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace");
        let database = workspace.with_file_name("workspace.sqlite3");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        drop(connection);
        assert!(StateDb::open(&workspace, "source").is_err());
        assert!(check_state(&workspace, "source").is_err());
        let _ = fs::remove_file(database);
        let _ = fs::remove_dir_all(root);
    }
}
