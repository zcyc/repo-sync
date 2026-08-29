use crate::{config, Item};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use job_scheduler_ng::Schedule;
use rand_core::OsRng;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 3;
const SESSION_TTL_MS: i64 = 12 * 60 * 60 * 1000;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    task_id INTEGER PRIMARY KEY AUTOINCREMENT,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0, 1)),
    source TEXT NOT NULL,
    workspace TEXT NOT NULL UNIQUE,
    config_json TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_updated_idx ON tasks(updated_ms DESC);
CREATE TABLE IF NOT EXISTS admin_account (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    username TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS admin_sessions (
    token_hash TEXT PRIMARY KEY NOT NULL,
    username TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS admin_sessions_expiry_idx ON admin_sessions(expires_ms);
"#;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Task {
    pub id: i64,
    pub enabled: bool,
    pub item: Item,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug)]
pub(crate) struct LoginSession {
    pub(crate) username: String,
    pub(crate) token: String,
}

pub(crate) struct TaskDb {
    connection: Connection,
}

impl TaskDb {
    pub(crate) fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let user_version: i64 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let has_user_schema: bool = connection.query_row(
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
                "task database has no supported schema version; remove it and recreate it".into(),
            );
        }
        if user_version != 0 && user_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported task database schema version {user_version}; expected {SCHEMA_VERSION}"
            )
            .into());
        }
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             ",
        )?;
        connection.execute_batch(SCHEMA)?;
        if user_version == 0 {
            connection.execute_batch("PRAGMA user_version = 3;")?;
        }
        Ok(Self { connection })
    }

    pub(crate) fn list(&self) -> Result<Vec<Task>, Box<dyn Error>> {
        let mut statement = self.connection.prepare(
            "SELECT task_id, enabled, config_json, created_ms, updated_ms
                 FROM tasks ORDER BY task_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut tasks = Vec::new();
        for row in rows {
            let (id, enabled, config_json, created_ms, updated_ms) = row?;
            let item: Item = serde_json::from_str(&config_json)?;
            validate_item(&item)?;
            tasks.push(Task {
                id,
                enabled,
                item,
                created_ms,
                updated_ms,
            });
        }
        Ok(tasks)
    }

    pub(crate) fn create(&mut self, item: &Item, enabled: bool) -> Result<Task, Box<dyn Error>> {
        validate_item(item)?;
        let config_json = serde_json::to_string(item)?;
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO tasks(enabled, source, workspace, config_json, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                i64::from(enabled),
                item.source,
                item.workspace,
                config_json,
                now
            ],
        )?;
        let id = transaction.last_insert_rowid();
        transaction.commit()?;
        Ok(Task {
            id,
            enabled,
            item: item.clone(),
            created_ms: now,
            updated_ms: now,
        })
    }

    pub(crate) fn update(
        &mut self,
        id: i64,
        item: &Item,
        enabled: bool,
    ) -> Result<Option<Task>, Box<dyn Error>> {
        validate_item(item)?;
        let config_json = serde_json::to_string(item)?;
        let created_ms: Option<i64> = self
            .connection
            .query_row(
                "SELECT created_ms FROM tasks WHERE task_id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(created_ms) = created_ms else {
            return Ok(None);
        };
        let updated_ms = now_ms();
        let changed = self.connection.execute(
            "UPDATE tasks
             SET enabled = ?2, source = ?3, workspace = ?4, config_json = ?5, updated_ms = ?6
             WHERE task_id = ?1",
            params![
                id,
                i64::from(enabled),
                item.source,
                item.workspace,
                config_json,
                updated_ms
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(Task {
            id,
            enabled,
            item: item.clone(),
            created_ms,
            updated_ms,
        }))
    }

    pub(crate) fn delete(&mut self, id: i64) -> rusqlite::Result<bool> {
        Ok(self
            .connection
            .execute("DELETE FROM tasks WHERE task_id = ?1", [id])?
            == 1)
    }

    pub(crate) fn auth_initialized(&self) -> rusqlite::Result<bool> {
        let exists: i64 = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM admin_account WHERE id = 1)",
            [],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    pub(crate) fn setup_admin(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<LoginSession, Box<dyn Error>> {
        validate_credentials(username, password)?;
        let password_hash = hash_password(password)?;
        let session = new_session(username);
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: i64 = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM admin_account WHERE id = 1)",
            [],
            |row| row.get(0),
        )?;
        if exists != 0 {
            return Err("admin account is already initialized".into());
        }
        transaction.execute(
            "INSERT INTO admin_account(id, username, password_hash, created_ms)
             VALUES (1, ?1, ?2, ?3)",
            params![username, password_hash, now],
        )?;
        insert_session(&transaction, &session, now)?;
        transaction.commit()?;
        Ok(session)
    }

    pub(crate) fn login_admin(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<LoginSession, Box<dyn Error>> {
        if validate_credentials(username, password).is_err() {
            return Err("invalid username or password".into());
        }
        let account: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT username, password_hash FROM admin_account WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((stored_username, password_hash)) = account else {
            return Err("admin account is not initialized".into());
        };
        let parsed_hash = PasswordHash::new(&password_hash)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        if stored_username != username
            || Argon2::default()
                .verify_password(password.as_bytes(), &parsed_hash)
                .is_err()
        {
            return Err("invalid username or password".into());
        }
        let session = new_session(&stored_username);
        let now = now_ms();
        self.connection
            .execute("DELETE FROM admin_sessions WHERE expires_ms <= ?1", [now])?;
        insert_session(&self.connection, &session, now)?;
        Ok(session)
    }

    pub(crate) fn authenticate_session(&mut self, token: &str) -> rusqlite::Result<Option<String>> {
        let now = now_ms();
        self.connection
            .execute("DELETE FROM admin_sessions WHERE expires_ms <= ?1", [now])?;
        self.connection
            .query_row(
                "SELECT username FROM admin_sessions
                 WHERE token_hash = ?1 AND expires_ms > ?2",
                params![token_hash(token), now],
                |row| row.get(0),
            )
            .optional()
    }

    pub(crate) fn logout_session(&mut self, token: &str) -> rusqlite::Result<()> {
        self.connection.execute(
            "DELETE FROM admin_sessions WHERE token_hash = ?1",
            [token_hash(token)],
        )?;
        Ok(())
    }

    pub(crate) fn reset_admin(&mut self) -> rusqlite::Result<bool> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = transaction.execute("DELETE FROM admin_account WHERE id = 1", [])?;
        transaction.execute("DELETE FROM admin_sessions", [])?;
        transaction.commit()?;
        Ok(deleted == 1)
    }

    pub(crate) fn change_password(
        &mut self,
        token: &str,
        current_password: &str,
        new_password: &str,
    ) -> Result<LoginSession, Box<dyn Error>> {
        let now = now_ms();
        let username: Option<String> = self
            .connection
            .query_row(
                "SELECT username FROM admin_sessions
                 WHERE token_hash = ?1 AND expires_ms > ?2",
                params![token_hash(token), now],
                |row| row.get(0),
            )
            .optional()?;
        let Some(username) = username else {
            return Err("login session is invalid or expired".into());
        };
        let password_hash: String = self.connection.query_row(
            "SELECT password_hash FROM admin_account WHERE id = 1 AND username = ?1",
            [&username],
            |row| row.get(0),
        )?;
        validate_credentials(&username, current_password)?;
        let parsed_hash = PasswordHash::new(&password_hash)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        Argon2::default()
            .verify_password(current_password.as_bytes(), &parsed_hash)
            .map_err(|_| "current password is invalid")?;
        validate_credentials(&username, new_password)?;
        let new_hash = hash_password(new_password)?;
        let session = new_session(&username);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE admin_account SET password_hash = ?1 WHERE id = 1",
            [&new_hash],
        )?;
        transaction.execute("DELETE FROM admin_sessions", [])?;
        insert_session(&transaction, &session, now)?;
        transaction.commit()?;
        Ok(session)
    }
}

pub fn list_tasks(path: &Path) -> Result<Vec<Task>, Box<dyn Error>> {
    TaskDb::open(path)?.list()
}

pub fn create_task(path: &Path, item: &Item, enabled: bool) -> Result<Task, Box<dyn Error>> {
    TaskDb::open(path)?.create(item, enabled)
}

pub fn update_task(
    path: &Path,
    id: i64,
    item: &Item,
    enabled: bool,
) -> Result<Option<Task>, Box<dyn Error>> {
    TaskDb::open(path)?.update(id, item, enabled)
}

pub fn delete_task(path: &Path, id: i64) -> Result<bool, Box<dyn Error>> {
    Ok(TaskDb::open(path)?.delete(id)?)
}

pub(crate) fn auth_initialized(path: &Path) -> Result<bool, Box<dyn Error>> {
    Ok(TaskDb::open(path)?.auth_initialized()?)
}

pub(crate) fn setup_admin(
    path: &Path,
    username: &str,
    password: &str,
) -> Result<LoginSession, Box<dyn Error>> {
    TaskDb::open(path)?.setup_admin(username, password)
}

pub(crate) fn login_admin(
    path: &Path,
    username: &str,
    password: &str,
) -> Result<LoginSession, Box<dyn Error>> {
    TaskDb::open(path)?.login_admin(username, password)
}

pub(crate) fn authenticate_session(
    path: &Path,
    token: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    Ok(TaskDb::open(path)?.authenticate_session(token)?)
}

pub(crate) fn logout_session(path: &Path, token: &str) -> Result<(), Box<dyn Error>> {
    TaskDb::open(path)?.logout_session(token)?;
    Ok(())
}

pub fn reset_admin(path: &Path) -> Result<bool, Box<dyn Error>> {
    if !path.exists() {
        return Err("task database does not exist".into());
    }
    Ok(TaskDb::open(path)?.reset_admin()?)
}

pub(crate) fn change_password(
    path: &Path,
    token: &str,
    current_password: &str,
    new_password: &str,
) -> Result<LoginSession, Box<dyn Error>> {
    TaskDb::open(path)?.change_password(token, current_password, new_password)
}

pub fn backup_task_database(path: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    if !path.exists() {
        return Err("task database does not exist".into());
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
    let db = TaskDb::open(path)?;
    db.connection
        .execute("VACUUM INTO ?1", [destination.to_string_lossy().as_ref()])?;
    check_task_database(destination)?;
    Ok(())
}

pub fn check_task_database(path: &Path) -> Result<(), Box<dyn Error>> {
    if !path.exists() {
        return Ok(());
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let user_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported task database schema version {user_version}; expected {SCHEMA_VERSION}"
        )
        .into());
    }
    let result: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(format!("task database integrity check failed: {result}").into());
    }
    Ok(())
}

fn validate_item(item: &Item) -> Result<(), Box<dyn Error>> {
    config::validate_item(item)?;
    if let Some(crontab) = item.crontab.as_deref() {
        crontab.parse::<Schedule>()?;
    }
    Ok(())
}

fn validate_credentials(username: &str, password: &str) -> Result<(), Box<dyn Error>> {
    let username_length = username.chars().count();
    if !(1..=64).contains(&username_length) || username.chars().any(char::is_control) {
        return Err("username must be 1-64 characters without control characters".into());
    }
    let password_length = password.chars().count();
    if !(12..=256).contains(&password_length) {
        return Err("password must be 12-256 characters".into());
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String, Box<dyn Error>> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| io::Error::other(error.to_string()))?
        .to_string())
}

fn new_session(username: &str) -> LoginSession {
    let token = Uuid::new_v4().to_string();
    LoginSession {
        username: username.to_owned(),
        token,
    }
}

fn insert_session(
    connection: &rusqlite::Connection,
    session: &LoginSession,
    now: i64,
) -> rusqlite::Result<usize> {
    connection.execute(
        "INSERT INTO admin_sessions(token_hash, username, created_ms, expires_ms)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            token_hash(&session.token),
            session.username,
            now,
            now.saturating_add(SESSION_TTL_MS)
        ],
    )
}

fn token_hash(token: &str) -> String {
    STANDARD.encode(Sha256::digest(token.as_bytes()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{backup_task_database, TaskDb};
    use crate::{DivergencePolicy, Item, SyncMode, TagPolicy};
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT_TASK_TEST: AtomicU64 = AtomicU64::new(0);

    fn item(workspace: &str) -> Item {
        Item {
            source: "source".into(),
            target: vec!["target".into()],
            workspace: workspace.into(),
            mode: SyncMode::Branch,
            crontab: None,
            branches: Vec::new(),
            include_refs: Vec::new(),
            exclude_refs: Vec::new(),
            timeout_secs: 300,
            dry_run: false,
            allow_destructive: false,
            sync_lfs: false,
            divergence: DivergencePolicy::Fail,
            tag_policy: TagPolicy::Preserve,
            prune_branches: false,
            prune_tags: false,
            atomic: true,
            max_retries: 3,
            retry_backoff_secs: 5,
            failure_cooldown_secs: 60,
            webhook_secret_envs: Vec::new(),
            webhook_max_pending_events: 10_000,
            webhook_event_lease_secs: 900,
        }
    }

    #[test]
    fn task_database_supports_crud() {
        let sequence = NEXT_TASK_TEST.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repo-sync-task-test-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let database = root.join("tasks.sqlite3");
        let mut db = TaskDb::open(&database).unwrap();
        let task = db.create(&item("./workspace"), true).unwrap();
        assert_eq!(db.list().unwrap().len(), 1);
        let updated = db
            .update(task.id, &item("./workspace"), false)
            .unwrap()
            .unwrap();
        assert!(!updated.enabled);
        assert!(db.delete(task.id).unwrap());
        assert!(db.list().unwrap().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task_database_supports_auth_and_backup() {
        let sequence = NEXT_TASK_TEST.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repo-sync-auth-test-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let database = root.join("tasks.sqlite3");
        let backup = root.join("tasks-backup.sqlite3");
        let mut db = TaskDb::open(&database).unwrap();
        let session = db
            .setup_admin("admin", "correct horse battery staple")
            .unwrap();
        assert!(db.auth_initialized().unwrap());
        assert!(db.authenticate_session(&session.token).unwrap().is_some());
        let new_session = db
            .change_password(
                &session.token,
                "correct horse battery staple",
                "another correct battery phrase",
            )
            .unwrap();
        assert!(db.authenticate_session(&session.token).unwrap().is_none());
        assert!(db
            .login_admin("admin", "another correct battery phrase")
            .is_ok());
        assert_ne!(session.token, new_session.token);
        drop(db);
        backup_task_database(&database, &backup).unwrap();
        assert!(backup.exists());
        let mut db = TaskDb::open(&database).unwrap();
        assert!(db.reset_admin().unwrap());
        assert!(!db.auth_initialized().unwrap());
        let _ = fs::remove_dir_all(root);
    }
}
