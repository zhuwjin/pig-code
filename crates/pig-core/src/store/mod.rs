//! SQLite storage: {data_dir}/store.sqlite
//! - sessions table: session index (pin/archive/title all write here)
//! - workspaces table: workspace registry (alias, hidden flag)
//! - turn_usage table: per-turn token usage for statistics aggregation
//! - api_usage table: per-logical-call usage/timing/outcome and provider response identifiers
//! - todos table: current session todo-list state (full upsert; the event stream no longer persists to JSONL)
//! - file_changes table: current session file-change state (upsert by path; rows deleted when the net change is zero)
//! - file_originals table: original-content snapshots of changed files (cross-restart diff baseline / revert)

use std::path::{Path, PathBuf};

use pig_protocol::{SessionMeta, WorkspaceMeta};
use rusqlite::{Connection, params};

use crate::rollout::now_secs;

mod deletion;
mod execution;
pub use execution::ExecutionClaim;
mod runtime;
mod tasks;
pub use tasks::BackgroundTask;

/// Current SQLite schema version tracked via `PRAGMA user_version`.
/// Bump this and append a transactional step to `MIGRATIONS` whenever tables
/// or columns evolve.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

type MigrationFn = fn(&rusqlite::Transaction<'_>) -> Result<(), String>;

const MIGRATIONS: &[(u32, MigrationFn)] = &[(1, migrate_v1_baseline)];

fn migrate_v1_baseline(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(BASELINE_SCHEMA)
        .map_err(|e| format!("store.sqlite v1 baseline schema failed: {e}"))?;
    runtime::create_schema(tx)?;
    Ok(())
}

fn apply_migrations(conn: &mut Connection) -> Result<(), String> {
    let current: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| format!("store.sqlite read user_version failed: {e}"))?;
    if current > CURRENT_SCHEMA_VERSION {
        return Err(format!(
            "store.sqlite schema version {current} is newer than supported version {CURRENT_SCHEMA_VERSION}"
        ));
    }
    for &(target_version, migrate) in MIGRATIONS {
        if current >= target_version {
            continue;
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| format!("store.sqlite begin migration v{target_version} failed: {e}"))?;
        migrate(&tx)?;
        tx.pragma_update(None, "user_version", target_version)
            .map_err(|e| format!("store.sqlite set user_version={target_version} failed: {e}"))?;
        tx.commit()
            .map_err(|e| format!("store.sqlite commit migration v{target_version} failed: {e}"))?;
    }
    Ok(())
}

const BASELINE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                title_custom INTEGER NOT NULL DEFAULT 0,
                cwd TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                pinned INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                provider_id TEXT,
                model_id TEXT,
                reasoning_level TEXT,
                plan_enabled INTEGER NOT NULL DEFAULT 0,
                -- Canonical PermissionSettings JSON (sandbox/approval/reviewer)
                permissions TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS workspaces (
                path TEXT PRIMARY KEY,
                added_at INTEGER NOT NULL,
                alias TEXT,
                hidden INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS turn_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                -- Wall-clock end of the turn (unix ms): the moment the turn's
                -- usage was written, which is when the turn ended
                ended_at_ms INTEGER NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL,
                reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
                -- Durable per-turn identity shared with the rollout's
                -- TurnStats/StepUsage records; unique per session so recovery
                -- after either side failed cannot double count.
                turn_id TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_turn_usage_ended_at ON turn_usage(ended_at_ms);
            CREATE INDEX IF NOT EXISTS idx_turn_usage_session ON turn_usage(session_id);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_turn_usage_turn
                ON turn_usage(session_id, turn_id);
            CREATE TABLE IF NOT EXISTS api_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                started_at_ms INTEGER NOT NULL,
                source TEXT NOT NULL,
                turn_id TEXT,
                step INTEGER,
                agent_id TEXT,
                background INTEGER NOT NULL DEFAULT 0,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                api_format TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                ttft_ms INTEGER,
                usage_reported INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
                used_tokens INTEGER NOT NULL DEFAULT 0,
                context_window INTEGER NOT NULL DEFAULT 0,
                finish TEXT NOT NULL,
                error TEXT,
                retry_count INTEGER NOT NULL DEFAULT 0,
                http_status INTEGER,
                request_id TEXT,
                trace_id TEXT,
                response_id TEXT,
                cf_ray TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_api_usage_started_at ON api_usage(started_at_ms);
            CREATE INDEX IF NOT EXISTS idx_api_usage_session ON api_usage(session_id);
            CREATE TABLE IF NOT EXISTS pending_queue (
                session_id TEXT PRIMARY KEY,
                items TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS execution_claims (
                session_id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS execution_receipts (
                session_id TEXT NOT NULL,
                request_id TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY (session_id, request_id)
            );
            CREATE TABLE IF NOT EXISTS session_deletions (
                session_id TEXT PRIMARY KEY,
                requested_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS todos (
                session_id TEXT PRIMARY KEY,
                items TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS file_changes (
                session_id TEXT NOT NULL,
                path TEXT NOT NULL,
                unified_diff TEXT NOT NULL,
                additions INTEGER NOT NULL,
                deletions INTEGER NOT NULL,
                PRIMARY KEY (session_id, path)
            );
            CREATE TABLE IF NOT EXISTS background_tasks (
                session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                agent_id TEXT,
                command TEXT NOT NULL,
                state TEXT NOT NULL,
                exit_code INTEGER,
                reason TEXT,
                detached INTEGER NOT NULL,
                output_path TEXT,
                output_tail TEXT NOT NULL,
                record_path TEXT,
                result_path TEXT,
                started_at_ms INTEGER NOT NULL,
                ended_at_ms INTEGER,
                turns INTEGER,
                model TEXT,
                run_id TEXT,
                parent_agent_id TEXT,
                delegation_depth INTEGER,
                profile TEXT,
                run_input_tokens INTEGER,
                run_cache_read_tokens INTEGER,
                run_output_tokens INTEGER,
                terminal_notification_suppressed INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (session_id, task_id)
            );
            CREATE INDEX IF NOT EXISTS idx_background_tasks_session
                ON background_tasks(session_id, started_at_ms);
            CREATE TABLE IF NOT EXISTS task_notifications (
                notification_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                status TEXT NOT NULL,
                payload TEXT NOT NULL,
                delivery_state TEXT NOT NULL,
                delivery_channel TEXT,
                created_at_ms INTEGER NOT NULL,
                delivered_at_ms INTEGER,
                last_error TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_task_notifications_pending
                ON task_notifications(session_id, delivery_state, created_at_ms);
            CREATE TABLE IF NOT EXISTS file_originals (
                session_id TEXT NOT NULL,
                path TEXT NOT NULL,
                content TEXT,
                PRIMARY KEY (session_id, path)
            );";

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| format!("Failed to create data directory: {e}"))?;
        let path = data_dir.join("store.sqlite");
        let mut conn = Connection::open(&path)
            .map_err(|e| format!("Failed to open store.sqlite {}: {e}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .and_then(|_| conn.pragma_update(None, "busy_timeout", 5000u64))
            .map_err(|e| format!("store.sqlite pragma failed: {e}"))?;
        apply_migrations(&mut conn)?;
        // Self-heal any missing baseline tables/indexes after migrations succeed.
        conn.execute_batch(BASELINE_SCHEMA)
            .map_err(|e| format!("store.sqlite ensure baseline schema failed: {e}"))?;
        runtime::create_schema(&conn)?;
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> Result<u32, String> {
        self.conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| format!("store.sqlite read user_version failed: {e}"))
    }

    // ---- Session index ----

    fn permissions_from_row(
        row: &rusqlite::Row,
    ) -> rusqlite::Result<pig_protocol::PermissionSettings> {
        let raw: String = row.get(12)?;
        serde_json::from_str(&raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                12,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    }

    fn session_from_row(row: &rusqlite::Row) -> rusqlite::Result<SessionMeta> {
        Ok(SessionMeta {
            id: row.get(0)?,
            title: row.get(1)?,
            title_custom: row.get(2)?,
            cwd: PathBuf::from(row.get::<_, String>(3)?),
            created_at: row.get(4)?,
            updated_at: row.get(5)?,
            pinned: row.get(6)?,
            archived: row.get(7)?,
            provider_id: row.get(8)?,
            model_id: row.get(9)?,
            reasoning_level: row.get(10)?,
            plan_enabled: row.get::<_, i64>(11)? != 0,
            permissions: Self::permissions_from_row(row)?,
        })
    }

    const SESSION_COLUMNS: &'static str = "id, title, title_custom, cwd, created_at, updated_at, pinned, archived, provider_id, model_id, reasoning_level, plan_enabled, permissions";

    pub fn upsert_session(&self, meta: &SessionMeta) {
        let permissions_raw =
            serde_json::to_string(&meta.permissions).unwrap_or_else(|_| "{}".to_string());
        let result = self.conn.execute(
            "INSERT INTO sessions (id, title, title_custom, cwd, created_at, updated_at, pinned, archived, provider_id, model_id, reasoning_level, plan_enabled, permissions)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                title_custom = excluded.title_custom,
                cwd = excluded.cwd,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at,
                pinned = excluded.pinned,
                archived = excluded.archived,
                provider_id = excluded.provider_id,
                model_id = excluded.model_id,
                reasoning_level = excluded.reasoning_level,
                plan_enabled = excluded.plan_enabled,
                permissions = excluded.permissions",
            params![
                meta.id,
                meta.title,
                meta.title_custom,
                meta.cwd.display().to_string(),
                meta.created_at,
                meta.updated_at,
                meta.pinned,
                meta.archived,
                meta.provider_id,
                meta.model_id,
                meta.reasoning_level,
                meta.plan_enabled,
                permissions_raw,
            ],
        );
        // Write failures must not be silent (a missing column once dropped whole batches of new sessions): at least log at error level
        if let Err(error) = result {
            tracing::error!("upsert_session failed for {}: {error}", meta.id);
        }
    }

    /// Read-modify-write; no-op when the session does not exist.
    pub fn update_session(&self, id: &str, f: impl FnOnce(&mut SessionMeta)) {
        if let Some(mut meta) = self.get_session(id) {
            f(&mut meta);
            self.upsert_session(&meta);
        }
    }

    pub fn delete_session_checked(&mut self, id: &str) -> Result<(), pig_protocol::CoreError> {
        let transaction =
            self.conn
                .transaction()
                .map_err(|error| pig_protocol::CoreError::TaskStore {
                    detail: error.to_string(),
                })?;
        let requested: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM session_deletions WHERE session_id = ?1)",
                params![id],
                |row| row.get(0),
            )
            .map_err(|error| pig_protocol::CoreError::TaskStore {
                detail: format!("Failed to verify session deletion request for {id}: {error}"),
            })?;
        if !requested {
            return Err(pig_protocol::CoreError::TaskStore {
                detail: format!("Session {id} has no durable deletion request"),
            });
        }
        transaction
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(|error| pig_protocol::CoreError::TaskStore {
                detail: format!("Failed to delete session {id}: {error}"),
            })?;
        for table in [
            "turn_usage",
            "api_usage",
            "todos",
            "file_changes",
            "file_originals",
            "pending_queue",
            "execution_claims",
            "execution_receipts",
            "background_tasks",
            "task_notifications",
            "runtime_agents",
            "runtime_runs",
            "runtime_attempts",
            "runtime_mailbox",
            "runtime_events",
            "runtime_meta",
        ] {
            transaction
                .execute(
                    &format!("DELETE FROM {table} WHERE session_id = ?1"),
                    params![id],
                )
                .map_err(|error| pig_protocol::CoreError::TaskStore {
                    detail: format!("Failed to delete {table} for session {id}: {error}"),
                })?;
        }
        transaction
            .execute(
                "DELETE FROM session_deletions WHERE session_id = ?1",
                params![id],
            )
            .map_err(|error| pig_protocol::CoreError::TaskStore {
                detail: format!("Failed to clear deletion request for session {id}: {error}"),
            })?;
        transaction
            .commit()
            .map_err(|error| pig_protocol::CoreError::TaskStore {
                detail: error.to_string(),
            })
    }

    pub fn get_session(&self, id: &str) -> Option<SessionMeta> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {} FROM sessions WHERE id = ?1",
                    Self::SESSION_COLUMNS
                ),
                params![id],
                Self::session_from_row,
            )
            .ok()
    }

    /// Sorted by updated_at descending (sidebar list order).
    pub fn sorted_sessions(&self) -> Vec<SessionMeta> {
        let mut stmt = match self.conn.prepare(&format!(
            "SELECT {} FROM sessions
             WHERE NOT EXISTS (
                 SELECT 1 FROM session_deletions WHERE session_id = sessions.id
             )
             ORDER BY updated_at DESC",
            Self::SESSION_COLUMNS
        )) {
            Ok(stmt) => stmt,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map([], Self::session_from_row);
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => vec![],
        }
    }

    /// The workspace's most recently active session (the seed for new-session model/mode/reasoning-level inheritance); archived sessions do not count as active
    pub fn latest_active_in_workspace(&self, cwd: &Path) -> Option<SessionMeta> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {} FROM sessions
                     WHERE cwd = ?1 AND archived = 0
                     ORDER BY updated_at DESC LIMIT 1",
                    Self::SESSION_COLUMNS
                ),
                params![cwd.display().to_string()],
                Self::session_from_row,
            )
            .ok()
    }

    // ---- Workspace registry ----

    pub fn workspaces(&self) -> Vec<WorkspaceMeta> {
        let mut stmt = match self
            .conn
            .prepare("SELECT path, added_at, alias, hidden FROM workspaces ORDER BY added_at")
        {
            Ok(stmt) => stmt,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map([], |row| {
            Ok(WorkspaceMeta {
                path: PathBuf::from(row.get::<_, String>(0)?),
                added_at: row.get(1)?,
                alias: row.get(2)?,
                hidden: row.get(3)?,
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => vec![],
        }
    }

    fn workspace(&self, path: &Path) -> Option<WorkspaceMeta> {
        self.conn
            .query_row(
                "SELECT path, added_at, alias, hidden FROM workspaces WHERE path = ?1",
                params![path.display().to_string()],
                |row| {
                    Ok(WorkspaceMeta {
                        path: PathBuf::from(row.get::<_, String>(0)?),
                        added_at: row.get(1)?,
                        alias: row.get(2)?,
                        hidden: row.get(3)?,
                    })
                },
            )
            .ok()
    }

    fn put_workspace(&self, meta: &WorkspaceMeta) {
        let _ = self.conn.execute(
            "INSERT INTO workspaces (path, added_at, alias, hidden)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
                added_at = excluded.added_at,
                alias = excluded.alias,
                hidden = excluded.hidden",
            params![
                meta.path.display().to_string(),
                meta.added_at,
                meta.alias,
                meta.hidden,
            ],
        );
    }

    /// Idempotent: if it already exists, only unhide it.
    pub fn add_workspace(&self, path: &Path) {
        match self.workspace(path) {
            Some(meta) if meta.hidden => {
                self.put_workspace(&WorkspaceMeta {
                    hidden: false,
                    ..meta
                });
            }
            Some(_) => {}
            None => self.put_workspace(&WorkspaceMeta {
                path: path.to_path_buf(),
                added_at: now_secs(),
                alias: None,
                hidden: false,
            }),
        }
    }

    /// Rename the display name; create the entry if missing. alias of None restores the default directory name.
    pub fn rename_workspace(&self, path: &Path, alias: Option<String>) {
        let meta = match self.workspace(path) {
            Some(meta) => WorkspaceMeta { alias, ..meta },
            None => WorkspaceMeta {
                path: path.to_path_buf(),
                added_at: now_secs(),
                alias,
                hidden: false,
            },
        };
        self.put_workspace(&meta);
    }

    /// Removing from the sidebar = mark hidden (entry and alias kept); create a hidden entry if missing.
    pub fn hide_workspace(&self, path: &Path) {
        let meta = match self.workspace(path) {
            Some(meta) if !meta.hidden => WorkspaceMeta {
                hidden: true,
                ..meta
            },
            Some(_) => return,
            None => WorkspaceMeta {
                path: path.to_path_buf(),
                added_at: now_secs(),
                alias: None,
                hidden: true,
            },
        };
        self.put_workspace(&meta);
    }

    /// Unhide; returns whether anything changed.
    pub fn unhide_workspace(&self, path: &Path) -> bool {
        match self.workspace(path) {
            Some(meta) if meta.hidden => {
                self.put_workspace(&WorkspaceMeta {
                    hidden: false,
                    ..meta
                });
                true
            }
            _ => false,
        }
    }

    // ---- Per-call API usage ----

    pub(crate) fn record_api_usage(&self, record: &crate::api_usage::ApiUsageRecord) {
        let result = self.conn.execute(
            "INSERT INTO api_usage (
                session_id, started_at_ms, source, turn_id, step, agent_id,
                background, provider, model, api_format, duration_ms, ttft_ms,
                usage_reported, input_tokens, cache_read_tokens, output_tokens,
                reasoning_output_tokens, used_tokens, context_window, finish,
                error, retry_count, http_status, request_id, trace_id,
                response_id, cf_ray
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
                ?25, ?26, ?27
             )",
            params![
                record.session_id,
                record.started_at_ms,
                record.source,
                record.turn_id,
                record.step,
                record.agent_id,
                record.background,
                record.provider,
                record.model,
                record.api_format,
                record.duration_ms,
                record.ttft_ms,
                record.usage_reported,
                record.input_tokens,
                record.cache_read_tokens,
                record.output_tokens,
                record.reasoning_output_tokens,
                record.used_tokens,
                record.context_window,
                record.finish,
                record.error,
                record.retry_count,
                record.response.http_status,
                record.response.request_id,
                record.response.trace_id,
                record.response.response_id,
                record.response.cf_ray,
            ],
        );
        if let Err(error) = result {
            tracing::error!("record_api_usage failed for {}: {error}", record.session_id);
        }
    }

    /// Aggregate historical and per-call usage without double-counting the cutover.
    pub fn usage_stats(
        &self,
        query: &crate::usage::UsageQuery,
    ) -> Result<crate::usage::UsageStats, String> {
        crate::usage::query(&self.conn, query)
    }

    /// One page of the per-call request log, newest first.
    pub fn request_log(
        &self,
        query: &crate::usage::RequestLogQuery,
    ) -> Result<crate::usage::RequestLogPage, String> {
        crate::usage::request_log(&self.conn, query)
    }

    // ---- Token usage ----

    /// One row per finished turn; statistics aggregation (by day/model/workspace) all query this table.
    /// input_tokens is input that missed the cache; cache_read_tokens is input served from the cache.
    /// reasoning_output_tokens is the reasoning/thinking slice of output_tokens (informational split);
    /// ended_at_ms is the turn's wall-clock end. Schema changes land by deleting the db (no
    /// migrations — the user rebuilds).
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        input_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
        turn_id: Option<&str>,
    ) {
        self.record_usage_at(
            crate::model_io::now_ms(),
            session_id,
            provider,
            model,
            input_tokens,
            cache_read_tokens,
            output_tokens,
            reasoning_output_tokens,
            turn_id,
        );
    }

    /// record_usage with an explicit end stamp: crash recovery attributes the
    /// interrupted turn to the rollout file's mtime (the recovery moment would
    /// skew by-day statistics). `turn_id` makes the insert idempotent:
    /// `INSERT OR IGNORE` against the unique `(session_id, turn_id)` index
    /// means a recovery after SQLite-success/rollout-failure (or a crash
    /// window between the two) cannot add a second row.
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage_at(
        &self,
        ended_at_ms: u64,
        session_id: &str,
        provider: &str,
        model: &str,
        input_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
        turn_id: Option<&str>,
    ) {
        let result = self.conn.execute(
            "INSERT OR IGNORE INTO turn_usage (session_id, ended_at_ms, provider, model, input_tokens, cache_read_tokens, output_tokens, reasoning_output_tokens, turn_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id,
                ended_at_ms,
                provider,
                model,
                input_tokens,
                cache_read_tokens,
                output_tokens,
                reasoning_output_tokens,
                turn_id
            ],
        );
        if let Err(error) = result {
            tracing::error!("record_usage failed for {session_id}: {error}");
        }
    }

    /// Test/diagnostic accessor: usage rows for one session in insertion order.
    #[cfg(test)]
    pub(crate) fn turn_usage_rows(&self, session_id: &str) -> Vec<(String, u64, u64, u64)> {
        let Ok(mut statement) = self.conn.prepare(
            "SELECT provider, input_tokens, cache_read_tokens, output_tokens
             FROM turn_usage WHERE session_id = ?1 ORDER BY id",
        ) else {
            return Vec::new();
        };
        statement
            .query_map(params![session_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    // ---- Session todos (current state, full upsert) ----

    pub fn set_todos(&self, session_id: &str, items_json: &str) {
        let _ = self.conn.execute(
            "INSERT INTO todos (session_id, items) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET items = excluded.items",
            params![session_id, items_json],
        );
    }

    /// No record -> None (the session has never written todos)
    pub fn get_todos(&self, session_id: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT items FROM todos WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .ok()
    }

    // ---- Pending message queue (queue+steer lanes, restored on reopen) ----

    pub fn set_pending(&self, session_id: &str, items_json: &str) {
        let _ = self.conn.execute(
            "INSERT INTO pending_queue (session_id, items) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET items = excluded.items",
            params![session_id, items_json],
        );
    }

    /// No record / empty queue -> None
    pub fn get_pending(&self, session_id: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT items FROM pending_queue WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .ok()
    }

    pub fn delete_pending(&self, session_id: &str) {
        let _ = self.conn.execute(
            "DELETE FROM pending_queue WHERE session_id = ?1",
            params![session_id],
        );
    }

    // ---- Session file changes (current state, upsert by path) ----

    /// A net change of zero (content reverted to the original) should go through delete_file_change by the caller.
    pub fn upsert_file_change(
        &self,
        session_id: &str,
        path: &str,
        unified_diff: &str,
        additions: u32,
        deletions: u32,
    ) {
        let _ = self.conn.execute(
            "INSERT INTO file_changes (session_id, path, unified_diff, additions, deletions)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id, path) DO UPDATE SET
                unified_diff = excluded.unified_diff,
                additions = excluded.additions,
                deletions = excluded.deletions",
            params![session_id, path, unified_diff, additions, deletions],
        );
    }

    pub fn delete_file_change(&self, session_id: &str, path: &str) {
        let _ = self.conn.execute(
            "DELETE FROM file_changes WHERE session_id = ?1 AND path = ?2",
            params![session_id, path],
        );
    }

    /// (path, unified_diff, additions, deletions), sorted by path for stable replay order
    pub fn file_changes(&self, session_id: &str) -> Vec<(String, String, u32, u32)> {
        let mut stmt = match self.conn.prepare(
            "SELECT path, unified_diff, additions, deletions
             FROM file_changes WHERE session_id = ?1 ORDER BY path",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        });
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => vec![],
        }
    }

    // ---- Original file snapshots (cross-restart diff baseline / revert) ----

    /// content of None means the file did not originally exist (revert deletes the newly created file)
    pub fn upsert_file_original(&self, session_id: &str, path: &str, content: Option<&str>) {
        let _ = self.conn.execute(
            "INSERT INTO file_originals (session_id, path, content) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id, path) DO UPDATE SET content = excluded.content",
            params![session_id, path, content],
        );
    }

    pub fn delete_file_original(&self, session_id: &str, path: &str) {
        let _ = self.conn.execute(
            "DELETE FROM file_originals WHERE session_id = ?1 AND path = ?2",
            params![session_id, path],
        );
    }

    /// (path, original content; None = did not originally exist)
    pub fn file_originals(&self, session_id: &str) -> Vec<(String, Option<String>)> {
        let mut stmt = match self
            .conn
            .prepare("SELECT path, content FROM file_originals WHERE session_id = ?1")
        {
            Ok(stmt) => stmt,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)));
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_store(name: &str) -> (PathBuf, Store) {
        let dir =
            std::env::temp_dir().join(format!("pig-core-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let store = Store::open(&dir).expect("open store");
        (dir, store)
    }

    #[test]
    fn delete_session_cascades_all_tables() {
        let (_dir, mut store) = open_test_store("delete");
        let meta = SessionMeta {
            id: "s1".to_string(),
            title: "t".to_string(),
            title_custom: true,
            cwd: PathBuf::from("/w"),
            created_at: 1,
            updated_at: 1,
            pinned: false,
            archived: false,
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: Default::default(),
            plan_enabled: false,
        };
        store.upsert_session(&meta);
        store.record_usage("s1", "p", "m", 1, 2, 3, 4, None);
        store.set_todos("s1", "[]");
        store.upsert_file_change("s1", "a.rs", "d", 1, 1);
        store.upsert_file_original("s1", "a.rs", Some("old"));
        store
            .request_session_deletion("s1")
            .expect("request deletion");
        store.delete_session_checked("s1").expect("delete session");
        assert!(
            store.get_session("s1").is_none(),
            "the sessions row should be deleted"
        );
        assert!(
            store.sorted_sessions().is_empty(),
            "the list no longer contains the session"
        );
        assert!(
            store.get_todos("s1").is_none(),
            "todos should cascade-delete"
        );
        assert!(store.file_changes("s1").is_empty());
        assert!(store.file_originals("s1").is_empty());
        // Deleting again does not error (idempotent)
        store
            .request_session_deletion("s1")
            .expect("request deletion");
        store.delete_session_checked("s1").expect("delete session");
    }

    /// The durable turn identity makes usage recovery idempotent: a second
    /// insert for the same (session, turn) is ignored, while a different turn
    /// still adds a row. Legacy NULL identities keep their old behavior.
    #[test]
    fn turn_usage_is_idempotent_by_turn_id() {
        let (_dir, store) = open_test_store("usage-idempotent");
        store.record_usage_at(10, "s1", "p", "m", 100, 20, 30, 5, Some("turn-a"));
        store.record_usage_at(10, "s1", "p", "m", 100, 20, 30, 5, Some("turn-a"));
        store.record_usage_at(20, "s1", "p", "m", 1, 2, 3, 0, Some("turn-b"));
        store.record_usage_at(30, "s1", "p", "m", 7, 0, 0, 0, None);
        store.record_usage_at(31, "s1", "p", "m", 7, 0, 0, 0, None);
        let rows = store.turn_usage_rows("s1");
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert_eq!(rows[0], ("p".to_string(), 100, 20, 30));
    }

    #[test]
    fn title_custom_roundtrip() {
        let (_dir, store) = open_test_store("title-custom");
        let mut meta = SessionMeta {
            id: "s1".to_string(),
            title: "t".to_string(),
            title_custom: false,
            cwd: PathBuf::from("/w"),
            created_at: 1,
            updated_at: 1,
            pinned: false,
            archived: false,
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: Default::default(),
            plan_enabled: false,
        };
        store.upsert_session(&meta);
        assert!(!store.get_session("s1").unwrap().title_custom);
        meta.title_custom = true;
        store.upsert_session(&meta);
        assert!(
            store.get_session("s1").unwrap().title_custom,
            "the manual-rename flag should persist"
        );
    }

    #[test]
    fn todos_upsert_and_read_back() {
        let (_dir, store) = open_test_store("todos");
        assert_eq!(store.get_todos("s1"), None, "None when nothing was written");
        store.set_todos("s1", "[{\"content\":\"a\",\"status\":\"pending\"}]");
        store.set_todos("s1", "[{\"content\":\"a\",\"status\":\"done\"}]");
        assert_eq!(
            store.get_todos("s1").as_deref(),
            Some("[{\"content\":\"a\",\"status\":\"done\"}]"),
            "the full upsert reads back the latest snapshot"
        );
        assert_eq!(store.get_todos("s2"), None, "isolated per session");
    }

    #[test]
    fn file_changes_upsert_delete_and_order() {
        let (_dir, store) = open_test_store("changes");
        store.upsert_file_change("s1", "b.rs", "diff-b", 1, 2);
        store.upsert_file_change("s1", "a.rs", "diff-a", 3, 4);
        store.upsert_file_change("s1", "b.rs", "diff-b2", 5, 6);
        let changes = store.file_changes("s1");
        assert_eq!(
            changes,
            vec![
                ("a.rs".to_string(), "diff-a".to_string(), 3, 4),
                ("b.rs".to_string(), "diff-b2".to_string(), 5, 6),
            ],
            "same-path upsert overwrites, sorted by path"
        );
        store.delete_file_change("s1", "a.rs");
        assert_eq!(
            store.file_changes("s1").len(),
            1,
            "only one row left after deletion"
        );
        assert!(store.file_changes("s2").is_empty(), "isolated per session");
    }

    #[test]
    fn file_originals_roundtrip_with_missing_file() {
        let (_dir, store) = open_test_store("originals");
        store.upsert_file_original("s1", "/w/exists.rs", Some("old content"));
        store.upsert_file_original("s1", "/w/new.rs", None);
        let mut originals = store.file_originals("s1");
        originals.sort();
        assert_eq!(
            originals,
            vec![
                ("/w/exists.rs".to_string(), Some("old content".to_string())),
                ("/w/new.rs".to_string(), None),
            ],
            "Some = original content, None = the file did not exist"
        );
        store.delete_file_original("s1", "/w/new.rs");
        assert_eq!(store.file_originals("s1").len(), 1);
    }

    #[test]
    fn api_usage_stores_each_call_and_cascades() {
        let (_dir, mut store) = open_test_store("api-usage");
        let mut first = crate::api_usage::ApiUsageRecord {
            session_id: "s1".to_string(),
            source: "main".to_string(),
            turn_id: Some("turn-1".to_string()),
            step: Some(1),
            provider: "provider".to_string(),
            model: "model".to_string(),
            api_format: "OpenAiChat".to_string(),
            started_at_ms: 1234,
            duration_ms: 45,
            ttft_ms: Some(12),
            finish: "tool_calls".to_string(),
            ..Default::default()
        };
        first.set_usage(Some(pig_provider::ProviderUsage {
            input: 10,
            cache_read: 20,
            output: 30,
            used: 60,
            total: 1000,
            reasoning_output: 4,
        }));
        first.response = pig_provider::ProviderResponseMetadata {
            http_status: Some(200),
            request_id: Some("request-1".to_string()),
            trace_id: Some("trace-1".to_string()),
            response_id: Some("response-1".to_string()),
            cf_ray: Some("ray-1".to_string()),
        };
        store.record_api_usage(&first);

        let second = crate::api_usage::ApiUsageRecord {
            session_id: "s1".to_string(),
            source: "main".to_string(),
            turn_id: Some("turn-1".to_string()),
            step: Some(2),
            provider: "provider".to_string(),
            model: "model".to_string(),
            api_format: "OpenAiChat".to_string(),
            started_at_ms: 1300,
            duration_ms: 5,
            finish: "error".to_string(),
            error: Some("boom".to_string()),
            ..Default::default()
        };
        store.record_api_usage(&second);

        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM api_usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2, "two API steps in one turn remain separate rows");
        let row = store.conn.query_row(
            "SELECT turn_id, step, usage_reported, input_tokens, cache_read_tokens,
                    output_tokens, reasoning_output_tokens, used_tokens, context_window,
                    request_id, trace_id, response_id, cf_ray
             FROM api_usage WHERE step = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                ))
            },
        );
        let (
            turn_id,
            step,
            usage_reported,
            input,
            cache_read,
            output,
            reasoning,
            used,
            context_window,
            request_id,
            trace_id,
            response_id,
            cf_ray,
        ) = row.unwrap();
        assert_eq!(turn_id.as_deref(), Some("turn-1"));
        assert_eq!(step, Some(1));
        assert_eq!(usage_reported, 1);
        assert_eq!((input, cache_read, output), (10, 20, 30));
        assert_eq!((reasoning, used, context_window), (4, 60, 1000));
        assert_eq!(request_id.as_deref(), Some("request-1"));
        assert_eq!(trace_id.as_deref(), Some("trace-1"));
        assert_eq!(response_id.as_deref(), Some("response-1"));
        assert_eq!(cf_ray.as_deref(), Some("ray-1"));
        let missing_usage: i64 = store
            .conn
            .query_row(
                "SELECT usage_reported FROM api_usage WHERE step = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(missing_usage, 0);

        store
            .request_session_deletion("s1")
            .expect("request deletion");
        store.delete_session_checked("s1").expect("delete session");
        let remaining: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM api_usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn sqlite_migrations_set_user_version_upgrade_legacy_v0_and_reject_future_versions() {
        let dir = std::env::temp_dir().join(format!(
            "pig-store-migrate-{}-{}",
            std::process::id(),
            crate::model_io::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        // Fresh database stamps CURRENT_SCHEMA_VERSION and reopens idempotently.
        let store = Store::open(&dir).expect("open fresh store");
        assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        drop(store);
        let reopened = Store::open(&dir).expect("reopen store");
        assert_eq!(reopened.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        drop(reopened);

        // Legacy v0 database (user_version == 0 with existing tables) upgrades cleanly.
        {
            let conn = Connection::open(dir.join("store.sqlite")).expect("raw conn");
            conn.pragma_update(None, "user_version", 0u32).expect("reset v0");
        }
        let upgraded = Store::open(&dir).expect("upgrade legacy v0 store");
        assert_eq!(upgraded.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
        drop(upgraded);

        // Future schema version fails closed instead of corrupting unknown tables.
        {
            let conn = Connection::open(dir.join("store.sqlite")).expect("raw conn");
            conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
                .expect("bump future version");
        }
        let err = Store::open(&dir)
            .err()
            .expect("future schema version must be rejected");
        assert!(err.contains("newer than supported version"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
