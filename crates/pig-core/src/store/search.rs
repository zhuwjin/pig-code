use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{
    Connection, OptionalExtension, Transaction, params, params_from_iter, types::Value,
};

use super::Store;
use crate::rollout::{
    ContentHit, ContentIndexRecord, ContentIndexSnapshot, content_snippet,
    read_content_index_snapshot,
};

const SEARCH_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS session_search_records (
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    record_offset INTEGER NOT NULL,
    kind TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    cwd TEXT NOT NULL,
    UNIQUE(session_id, record_offset)
);
CREATE INDEX IF NOT EXISTS idx_session_search_session
    ON session_search_records(session_id);
CREATE INDEX IF NOT EXISTS idx_session_search_filters
    ON session_search_records(cwd, created_at_ms);
CREATE VIRTUAL TABLE IF NOT EXISTS session_search_fts USING fts5(
    content,
    content='session_search_records',
    content_rowid='id',
    tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS session_search_records_ai
AFTER INSERT ON session_search_records BEGIN
    INSERT INTO session_search_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER IF NOT EXISTS session_search_records_ad
AFTER DELETE ON session_search_records BEGIN
    INSERT INTO session_search_fts(session_search_fts, rowid, content)
        VALUES ('delete', old.id, old.content);
END;
CREATE TRIGGER IF NOT EXISTS session_search_records_au
AFTER UPDATE ON session_search_records BEGIN
    INSERT INTO session_search_fts(session_search_fts, rowid, content)
        VALUES ('delete', old.id, old.content);
    INSERT INTO session_search_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TABLE IF NOT EXISTS session_search_files (
    session_id TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime_ms INTEGER NOT NULL,
    record_count INTEGER NOT NULL
);
";

pub(super) fn create_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(SEARCH_SCHEMA)
        .map_err(|error| format!("store.sqlite v2 session search schema failed: {error}"))
}

pub(super) fn ensure_schema(conn: &Connection) -> Result<(), String> {
    let fts_missing = !conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_search_fts')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| format!("store.sqlite inspect session search schema failed: {error}"))?;
    create_schema(conn)?;
    if fts_missing {
        conn.execute(
            "INSERT INTO session_search_fts(session_search_fts) VALUES ('rebuild')",
            [],
        )
        .map_err(|error| format!("store.sqlite rebuild session FTS index failed: {error}"))?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SessionSearchQuery {
    pub text: String,
    pub workspace: Option<PathBuf>,
    /// Inclusive unix-millisecond bounds. Legacy records with timestamp zero
    /// remain visible only when no lower bound is requested.
    pub from_created_at_ms: Option<u64>,
    pub to_created_at_ms: Option<u64>,
    pub exclude_session_ids: Vec<String>,
    pub limit: usize,
    pub offset: usize,
}

impl SessionSearchQuery {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }
}

impl Default for SessionSearchQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            workspace: None,
            from_created_at_ms: None,
            to_created_at_ms: None,
            exclude_session_ids: Vec::new(),
            limit: 100,
            offset: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub workspace: PathBuf,
    pub kind: String,
    pub created_at_ms: u64,
    pub record_offset: u64,
    pub before: String,
    pub hit: String,
    pub after: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SearchFileState {
    size: i64,
    mtime_ms: i64,
    record_count: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileSignature {
    size: i64,
    mtime_ms: i64,
}

fn file_signature(path: &Path) -> Result<FileSignature, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("inspect search source {} failed: {error}", path.display()))?;
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    Ok(FileSignature {
        size: metadata.len().min(i64::MAX as u64) as i64,
        mtime_ms,
    })
}

fn read_file_state(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<SearchFileState>, rusqlite::Error> {
    conn.query_row(
        "SELECT size, mtime_ms, record_count FROM session_search_files WHERE session_id = ?1",
        params![session_id],
        |row| {
            Ok(SearchFileState {
                size: row.get(0)?,
                mtime_ms: row.get(1)?,
                record_count: row.get(2)?,
            })
        },
    )
    .optional()
}

fn indexed_record_count(conn: &Connection, session_id: &str) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM session_search_records WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    )
}

fn write_file_state(
    tx: &Transaction<'_>,
    session_id: &str,
    signature: FileSignature,
) -> Result<(), rusqlite::Error> {
    let record_count = indexed_record_count(tx, session_id)?;
    tx.execute(
        "INSERT INTO session_search_files (session_id, size, mtime_ms, record_count)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO UPDATE SET
             size = excluded.size,
             mtime_ms = excluded.mtime_ms,
             record_count = excluded.record_count",
        params![session_id, signature.size, signature.mtime_ms, record_count],
    )?;
    Ok(())
}

fn insert_record(
    tx: &Transaction<'_>,
    session_id: &str,
    cwd: &Path,
    record: &ContentIndexRecord,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "INSERT INTO session_search_records (
             session_id, record_offset, kind, content, created_at_ms, cwd
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(session_id, record_offset) DO UPDATE SET
             kind = excluded.kind,
             content = excluded.content,
             created_at_ms = excluded.created_at_ms,
             cwd = excluded.cwd",
        params![
            session_id,
            record.offset.min(i64::MAX as u64) as i64,
            record.kind,
            record.content,
            record.created_at_ms.min(i64::MAX as u64) as i64,
            cwd.display().to_string(),
        ],
    )?;
    Ok(())
}

/// Build an FTS5 trigram MATCH expression when the query contains at least one
/// 3+ character token whose case folding matches ASCII/caseless trigram lookup.
/// Multi-word queries AND their 3+ character tokens so whitespace-collapsed
/// matches across newlines still hit the inverted index before `content_snippet`
/// verifies the exact literal substring.
fn fts_match_query(text: &str) -> Option<String> {
    let tokens = text
        .split_whitespace()
        .filter(|part| {
            part.chars().count() >= 3
                && part
                    .chars()
                    .all(|ch| ch.is_ascii() || ch.to_lowercase().eq(ch.to_uppercase()))
        })
        .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    (!tokens.is_empty()).then(|| tokens.join(" "))
}

impl Store {
    /// Incrementally persist one appended rollout record and its source cursor.
    /// Non-searchable records still advance the cursor so unchanged files are
    /// not reparsed at every launch. Errors are returned to a best-effort caller;
    /// the JSONL append itself is never rolled back because of a derived index.
    pub(crate) fn index_session_record(
        &mut self,
        session_id: &str,
        cwd: &Path,
        record: Option<&ContentIndexRecord>,
        rollout_path: &Path,
    ) -> Result<(), String> {
        let signature = file_signature(rollout_path)?;
        let tx = self
            .conn
            .transaction()
            .map_err(|error| format!("begin search increment failed: {error}"))?;
        if let Some(record) = record {
            insert_record(&tx, session_id, cwd, record)
                .map_err(|error| format!("index session {session_id} record failed: {error}"))?;
        }
        write_file_state(&tx, session_id, signature).map_err(|error| {
            format!("advance session {session_id} search cursor failed: {error}")
        })?;
        tx.commit().map_err(|error| {
            format!("commit session {session_id} search increment failed: {error}")
        })
    }

    pub(crate) fn replace_session_search_snapshot(
        &mut self,
        snapshot: &ContentIndexSnapshot,
        rollout_path: &Path,
    ) -> Result<(), String> {
        let signature = file_signature(rollout_path)?;
        self.replace_snapshot_inner(snapshot, signature, None)
    }

    fn replace_snapshot_inner(
        &mut self,
        snapshot: &ContentIndexSnapshot,
        signature: FileSignature,
        expected_state: Option<Option<SearchFileState>>,
    ) -> Result<(), String> {
        let tx = self
            .conn
            .transaction()
            .map_err(|error| format!("begin search rebuild failed: {error}"))?;
        if let Some(expected) = expected_state {
            let current = read_file_state(&tx, &snapshot.session_id)
                .map_err(|error| format!("check concurrent search update failed: {error}"))?;
            if current != expected {
                return Err("concurrent update".to_string());
            }
            let deleting: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM session_deletions WHERE session_id = ?1)",
                    params![snapshot.session_id],
                    |row| row.get(0),
                )
                .map_err(|error| {
                    format!("check session deletion during backfill failed: {error}")
                })?;
            if deleting {
                return Err("session deletion pending".to_string());
            }
        }
        tx.execute(
            "DELETE FROM session_search_records WHERE session_id = ?1",
            params![snapshot.session_id],
        )
        .map_err(|error| format!("clear session search records failed: {error}"))?;
        for record in &snapshot.records {
            insert_record(&tx, &snapshot.session_id, &snapshot.cwd, record)
                .map_err(|error| format!("rebuild session search record failed: {error}"))?;
        }
        write_file_state(&tx, &snapshot.session_id, signature)
            .map_err(|error| format!("write rebuilt search cursor failed: {error}"))?;
        tx.commit()
            .map_err(|error| format!("commit rebuilt search index failed: {error}"))
    }

    pub(crate) fn delete_session_search(&self, session_id: &str) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM session_search_records WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|error| format!("delete session FTS records failed: {error}"))?;
        self.conn
            .execute(
                "DELETE FROM session_search_files WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|error| format!("delete session FTS cursor failed: {error}"))?;
        Ok(())
    }

    /// Query message-granularity hits for the desktop and future read-only Bot
    /// retrieval. Any query with a 3+ character ASCII/CJK token (including code
    /// identifiers, punctuation and multi-word phrases) narrows candidates
    /// through the FTS5 trigram index before `content_snippet` verifies the
    /// exact literal substring; 1-2 character queries fall back to a
    /// parameterized table scan so CJK short terms and single symbols stay
    /// literal.
    pub fn search_sessions(
        &self,
        query: &SessionSearchQuery,
    ) -> Result<Vec<SessionSearchHit>, String> {
        if query.text.is_empty() || query.limit == 0 {
            return Ok(Vec::new());
        }
        let (sql, parameters) = self.build_search_sql(query);
        let mut statement = self
            .conn
            .prepare(&sql)
            .map_err(|error| format!("prepare session search failed: {error}"))?;
        let mut rows = statement
            .query(params_from_iter(parameters))
            .map_err(|error| format!("query session FTS failed: {error}"))?;
        let mut hits = Vec::new();
        let mut skipped = 0usize;
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("read session search row failed: {error}"))?
        {
            let session_id: String = row
                .get(0)
                .map_err(|error| format!("read session_id failed: {error}"))?;
            let kind: String = row
                .get(1)
                .map_err(|error| format!("read kind failed: {error}"))?;
            let content: String = row
                .get(2)
                .map_err(|error| format!("read content failed: {error}"))?;
            let created_at_ms: i64 = row
                .get(3)
                .map_err(|error| format!("read created_at_ms failed: {error}"))?;
            let cwd: String = row
                .get(4)
                .map_err(|error| format!("read cwd failed: {error}"))?;
            let record_offset: i64 = row
                .get(5)
                .map_err(|error| format!("read record_offset failed: {error}"))?;
            let Some((before, hit, after)) = content_snippet(&content, &query.text) else {
                continue;
            };
            if skipped < query.offset {
                skipped += 1;
                continue;
            }
            hits.push(SessionSearchHit {
                session_id,
                workspace: PathBuf::from(cwd),
                kind,
                created_at_ms: created_at_ms.max(0) as u64,
                record_offset: record_offset.max(0) as u64,
                before,
                hit,
                after,
            });
            if hits.len() >= query.limit {
                break;
            }
        }
        Ok(hits)
    }

    fn build_search_sql(&self, query: &SessionSearchQuery) -> (String, Vec<Value>) {
        let fts_expr = fts_match_query(&query.text);
        let mut parameters = Vec::<Value>::new();
        let mut sql = if let Some(expr) = fts_expr.as_ref() {
            parameters.push(Value::Text(expr.clone()));
            "SELECT r.session_id, r.kind, r.content, r.created_at_ms, r.cwd, r.record_offset
             FROM session_search_fts
             JOIN session_search_records AS r ON r.id = session_search_fts.rowid
             WHERE session_search_fts MATCH ?1"
                .to_string()
        } else {
            "SELECT r.session_id, r.kind, r.content, r.created_at_ms, r.cwd, r.record_offset
             FROM session_search_records AS r
             WHERE 1 = 1"
                .to_string()
        };
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM sessions AS s WHERE s.id = r.session_id)
              AND NOT EXISTS (
                  SELECT 1 FROM session_deletions AS d WHERE d.session_id = r.session_id
              )",
        );
        if let Some(workspace) = &query.workspace {
            parameters.push(Value::Text(workspace.display().to_string()));
            sql.push_str(&format!(" AND r.cwd = ?{}", parameters.len()));
        }
        if let Some(from) = query.from_created_at_ms {
            parameters.push(Value::Integer(from.min(i64::MAX as u64) as i64));
            sql.push_str(&format!(" AND r.created_at_ms >= ?{}", parameters.len()));
        }
        if let Some(to) = query.to_created_at_ms {
            parameters.push(Value::Integer(to.min(i64::MAX as u64) as i64));
            sql.push_str(&format!(" AND r.created_at_ms <= ?{}", parameters.len()));
        }
        if !query.exclude_session_ids.is_empty() {
            sql.push_str(" AND r.session_id NOT IN (");
            for (index, session_id) in query.exclude_session_ids.iter().enumerate() {
                if index > 0 {
                    sql.push_str(", ");
                }
                parameters.push(Value::Text(session_id.clone()));
                sql.push_str(&format!("?{}", parameters.len()));
            }
            sql.push(')');
        }
        if fts_expr.is_some() {
            sql.push_str(
                " ORDER BY bm25(session_search_fts), r.created_at_ms DESC, r.record_offset DESC",
            );
        } else {
            sql.push_str(" ORDER BY r.created_at_ms DESC, r.record_offset DESC, r.id DESC");
        }
        (sql, parameters)
    }

    /// Existing desktop JSON contract: one hit per session, deduplicated while
    /// streaming so one long session with many matches cannot crowd out other
    /// sessions.
    pub fn search_session_content(&self, text: &str) -> Result<Vec<ContentHit>, String> {
        const MAX_SESSIONS: usize = 500;
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let query = SessionSearchQuery::new(text);
        let (sql, parameters) = self.build_search_sql(&query);
        let mut statement = self
            .conn
            .prepare(&sql)
            .map_err(|error| format!("prepare session search failed: {error}"))?;
        let mut rows = statement
            .query(params_from_iter(parameters))
            .map_err(|error| format!("query session FTS failed: {error}"))?;
        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("read session search row failed: {error}"))?
        {
            let session_id: String = row
                .get(0)
                .map_err(|error| format!("read session_id failed: {error}"))?;
            if seen.contains(&session_id) {
                continue;
            }
            let content: String = row
                .get(2)
                .map_err(|error| format!("read content failed: {error}"))?;
            let Some((before, hit, after)) = content_snippet(&content, text) else {
                continue;
            };
            seen.insert(session_id.clone());
            hits.push(ContentHit {
                session_id,
                before,
                hit,
                after,
            });
            if hits.len() >= MAX_SESSIONS {
                break;
            }
        }
        Ok(hits)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchBackfillReport {
    pub scanned: usize,
    pub unchanged: usize,
    pub rebuilt: usize,
    pub failed: usize,
}

/// Incrementally backfill historical rollouts. File parsing happens without
/// the Store lock, and a source-state compare prevents an old snapshot from
/// overwriting a concurrent live append.
pub fn backfill_session_search(
    store: &Arc<Mutex<Store>>,
    sessions_dir: &Path,
) -> SearchBackfillReport {
    let mut report = SearchBackfillReport::default();
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return report;
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".jsonl") && !name.ends_with(".model-io.jsonl"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        report.scanned += 1;
        let Some(session_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string)
        else {
            report.failed += 1;
            continue;
        };
        let signature = match file_signature(&path) {
            Ok(signature) => signature,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "skip unreadable search source");
                report.failed += 1;
                continue;
            }
        };
        let (expected_state, unchanged) = {
            let guard = store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let state = match read_file_state(&guard.conn, &session_id) {
                Ok(state) => state,
                Err(error) => {
                    tracing::warn!(%error, %session_id, "read search backfill cursor failed");
                    report.failed += 1;
                    continue;
                }
            };
            let unchanged = state.is_some_and(|state| {
                state.size == signature.size
                    && state.mtime_ms == signature.mtime_ms
                    && indexed_record_count(&guard.conn, &session_id)
                        .is_ok_and(|count| count == state.record_count)
            });
            (state, unchanged)
        };
        if unchanged {
            report.unchanged += 1;
            continue;
        }
        let snapshot = match read_content_index_snapshot(&path) {
            Ok(snapshot) if snapshot.session_id == session_id => snapshot,
            Ok(snapshot) => {
                tracing::warn!(path = %path.display(), id = %snapshot.session_id, "search source id does not match its filename");
                report.failed += 1;
                continue;
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), error = ?error, "search backfill could not parse rollout");
                report.failed += 1;
                continue;
            }
        };
        if !matches!(file_signature(&path), Ok(current) if current == signature) {
            report.unchanged += 1;
            continue;
        }
        let result = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace_snapshot_inner(&snapshot, signature, Some(expected_state));
        match result {
            Ok(()) => report.rebuilt += 1,
            Err(error) if error == "concurrent update" || error == "session deletion pending" => {
                report.unchanged += 1;
            }
            Err(error) => {
                tracing::warn!(%error, %session_id, "search backfill write failed");
                report.failed += 1;
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollout::{Rollout, RolloutRecord};
    use pig_protocol::SessionMeta;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pig-core-store-search-{name}-{}-{}",
            std::process::id(),
            crate::model_io::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn meta(id: &str, cwd: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            title: id.to_string(),
            title_custom: false,
            cwd: PathBuf::from(cwd),
            created_at: 1,
            updated_at: 1,
            pinned: false,
            archived: false,
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: Default::default(),
            plan_enabled: false,
        }
    }

    fn user(text: &str, sent_at_ms: u64) -> RolloutRecord {
        RolloutRecord::User {
            text: text.to_string(),
            files: vec![],
            images: vec![],
            sent_at_ms,
            sent_text: None,
            sent_images: vec![],
            model_text: None,
            sent_input_image: None,
        }
    }

    fn assistant(text: &str, ts_ms: u64) -> RolloutRecord {
        RolloutRecord::Assistant {
            text: text.to_string(),
            reasoning: None,
            replay: None,
            tool_calls: vec![],
            tool_execution_ids: vec![],
            ts_ms,
            reasoning_ms: None,
        }
    }

    fn append_live(
        store: &mut Store,
        rollout: &mut Rollout,
        meta: &SessionMeta,
        record: RolloutRecord,
    ) {
        rollout.append(&record).expect("append rollout");
        let indexed = crate::rollout::content_index_record(&record, rollout.record_offset(), 0);
        store
            .index_session_record(&meta.id, &meta.cwd, indexed.as_ref(), rollout.path())
            .expect("index appended record");
    }

    fn hit(store: &Store, text: &str) -> Vec<SessionSearchHit> {
        store
            .search_sessions(&SessionSearchQuery::new(text))
            .expect("search")
    }

    #[test]
    fn fts5_migration_and_literal_cjk_english_special_character_search() {
        let dir = temp_dir("literal");
        let mut store = Store::open(&dir).expect("open store");
        assert_eq!(
            store.schema_version().unwrap(),
            super::super::CURRENT_SCHEMA_VERSION
        );
        let compile_options = store
            .conn
            .prepare("PRAGMA compile_options")
            .expect("compile options")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query compile options")
            .collect::<Result<Vec<_>, _>>()
            .expect("read compile options");
        assert!(
            compile_options.iter().any(|option| option == "ENABLE_FTS5"),
            "bundled SQLite must enable FTS5: {compile_options:?}"
        );

        let first = meta("s1", "/work/a");
        let second = meta("s2", "/work/b");
        store.upsert_session(&first);
        store.upsert_session(&second);
        let sessions = dir.join("sessions");
        let mut first_rollout = Rollout::create(&sessions, &first).expect("create first");
        let mut second_rollout = Rollout::create(&sessions, &second).expect("create second");
        append_live(
            &mut store,
            &mut first_rollout,
            &first,
            user("中文搜索与 data_sync 标识符 SharedToken", 1_000),
        );
        append_live(
            &mut store,
            &mut first_rollout,
            &first,
            assistant("Kiwi-Fruit \"AND\" * 状态", 2_000),
        );
        append_live(
            &mut store,
            &mut second_rollout,
            &second,
            user("DataSync in another workspace SharedToken", 1_500),
        );

        for query in ["中", "中文", "data_sync", "AND", "*", "-", "\""] {
            let hits = hit(&store, query);
            assert_eq!(hits.len(), 1, "literal query {query:?}: {hits:?}");
            assert_eq!(hits[0].session_id, "s1");
        }
        assert_eq!(hit(&store, "SharedToken").len(), 2);
        assert_eq!(hit(&store, "DataSync")[0].session_id, "s2");
        let kiwi = hit(&store, "Kiwi");
        assert_eq!(kiwi[0].hit, "Kiwi");
        assert!(kiwi[0].after.starts_with("-Fruit"));
        assert_eq!(kiwi[0].kind, "assistant");

        let mut filtered = SessionSearchQuery::new("SharedToken");
        filtered.workspace = Some(PathBuf::from("/work/b"));
        assert_eq!(hit_with(&store, &filtered)[0].session_id, "s2");
        filtered.workspace = None;
        filtered.from_created_at_ms = Some(1_200);
        assert_eq!(hit_with(&store, &filtered)[0].session_id, "s2");
        filtered.from_created_at_ms = None;
        filtered.exclude_session_ids.push("s2".to_string());
        assert_eq!(hit_with(&store, &filtered)[0].session_id, "s1");
        filtered.exclude_session_ids.clear();
        filtered.limit = 1;
        assert_eq!(hit_with(&store, &filtered).len(), 1);
        filtered.offset = 1;
        assert_eq!(hit_with(&store, &filtered).len(), 1);
        filtered.offset = 2;
        assert!(hit_with(&store, &filtered).is_empty());

        store
            .request_session_deletion("s2")
            .expect("request deletion");
        store.delete_session_checked("s2").expect("delete session");
        let remaining = hit(&store, "SharedToken");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].session_id, "s1");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn hit_with(store: &Store, query: &SessionSearchQuery) -> Vec<SessionSearchHit> {
        store.search_sessions(query).expect("filtered search")
    }

    #[test]
    fn historical_backfill_is_incremental_idempotent_and_recovers_a_dropped_fts_table() {
        let dir = temp_dir("backfill");
        let sessions = dir.join("sessions");
        let old = meta("old", "/work/old");
        let mut rollout = Rollout::create(&sessions, &old).expect("create old rollout");
        rollout
            .append(&user("历史会话中的中文内容", 100))
            .expect("append old");
        rollout
            .append(&assistant("legacy_identifier remains searchable", 200))
            .expect("append old");
        rollout.sync().expect("sync old");

        let store = Store::open(&dir).expect("open store");
        store.upsert_session(&old);
        let store = Arc::new(Mutex::new(store));
        let first = backfill_session_search(&store, &sessions);
        assert_eq!(first.scanned, 1);
        assert_eq!(first.rebuilt, 1);
        assert_eq!(first.failed, 0);
        assert_eq!(
            store
                .lock()
                .unwrap()
                .search_session_content("中文")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(hit(&store.lock().unwrap(), "legacy_identifier").len(), 1);

        let second = backfill_session_search(&store, &sessions);
        assert_eq!(second.unchanged, 1);
        assert_eq!(second.rebuilt, 0);
        assert_eq!(hit(&store.lock().unwrap(), "legacy_identifier").len(), 1);

        rollout
            .append(&user("another historical message", 300))
            .expect("append later");
        rollout.sync().expect("sync later");
        let third = backfill_session_search(&store, &sessions);
        assert_eq!(third.rebuilt, 1);
        assert_eq!(hit(&store.lock().unwrap(), "another historical").len(), 1);
        let original_rollout = std::fs::read(sessions.join("old.jsonl")).expect("source bytes");
        drop(store);

        {
            let conn = Connection::open(dir.join("store.sqlite")).expect("raw connection");
            conn.execute("DROP TABLE session_search_fts", [])
                .expect("drop derived FTS table");
        }
        let store = Store::open(&dir).expect("self-heal dropped FTS table");
        assert_eq!(hit(&store, "legacy_identifier").len(), 1);
        assert_eq!(hit(&store, "another historical").len(), 1);
        assert_eq!(
            std::fs::read(sessions.join("old.jsonl")).expect("source after rebuild"),
            original_rollout,
            "rebuilding derived search state never rewrites the JSONL source"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recall_snapshots_exclude_withdrawn_turns_and_keep_compaction_summaries() {
        let dir = temp_dir("recall");
        let sessions = dir.join("sessions");
        let meta = meta("recall", "/work/recall");
        let mut rollout = Rollout::create(&sessions, &meta).expect("create rollout");
        for record in [
            user("keep this request", 100),
            assistant("keep this answer", 200),
            user("withdraw secret-request", 300),
            assistant("withdraw secret-answer", 400),
            RolloutRecord::Recall,
        ] {
            rollout.append(&record).expect("append");
        }
        rollout.sync().expect("sync");
        let snapshot = read_content_index_snapshot(rollout.path()).expect("search snapshot");
        assert_eq!(snapshot.records.len(), 2);
        assert!(
            snapshot
                .records
                .iter()
                .all(|record| !record.content.contains("withdraw"))
        );

        let mut store = Store::open(&dir).expect("open store");
        store.upsert_session(&meta);
        store
            .replace_session_search_snapshot(&snapshot, rollout.path())
            .expect("replace recalled index");
        assert!(hit(&store, "secret-request").is_empty());
        assert!(hit(&store, "secret-answer").is_empty());
        assert_eq!(store.search_session_content("keep this").unwrap().len(), 1);

        rollout
            .append(&RolloutRecord::Compact {
                note: "handoff".to_string(),
                omitted: 0,
                automatic: true,
                used_after: None,
                used_before: None,
                summary: Some("历史压缩摘要".to_string()),
                tail_len: None,
                preserved_user: None,
                trigger: None,
            })
            .expect("append compact");
        rollout.sync().expect("sync compact");
        let snapshot = read_content_index_snapshot(rollout.path()).expect("compacted snapshot");
        store
            .replace_session_search_snapshot(&snapshot, rollout.path())
            .expect("replace compacted index");
        assert_eq!(hit(&store, "压缩摘要").len(), 1);
        let mut since_recent = SessionSearchQuery::new("压缩摘要");
        since_recent.from_created_at_ms = Some(200);
        assert_eq!(
            hit_with(&store, &since_recent).len(),
            1,
            "compaction summary inherits recent message timestamp for time-filtered search"
        );
        assert!(hit(&store, "secret-answer").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fts_matches_identifiers_phrases_across_newlines_and_dedups_noisy_sessions() {
        assert!(fts_match_query("data_sync").is_some());
        assert!(fts_match_query("src/agent.rs").is_some());
        assert!(fts_match_query("fix bug").is_some());
        assert!(fts_match_query("中文搜索").is_some());
        assert!(fts_match_query("中文").is_none());
        assert!(fts_match_query("*").is_none());

        let dir = temp_dir("dedup");
        let sessions = dir.join("sessions");
        let mut store = Store::open(&dir).expect("open store");
        let noisy = meta("s-noisy", "/work/noisy");
        let other = meta("s-other", "/work/other");
        store.upsert_session(&noisy);
        store.upsert_session(&other);
        let mut noisy_rollout = Rollout::create(&sessions, &noisy).expect("create noisy");
        let mut other_rollout = Rollout::create(&sessions, &other).expect("create other");

        // Write >1,000 matching rows in one session to ensure streaming session
        // deduplication never crowds out other matching sessions.
        let mut noisy_records = Vec::new();
        for index in 0..1_050_u64 {
            noisy_records.push(ContentIndexRecord {
                offset: index + 1,
                kind: "user",
                content: format!("hello\nworld token_needle #{index}"),
                created_at_ms: 10_000 + index,
            });
        }
        noisy_rollout
            .append(&user("hello\nworld token_needle", 10_000))
            .expect("append noisy");
        noisy_rollout.sync().expect("sync noisy");
        store
            .replace_session_search_snapshot(
                &ContentIndexSnapshot {
                    session_id: noisy.id.clone(),
                    cwd: noisy.cwd.clone(),
                    records: noisy_records,
                },
                noisy_rollout.path(),
            )
            .expect("seed noisy snapshot");

        append_live(
            &mut store,
            &mut other_rollout,
            &other,
            user("other session with hello\nworld token_needle", 1_000),
        );

        let phrase_hits = store
            .search_session_content("hello world")
            .expect("cross-newline phrase search");
        let ids: std::collections::HashSet<_> = phrase_hits
            .iter()
            .map(|hit| hit.session_id.as_str())
            .collect();
        assert!(ids.contains("s-noisy"));
        assert!(ids.contains("s-other"));

        let needle_hits = store
            .search_session_content("token_needle")
            .expect("identifier search");
        assert_eq!(needle_hits.len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
