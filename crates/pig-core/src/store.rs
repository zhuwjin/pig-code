//! SQLite storage: {data_dir}/store.sqlite
//! - sessions table: session index (pin/archive/title all write here)
//! - workspaces table: workspace registry (alias, hidden flag)
//! - turn_usage table: per-turn token usage for statistics aggregation
//! - todos table: current session todo-list state (full upsert; the event stream no longer persists to JSONL)
//! - file_changes table: current session file-change state (upsert by path; rows deleted when the net change is zero)
//! - file_originals table: original-content snapshots of changed files (cross-restart diff baseline / revert)

use std::path::{Path, PathBuf};

use pig_protocol::{SessionMeta, WorkspaceMeta};
use rusqlite::{Connection, params};

use crate::rollout::now_secs;

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| format!("Failed to create data directory: {e}"))?;
        let path = data_dir.join("store.sqlite");
        let conn = Connection::open(&path)
            .map_err(|e| format!("Failed to open store.sqlite {}: {e}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .and_then(|_| conn.pragma_update(None, "busy_timeout", 5000u64))
            .map_err(|e| format!("store.sqlite pragma failed: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
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
                exec_mode TEXT NOT NULL DEFAULT 'ConfirmBeforeEdit',
                plan_enabled INTEGER NOT NULL DEFAULT 0,
                fs_read_outside INTEGER NOT NULL DEFAULT 0,
                fs_write_outside INTEGER NOT NULL DEFAULT 0
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
                ts INTEGER NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_turn_usage_ts ON turn_usage(ts);
            CREATE INDEX IF NOT EXISTS idx_turn_usage_session ON turn_usage(session_id);
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
            CREATE TABLE IF NOT EXISTS file_originals (
                session_id TEXT NOT NULL,
                path TEXT NOT NULL,
                content TEXT,
                PRIMARY KEY (session_id, path)
            );",
        )
        .map_err(|e| format!("store.sqlite create-table failed: {e}"))?;
        Ok(Self { conn })
    }

    // ---- Session index ----

    /// exec_mode is stored as the serde variant name ("AutoEdit" etc.); parse failure falls back to the default
    fn mode_from_row(raw: String) -> pig_protocol::ExecMode {
        serde_json::from_str(&format!("\"{raw}\"")).unwrap_or_default()
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
            exec_mode: Self::mode_from_row(row.get::<_, String>(11)?),
            plan_enabled: row.get::<_, i64>(12)? != 0,
            fs_read_outside: row.get::<_, i64>(13)? != 0,
            fs_write_outside: row.get::<_, i64>(14)? != 0,
        })
    }

    const SESSION_COLUMNS: &'static str = "id, title, title_custom, cwd, created_at, updated_at, pinned, archived, provider_id, model_id, reasoning_level, exec_mode, plan_enabled, fs_read_outside, fs_write_outside";

    pub fn upsert_session(&self, meta: &SessionMeta) {
        // exec_mode is stored as the variant name ("AutoEdit" etc.) and parsed back as a serde variant name on read
        let mode_raw = format!("{:?}", meta.exec_mode);
        let result = self.conn.execute(
            "INSERT INTO sessions (id, title, title_custom, cwd, created_at, updated_at, pinned, archived, provider_id, model_id, reasoning_level, exec_mode, plan_enabled, fs_read_outside, fs_write_outside)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
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
                exec_mode = excluded.exec_mode,
                plan_enabled = excluded.plan_enabled,
                fs_read_outside = excluded.fs_read_outside,
                fs_write_outside = excluded.fs_write_outside",
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
                mode_raw,
                meta.plan_enabled,
                meta.fs_read_outside,
                meta.fs_write_outside,
            ],
        );
        // Write failures must not be silent (a missing column once dropped whole batches of new sessions): at least log to the console
        if let Err(error) = result {
            eprintln!("[store] upsert_session failed for {}: {error}", meta.id);
        }
    }

    /// Read-modify-write; no-op when the session does not exist.
    pub fn update_session(&self, id: &str, f: impl FnOnce(&mut SessionMeta)) {
        if let Some(mut meta) = self.get_session(id) {
            f(&mut meta);
            self.upsert_session(&meta);
        }
    }

    /// Delete a session: cascade-clears turn_usage / todos / file_changes / file_originals.
    /// The rollout JSONL file is deleted by the caller (the handle may still be in use mid-turn)
    pub fn delete_session(&self, id: &str) {
        let _ = self
            .conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id]);
        for table in ["turn_usage", "todos", "file_changes", "file_originals"] {
            let _ = self.conn.execute(
                &format!("DELETE FROM {table} WHERE session_id = ?1"),
                params![id],
            );
        }
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
            "SELECT {} FROM sessions ORDER BY updated_at DESC",
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

    // ---- Token usage ----

    /// One row per finished turn; statistics aggregation (by day/model/workspace) all query this table.
    /// input_tokens is input that missed the cache; cache_read_tokens is input served from the cache.
    pub fn record_usage(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        input_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
    ) {
        let _ = self.conn.execute(
            "INSERT INTO turn_usage (session_id, ts, provider, model, input_tokens, cache_read_tokens, output_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                now_secs(),
                provider,
                model,
                input_tokens,
                cache_read_tokens,
                output_tokens
            ],
        );
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
        let (_dir, store) = open_test_store("delete");
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
            exec_mode: Default::default(),
            plan_enabled: false,
            fs_read_outside: false,
            fs_write_outside: false,
        };
        store.upsert_session(&meta);
        store.record_usage("s1", "p", "m", 1, 2, 3);
        store.set_todos("s1", "[]");
        store.upsert_file_change("s1", "a.rs", "d", 1, 1);
        store.upsert_file_original("s1", "a.rs", Some("old"));
        store.delete_session("s1");
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
        store.delete_session("s1");
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
            exec_mode: Default::default(),
            plan_enabled: false,
            fs_read_outside: false,
            fs_write_outside: false,
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
    fn fs_access_columns_roundtrip_and_reopen() {
        let (dir, store) = open_test_store("fs-access");
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
            exec_mode: Default::default(),
            plan_enabled: false,
            fs_read_outside: true,
            fs_write_outside: false,
        };
        store.upsert_session(&meta);
        let read = store.get_session("s1").expect("readable after write");
        assert!(
            read.fs_read_outside && !read.fs_write_outside,
            "new columns read back"
        );
        assert!(!read.plan_enabled, "plan_enabled defaults to off");

        meta.fs_write_outside = true;
        store.upsert_session(&meta);
        let read = store.get_session("s1").unwrap();
        assert!(
            read.fs_read_outside && read.fs_write_outside,
            "updates write through"
        );

        // Reopen the same database: CREATE TABLE IF NOT EXISTS is idempotent, data remains
        drop(store);
        let store = Store::open(&dir).expect("reopening the same database should not error");
        let read = store.get_session("s1").unwrap();
        assert!(
            read.fs_read_outside && read.fs_write_outside,
            "still present after reopen"
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
}
