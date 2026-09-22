//! SQLite 存储：{data_dir}/store.sqlite
//! - sessions 表：会话索引（置顶/归档/标题都改这里）
//! - workspaces 表：工作区注册表（别名、隐藏标记）
//! - turn_usage 表：每回合 token 用量，供统计聚合
//! - todos 表：会话待办清单当前态（整体 upsert；事件流不再落 JSONL）
//! - file_changes 表：会话文件改动当前态（按路径 upsert，净额归零删行）
//! - file_originals 表：改动文件的原始内容快照（跨重启 diff 基线 / revert）

use std::path::{Path, PathBuf};

use pig_protocol::{SessionMeta, WorkspaceMeta};
use rusqlite::{Connection, params};

use crate::rollout::now_secs;

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
        let path = data_dir.join("store.sqlite");
        let conn = Connection::open(&path)
            .map_err(|e| format!("打开 store.sqlite 失败 {}: {e}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .and_then(|_| conn.pragma_update(None, "busy_timeout", 5000u64))
            .map_err(|e| format!("store.sqlite pragma 失败: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                cwd TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                pinned INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                provider_id TEXT,
                model_id TEXT,
                reasoning_level TEXT,
                exec_mode TEXT NOT NULL DEFAULT 'ConfirmBeforeEdit'
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
        .map_err(|e| format!("store.sqlite 建表失败: {e}"))?;
        Ok(Self { conn })
    }

    // ---- 会话索引 ----

    /// exec_mode 存 serde 变体名（"AutoEdit" 等），解析失败回退默认
    fn mode_from_row(raw: String) -> pig_protocol::ExecMode {
        serde_json::from_str(&format!("\"{raw}\"")).unwrap_or_default()
    }

    fn session_from_row(row: &rusqlite::Row) -> rusqlite::Result<SessionMeta> {
        Ok(SessionMeta {
            id: row.get(0)?,
            title: row.get(1)?,
            cwd: PathBuf::from(row.get::<_, String>(2)?),
            created_at: row.get(3)?,
            updated_at: row.get(4)?,
            pinned: row.get(5)?,
            archived: row.get(6)?,
            provider_id: row.get(7)?,
            model_id: row.get(8)?,
            reasoning_level: row.get(9)?,
            exec_mode: Self::mode_from_row(row.get::<_, String>(10)?),
        })
    }

    const SESSION_COLUMNS: &'static str =
        "id, title, cwd, created_at, updated_at, pinned, archived, provider_id, model_id, reasoning_level, exec_mode";

    pub fn upsert_session(&self, meta: &SessionMeta) {
        // exec_mode 存变体名（"AutoEdit" 等），读出时按 serde 变体名解析
        let mode_raw = format!("{:?}", meta.exec_mode);
        let _ = self.conn.execute(
            "INSERT INTO sessions (id, title, cwd, created_at, updated_at, pinned, archived, provider_id, model_id, reasoning_level, exec_mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                cwd = excluded.cwd,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at,
                pinned = excluded.pinned,
                archived = excluded.archived,
                provider_id = excluded.provider_id,
                model_id = excluded.model_id,
                reasoning_level = excluded.reasoning_level,
                exec_mode = excluded.exec_mode",
            params![
                meta.id,
                meta.title,
                meta.cwd.display().to_string(),
                meta.created_at,
                meta.updated_at,
                meta.pinned,
                meta.archived,
                meta.provider_id,
                meta.model_id,
                meta.reasoning_level,
                mode_raw,
            ],
        );
    }

    /// 读出-修改-写回；会话不存在则不动。
    pub fn update_session(&self, id: &str, f: impl FnOnce(&mut SessionMeta)) {
        if let Some(mut meta) = self.get_session(id) {
            f(&mut meta);
            self.upsert_session(&meta);
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

    /// 按 updated_at 倒序（侧栏列表顺序）。
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

    /// 工作区最近活跃的会话（新会话继承模型/模式/思考等级的种子）；归档的不算活跃
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

    // ---- 工作区注册表 ----

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

    /// 幂等：已存在则仅取消隐藏。
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

    /// 重命名显示名；不存在则补建条目。alias 为 None 恢复默认目录名。
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

    /// 从侧栏移除 = 置为隐藏（条目与别名保留）；不存在则补建隐藏条目。
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

    /// 取消隐藏；返回是否有变化。
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

    // ---- token 用量 ----

    /// 每回合结束记一行；统计聚合（按天/模型/工作区）都查这张表。
    pub fn record_usage(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) {
        let _ = self.conn.execute(
            "INSERT INTO turn_usage (session_id, ts, provider, model, input_tokens, output_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id,
                now_secs(),
                provider,
                model,
                input_tokens,
                output_tokens
            ],
        );
    }

    // ---- 会话待办（当前态，整体 upsert）----

    pub fn set_todos(&self, session_id: &str, items_json: &str) {
        let _ = self.conn.execute(
            "INSERT INTO todos (session_id, items) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET items = excluded.items",
            params![session_id, items_json],
        );
    }

    /// 无记录 → None（会话还没有过待办写入）
    pub fn get_todos(&self, session_id: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT items FROM todos WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .ok()
    }

    // ---- 会话文件改动（当前态，按路径 upsert）----

    /// 净额归零（改回原始内容）应由调用方走 delete_file_change。
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

    /// (path, unified_diff, additions, deletions)，按路径排序保证回放顺序稳定
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

    // ---- 文件原始快照（跨重启 diff 基线 / revert）----

    /// content 为 None 表示文件原本不存在（revert 时删除新建文件）
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

    /// (path, 原始内容；None = 原本不存在)
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
        let dir = std::env::temp_dir().join(format!(
            "pig-core-store-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let store = Store::open(&dir).expect("打开 store");
        (dir, store)
    }

    #[test]
    fn todos_upsert_and_read_back() {
        let (_dir, store) = open_test_store("todos");
        assert_eq!(store.get_todos("s1"), None, "无写入时应为 None");
        store.set_todos("s1", "[{\"content\":\"a\",\"status\":\"pending\"}]");
        store.set_todos("s1", "[{\"content\":\"a\",\"status\":\"done\"}]");
        assert_eq!(
            store.get_todos("s1").as_deref(),
            Some("[{\"content\":\"a\",\"status\":\"done\"}]"),
            "整体 upsert 后读到最新快照"
        );
        assert_eq!(store.get_todos("s2"), None, "按会话隔离");
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
            "同路径 upsert 覆盖、按路径排序"
        );
        store.delete_file_change("s1", "a.rs");
        assert_eq!(store.file_changes("s1").len(), 1, "删除后只剩一条");
        assert!(store.file_changes("s2").is_empty(), "按会话隔离");
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
            "Some=原有内容，None=文件原本不存在"
        );
        store.delete_file_original("s1", "/w/new.rs");
        assert_eq!(store.file_originals("s1").len(), 1);
    }
}
