use pig_protocol::{BotCapabilities, BotDeletionTombstone, BotProfile, BotSyncPayload};
use rusqlite::{Connection, OptionalExtension as _, params};

use super::Store;

pub(super) fn create_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS bots (
            bot_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            avatar TEXT,
            description TEXT NOT NULL DEFAULT '',
            system_prompt TEXT NOT NULL DEFAULT '',
            provider_id TEXT,
            model_id TEXT,
            reasoning_level TEXT,
            capabilities_json TEXT NOT NULL,
            builtin INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS bot_deletions (
            bot_id TEXT PRIMARY KEY,
            deleted_at_ms INTEGER NOT NULL
        );
        "#,
    )
    .map_err(|error| format!("store.sqlite bots schema failed: {error}"))?;

    ensure_session_column(conn, "bot_id", "TEXT")?;
    ensure_session_column(conn, "global", "INTEGER NOT NULL DEFAULT 0")?;
    seed_builtin_bots(conn)?;
    Ok(())
}

fn ensure_session_column(conn: &Connection, column: &str, definition: &str) -> Result<(), String> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(sessions)")
        .map_err(|error| format!("store.sqlite pragma table_info(sessions) failed: {error}"))?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("store.sqlite query table_info(sessions) failed: {error}"))?
        .filter_map(Result::ok)
        .collect();
    if existing.iter().any(|name| name == column) {
        return Ok(());
    }
    conn.execute(
        &format!("ALTER TABLE sessions ADD COLUMN {column} {definition}"),
        [],
    )
    .map_err(|error| format!("store.sqlite add sessions.{column} failed: {error}"))?;
    Ok(())
}

pub(super) fn seed_builtin_bots(conn: &Connection) -> Result<(), String> {
    for bot in crate::bots::builtin_bots() {
        let capabilities_json =
            serde_json::to_string(&bot.capabilities).unwrap_or_else(|_| "{}".to_string());
        conn.execute(
            r#"
            INSERT OR IGNORE INTO bots (
                bot_id, name, avatar, description, system_prompt,
                provider_id, model_id, reasoning_level,
                capabilities_json, builtin, created_at_ms, updated_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            "#,
            params![
                bot.bot_id,
                bot.name,
                bot.avatar,
                bot.description,
                bot.system_prompt,
                bot.provider_id,
                bot.model_id,
                bot.reasoning_level,
                capabilities_json,
                if bot.builtin { 1_i64 } else { 0_i64 },
                bot.created_at_ms as i64,
                bot.updated_at_ms as i64,
            ],
        )
        .map_err(|error| format!("store.sqlite seed builtin bot {} failed: {error}", bot.bot_id))?;
    }
    Ok(())
}

fn bot_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BotProfile> {
    let caps_raw: String = row.get(8)?;
    let capabilities: BotCapabilities = serde_json::from_str(&caps_raw).unwrap_or_default();
    Ok(BotProfile {
        bot_id: row.get(0)?,
        name: row.get(1)?,
        avatar: row.get(2)?,
        description: row.get(3)?,
        system_prompt: row.get(4)?,
        provider_id: row.get(5)?,
        model_id: row.get(6)?,
        reasoning_level: row.get(7)?,
        capabilities,
        builtin: row.get::<_, i64>(9)? != 0,
        created_at_ms: row.get::<_, i64>(10)?.max(0) as u64,
        updated_at_ms: row.get::<_, i64>(11)?.max(0) as u64,
    })
}

const BOT_COLUMNS: &str = "bot_id, name, avatar, description, system_prompt, provider_id, model_id, reasoning_level, capabilities_json, builtin, created_at_ms, updated_at_ms";

impl Store {
    pub fn list_bots(&self) -> Vec<BotProfile> {
        let Ok(mut stmt) = self.conn.prepare(&format!(
            "SELECT {BOT_COLUMNS} FROM bots ORDER BY builtin DESC, updated_at_ms DESC, name ASC"
        )) else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], bot_from_row) else {
            return Vec::new();
        };
        rows.filter_map(Result::ok).collect()
    }

    pub fn get_bot(&self, bot_id: &str) -> Option<BotProfile> {
        self.conn
            .query_row(
                &format!("SELECT {BOT_COLUMNS} FROM bots WHERE bot_id = ?1"),
                params![bot_id],
                bot_from_row,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn upsert_bot(&self, bot: &BotProfile) -> Result<BotProfile, String> {
        let mut normalized = crate::bots::normalize_bot_profile(bot.clone())?;
        if let Some(existing) = self.get_bot(&normalized.bot_id) {
            normalized.builtin = existing.builtin;
            if existing.created_at_ms > 0 {
                normalized.created_at_ms = existing.created_at_ms;
            }
        } else {
            normalized.builtin = false;
        }
        normalized.updated_at_ms = crate::bots::now_ms().max(normalized.updated_at_ms);
        let capabilities_json =
            serde_json::to_string(&normalized.capabilities).map_err(|error| error.to_string())?;
        self.conn
            .execute(
                r#"
                INSERT INTO bots (
                    bot_id, name, avatar, description, system_prompt,
                    provider_id, model_id, reasoning_level,
                    capabilities_json, builtin, created_at_ms, updated_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                ON CONFLICT(bot_id) DO UPDATE SET
                    name = excluded.name,
                    avatar = excluded.avatar,
                    description = excluded.description,
                    system_prompt = excluded.system_prompt,
                    provider_id = excluded.provider_id,
                    model_id = excluded.model_id,
                    reasoning_level = excluded.reasoning_level,
                    capabilities_json = excluded.capabilities_json,
                    builtin = excluded.builtin,
                    created_at_ms = excluded.created_at_ms,
                    updated_at_ms = excluded.updated_at_ms
                "#,
                params![
                    normalized.bot_id,
                    normalized.name,
                    normalized.avatar,
                    normalized.description,
                    normalized.system_prompt,
                    normalized.provider_id,
                    normalized.model_id,
                    normalized.reasoning_level,
                    capabilities_json,
                    if normalized.builtin { 1_i64 } else { 0_i64 },
                    normalized.created_at_ms as i64,
                    normalized.updated_at_ms as i64,
                ],
            )
            .map_err(|error| error.to_string())?;
        let _ = self.conn.execute(
            "DELETE FROM bot_deletions WHERE bot_id = ?1",
            params![normalized.bot_id],
        );
        Ok(normalized)
    }

    pub fn delete_bot(&self, bot_id: &str) -> Result<bool, String> {
        let Some(existing) = self.get_bot(bot_id) else {
            return Ok(false);
        };
        if existing.builtin {
            return Err(format!("built-in bot `{bot_id}` cannot be deleted"));
        }
        let changed = self
            .conn
            .execute("DELETE FROM bots WHERE bot_id = ?1", params![bot_id])
            .map_err(|error| error.to_string())?;
        if changed > 0 {
            let now = crate::bots::now_ms();
            let _ = self.conn.execute(
                "INSERT INTO bot_deletions (bot_id, deleted_at_ms) VALUES (?1, ?2)
                 ON CONFLICT(bot_id) DO UPDATE SET deleted_at_ms = MAX(deleted_at_ms, excluded.deleted_at_ms)",
                params![bot_id, now as i64],
            );
        }
        Ok(changed > 0)
    }

    pub fn bot_deletions(&self) -> Vec<BotDeletionTombstone> {
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT bot_id, deleted_at_ms FROM bot_deletions")
        else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |row| {
            Ok(BotDeletionTombstone {
                bot_id: row.get(0)?,
                deleted_at_ms: row.get::<_, i64>(1)?.max(0) as u64,
            })
        }) else {
            return Vec::new();
        };
        rows.filter_map(Result::ok).collect()
    }

    /// Merge synced bots & deletion tombstones from another device (last-write-wins).
    pub fn apply_bot_sync(&self, payload: &BotSyncPayload) -> Result<usize, String> {
        let mut applied = 0;
        let deletions = self.bot_deletions();
        for remote in &payload.bots {
            let Ok(normalized) = crate::bots::normalize_bot_profile(remote.clone()) else {
                continue;
            };
            if deletions
                .iter()
                .any(|d| d.bot_id == normalized.bot_id && d.deleted_at_ms >= normalized.updated_at_ms)
            {
                continue;
            }
            if let Some(local) = self.get_bot(&normalized.bot_id)
                && local.updated_at_ms >= normalized.updated_at_ms
            {
                continue;
            }
            let capabilities_json = serde_json::to_string(&normalized.capabilities)
                .map_err(|error| error.to_string())?;
            self.conn
                .execute(
                    r#"
                    INSERT INTO bots (
                        bot_id, name, avatar, description, system_prompt,
                        provider_id, model_id, reasoning_level,
                        capabilities_json, builtin, created_at_ms, updated_at_ms
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                    ON CONFLICT(bot_id) DO UPDATE SET
                        name = excluded.name,
                        avatar = excluded.avatar,
                        description = excluded.description,
                        system_prompt = excluded.system_prompt,
                        provider_id = excluded.provider_id,
                        model_id = excluded.model_id,
                        reasoning_level = excluded.reasoning_level,
                        capabilities_json = excluded.capabilities_json,
                        builtin = excluded.builtin,
                        created_at_ms = excluded.created_at_ms,
                        updated_at_ms = excluded.updated_at_ms
                    "#,
                    params![
                        normalized.bot_id,
                        normalized.name,
                        normalized.avatar,
                        normalized.description,
                        normalized.system_prompt,
                        normalized.provider_id,
                        normalized.model_id,
                        normalized.reasoning_level,
                        capabilities_json,
                        if normalized.builtin { 1_i64 } else { 0_i64 },
                        normalized.created_at_ms as i64,
                        normalized.updated_at_ms as i64,
                    ],
                )
                .map_err(|error| error.to_string())?;
            applied += 1;
        }
        for tombstone in &payload.deletions {
            if let Some(local) = self.get_bot(&tombstone.bot_id) {
                if local.builtin || local.updated_at_ms > tombstone.deleted_at_ms {
                    continue;
                }
                let _ = self
                    .conn
                    .execute("DELETE FROM bots WHERE bot_id = ?1", params![tombstone.bot_id]);
                applied += 1;
            }
            let _ = self.conn.execute(
                "INSERT INTO bot_deletions (bot_id, deleted_at_ms) VALUES (?1, ?2)
                 ON CONFLICT(bot_id) DO UPDATE SET deleted_at_ms = MAX(deleted_at_ms, excluded.deleted_at_ms)",
                params![tombstone.bot_id, tombstone.deleted_at_ms as i64],
            );
        }
        Ok(applied)
    }
}
