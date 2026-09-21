//! SQLite 存储：{data_dir}/store.sqlite
//! - sessions 表：会话索引（置顶/归档/标题都改这里）
//! - workspaces 表：工作区注册表（别名、隐藏标记）
//! - turn_usage 表：每回合 token 用量，供统计聚合

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
                archived INTEGER NOT NULL DEFAULT 0
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
            CREATE INDEX IF NOT EXISTS idx_turn_usage_session ON turn_usage(session_id);",
        )
        .map_err(|e| format!("store.sqlite 建表失败: {e}"))?;
        Ok(Self { conn })
    }

    // ---- 会话索引 ----

    pub fn upsert_session(&self, meta: &SessionMeta) {
        let _ = self.conn.execute(
            "INSERT INTO sessions (id, title, cwd, created_at, updated_at, pinned, archived)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                cwd = excluded.cwd,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at,
                pinned = excluded.pinned,
                archived = excluded.archived",
            params![
                meta.id,
                meta.title,
                meta.cwd.display().to_string(),
                meta.created_at,
                meta.updated_at,
                meta.pinned,
                meta.archived,
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
                "SELECT id, title, cwd, created_at, updated_at, pinned, archived
                 FROM sessions WHERE id = ?1",
                params![id],
                |row| {
                    Ok(SessionMeta {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        cwd: PathBuf::from(row.get::<_, String>(2)?),
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                        pinned: row.get(5)?,
                        archived: row.get(6)?,
                    })
                },
            )
            .ok()
    }

    /// 按 updated_at 倒序（侧栏列表顺序）。
    pub fn sorted_sessions(&self) -> Vec<SessionMeta> {
        let mut stmt = match self.conn.prepare(
            "SELECT id, title, cwd, created_at, updated_at, pinned, archived
             FROM sessions ORDER BY updated_at DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map([], |row| {
            Ok(SessionMeta {
                id: row.get(0)?,
                title: row.get(1)?,
                cwd: PathBuf::from(row.get::<_, String>(2)?),
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                pinned: row.get(5)?,
                archived: row.get(6)?,
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => vec![],
        }
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
}
