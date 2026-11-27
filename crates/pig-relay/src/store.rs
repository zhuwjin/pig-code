use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use pig_protocol::{
    BotDeletionTombstone, BotMemoryFile, BotProfile, BotSyncPayload, RelayDeviceInfo, RelayUser,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use uuid::Uuid;

use crate::auth::{generate_secret_token, generate_user_code, hash_password, hash_token, verify_password};

const DEVICE_CODE_TTL_MS: u64 = 10 * 60 * 1000;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct DeviceRecord {
    pub device_id: String,
    pub user_id: String,
    pub name: String,
    pub os: String,
    pub arch: String,
    pub workspaces: Vec<String>,
    pub daemon_protocol: u32,
    pub last_seen_ms: u64,
}

#[derive(Clone, Debug)]
pub struct DeviceCodeChallenge {
    pub device_code: String,
    pub user_code: String,
    pub device_name: String,
    pub os: String,
    pub arch: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug)]
pub enum DeviceCodeStatus {
    Pending,
    Expired,
    Approved {
        user: RelayUser,
        session_token: String,
        device_id: String,
        device_token: String,
    },
}

/// SQLite-backed store for `pig-relay`.
///
/// Privacy boundary: this store persists ONLY account credentials (salted
/// hashes), hashed tokens, device registry metadata, ephemeral device-code
/// login challenges, and account-scoped Bot profiles + Bot personal long-term
/// memory. It NEVER stores workspace source code, workspace memory, tool
/// outputs, conversation content, or model API keys.
pub struct RelayStore {
    conn: Connection,
}

impl RelayStore {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir).map_err(|error| error.to_string())?;
        let db_path = data_dir.join("relay.db");
        let conn = Connection::open(&db_path).map_err(|error| error.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| error.to_string())?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| error.to_string())?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                user_id TEXT PRIMARY KEY,
                username TEXT NOT NULL UNIQUE COLLATE NOCASE,
                password_hash TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS session_tokens (
                token_hash TEXT PRIMARY KEY,
                user_id TEXT NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
                label TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                revoked INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS devices (
                device_id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                os TEXT NOT NULL,
                arch TEXT NOT NULL,
                workspaces_json TEXT NOT NULL DEFAULT '[]',
                daemon_protocol INTEGER NOT NULL DEFAULT 1,
                token_hash TEXT NOT NULL UNIQUE,
                created_at_ms INTEGER NOT NULL,
                last_seen_ms INTEGER NOT NULL,
                revoked INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS device_codes (
                device_code_hash TEXT PRIMARY KEY,
                user_code TEXT NOT NULL UNIQUE,
                device_name TEXT NOT NULL,
                os TEXT NOT NULL,
                arch TEXT NOT NULL,
                expires_at_ms INTEGER NOT NULL,
                approved_user_id TEXT REFERENCES users(user_id) ON DELETE CASCADE,
                consumed INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS account_bots (
                user_id TEXT NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
                bot_id TEXT NOT NULL,
                profile_json TEXT NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY (user_id, bot_id)
            );

            CREATE TABLE IF NOT EXISTS account_bot_deletions (
                user_id TEXT NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
                bot_id TEXT NOT NULL,
                deleted_at_ms INTEGER NOT NULL,
                PRIMARY KEY (user_id, bot_id)
            );

            CREATE TABLE IF NOT EXISTS account_bot_memories (
                user_id TEXT NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
                bot_id TEXT NOT NULL,
                path TEXT NOT NULL,
                content TEXT NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (user_id, bot_id, path)
            );
            "#,
        )
        .map_err(|error| error.to_string())?;
        Ok(Self { conn })
    }

    pub fn register_user(&mut self, username: &str, password: &str) -> Result<RelayUser, String> {
        let username = normalize_username(username)?;
        validate_password(password)?;
        let user_id = Uuid::new_v4().to_string();
        let created_at_ms = now_ms();
        let password_hash = hash_password(password);
        self.conn
            .execute(
                "INSERT INTO users (user_id, username, password_hash, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
                params![user_id, username, password_hash, created_at_ms as i64],
            )
            .map_err(|error| {
                if error.to_string().contains("UNIQUE") {
                    "username already exists".to_string()
                } else {
                    error.to_string()
                }
            })?;
        Ok(RelayUser {
            user_id,
            username,
            created_at_ms,
        })
    }

    pub fn authenticate_user(&self, username: &str, password: &str) -> Result<RelayUser, String> {
        let username = username.trim();
        let row: Option<(String, String, String, i64)> = self
            .conn
            .query_row(
                "SELECT user_id, username, password_hash, created_at_ms FROM users WHERE username = ?1",
                params![username],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some((user_id, canonical_username, password_hash, created_at_ms)) = row else {
            return Err("invalid username or password".to_string());
        };
        if !verify_password(password, &password_hash) {
            return Err("invalid username or password".to_string());
        }
        Ok(RelayUser {
            user_id,
            username: canonical_username,
            created_at_ms: created_at_ms.max(0) as u64,
        })
    }

    pub fn issue_session_token(&mut self, user_id: &str, label: &str) -> Result<String, String> {
        let token = generate_secret_token("pig_sess");
        let token_hash = hash_token(&token);
        let created_at_ms = now_ms();
        self.conn
            .execute(
                "INSERT INTO session_tokens (token_hash, user_id, label, created_at_ms, revoked) VALUES (?1, ?2, ?3, ?4, 0)",
                params![token_hash, user_id, label, created_at_ms as i64],
            )
            .map_err(|error| error.to_string())?;
        Ok(token)
    }

    pub fn verify_session_token(&self, token: &str) -> Result<Option<RelayUser>, String> {
        let token_hash = hash_token(token);
        self.conn
            .query_row(
                r#"
                SELECT u.user_id, u.username, u.created_at_ms
                FROM session_tokens s
                JOIN users u ON u.user_id = s.user_id
                WHERE s.token_hash = ?1 AND s.revoked = 0
                "#,
                params![token_hash],
                |row| {
                    Ok(RelayUser {
                        user_id: row.get(0)?,
                        username: row.get(1)?,
                        created_at_ms: row.get::<_, i64>(2)?.max(0) as u64,
                    })
                },
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn revoke_session_token(&mut self, token: &str) -> Result<bool, String> {
        let token_hash = hash_token(token);
        let changed = self
            .conn
            .execute(
                "UPDATE session_tokens SET revoked = 1 WHERE token_hash = ?1 AND revoked = 0",
                params![token_hash],
            )
            .map_err(|error| error.to_string())?;
        Ok(changed > 0)
    }

    pub fn register_device(
        &mut self,
        user_id: &str,
        device_id: Option<&str>,
        name: &str,
        os: &str,
        arch: &str,
    ) -> Result<(String, String), String> {
        let now = now_ms();
        let token = generate_secret_token("pig_dev");
        let token_hash = hash_token(&token);
        let name = if name.trim().is_empty() {
            "Unnamed Device"
        } else {
            name.trim()
        };
        if let Some(existing_id) = device_id.filter(|id| !id.trim().is_empty()) {
            let owned: Option<String> = self
                .conn
                .query_row(
                    "SELECT user_id FROM devices WHERE device_id = ?1",
                    params![existing_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if let Some(owner) = owned {
                if owner != user_id {
                    return Err("device_id belongs to another account".to_string());
                }
                self.conn
                    .execute(
                        r#"
                        UPDATE devices
                        SET name = ?2, os = ?3, arch = ?4, token_hash = ?5, last_seen_ms = ?6, revoked = 0
                        WHERE device_id = ?1
                        "#,
                        params![existing_id, name, os, arch, token_hash, now as i64],
                    )
                    .map_err(|error| error.to_string())?;
                return Ok((existing_id.to_string(), token));
            }
            self.conn
                .execute(
                    r#"
                    INSERT INTO devices (device_id, user_id, name, os, arch, workspaces_json, daemon_protocol, token_hash, created_at_ms, last_seen_ms, revoked)
                    VALUES (?1, ?2, ?3, ?4, ?5, '[]', 1, ?6, ?7, ?7, 0)
                    "#,
                    params![existing_id, user_id, name, os, arch, token_hash, now as i64],
                )
                .map_err(|error| error.to_string())?;
            return Ok((existing_id.to_string(), token));
        }
        let device_id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                r#"
                INSERT INTO devices (device_id, user_id, name, os, arch, workspaces_json, daemon_protocol, token_hash, created_at_ms, last_seen_ms, revoked)
                VALUES (?1, ?2, ?3, ?4, ?5, '[]', 1, ?6, ?7, ?7, 0)
                "#,
                params![device_id, user_id, name, os, arch, token_hash, now as i64],
            )
            .map_err(|error| error.to_string())?;
        Ok((device_id, token))
    }

    pub fn verify_device_token(&self, token: &str) -> Result<Option<DeviceRecord>, String> {
        let token_hash = hash_token(token);
        let row: Option<(String, String, String, String, String, String, u32, i64)> = self
            .conn
            .query_row(
                r#"
                SELECT device_id, user_id, name, os, arch, workspaces_json, daemon_protocol, last_seen_ms
                FROM devices
                WHERE token_hash = ?1 AND revoked = 0
                "#,
                params![token_hash],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?;
        Ok(row.map(
            |(device_id, user_id, name, os, arch, workspaces_json, daemon_protocol, last_seen_ms)| {
                DeviceRecord {
                    device_id,
                    user_id,
                    name,
                    os,
                    arch,
                    workspaces: serde_json::from_str(&workspaces_json).unwrap_or_default(),
                    daemon_protocol,
                    last_seen_ms: last_seen_ms.max(0) as u64,
                }
            },
        ))
    }

    pub fn update_device_presence(
        &mut self,
        device_id: &str,
        name: Option<&str>,
        os: Option<&str>,
        arch: Option<&str>,
        workspaces: &[String],
        daemon_protocol: Option<u32>,
    ) -> Result<(), String> {
        let now = now_ms();
        let workspaces_json = serde_json::to_string(workspaces).unwrap_or_else(|_| "[]".into());
        self.conn
            .execute(
                r#"
                UPDATE devices
                SET name = COALESCE(?2, name),
                    os = COALESCE(?3, os),
                    arch = COALESCE(?4, arch),
                    workspaces_json = ?5,
                    daemon_protocol = COALESCE(?6, daemon_protocol),
                    last_seen_ms = ?7
                WHERE device_id = ?1
                "#,
                params![
                    device_id,
                    name,
                    os,
                    arch,
                    workspaces_json,
                    daemon_protocol,
                    now as i64
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn list_user_devices(
        &self,
        user_id: &str,
        online_ids: &std::collections::HashSet<String>,
    ) -> Result<Vec<RelayDeviceInfo>, String> {
        let mut stmt = self
            .conn
            .prepare(
                r#"
                SELECT device_id, name, os, arch, workspaces_json, daemon_protocol, last_seen_ms
                FROM devices
                WHERE user_id = ?1 AND revoked = 0
                ORDER BY last_seen_ms DESC
                "#,
            )
            .map_err(|error| error.to_string())?;
        let rows = stmt
            .query_map(params![user_id], |row| {
                let device_id: String = row.get(0)?;
                let workspaces_json: String = row.get(4)?;
                Ok(RelayDeviceInfo {
                    online: online_ids.contains(&device_id),
                    device_id,
                    name: row.get(1)?,
                    os: row.get(2)?,
                    arch: row.get(3)?,
                    workspaces: serde_json::from_str(&workspaces_json).unwrap_or_default(),
                    daemon_protocol: row.get(5)?,
                    last_seen_ms: row.get::<_, i64>(6)?.max(0) as u64,
                })
            })
            .map_err(|error| error.to_string())?;
        let mut devices = Vec::new();
        for row in rows {
            devices.push(row.map_err(|error| error.to_string())?);
        }
        devices.sort_by(|left, right| {
            right
                .online
                .cmp(&left.online)
                .then_with(|| right.last_seen_ms.cmp(&left.last_seen_ms))
        });
        Ok(devices)
    }

    pub fn device_belongs_to_user(&self, device_id: &str, user_id: &str) -> Result<bool, String> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM devices WHERE device_id = ?1 AND user_id = ?2 AND revoked = 0",
                params![device_id, user_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        Ok(found.is_some())
    }

    pub fn revoke_device(&mut self, user_id: &str, device_id: &str) -> Result<bool, String> {
        let changed = self
            .conn
            .execute(
                "UPDATE devices SET revoked = 1 WHERE device_id = ?1 AND user_id = ?2 AND revoked = 0",
                params![device_id, user_id],
            )
            .map_err(|error| error.to_string())?;
        Ok(changed > 0)
    }

    pub fn start_device_code(
        &mut self,
        device_name: &str,
        os: &str,
        arch: &str,
    ) -> Result<DeviceCodeChallenge, String> {
        let now = now_ms();
        let _ = self.conn.execute(
            "DELETE FROM device_codes WHERE expires_at_ms < ?1 OR consumed = 1",
            params![now as i64],
        );
        let device_code = generate_secret_token("pig_dc");
        let device_code_hash = hash_token(&device_code);
        let user_code = generate_user_code();
        let expires_at_ms = now + DEVICE_CODE_TTL_MS;
        let device_name = if device_name.trim().is_empty() {
            "Pig Device"
        } else {
            device_name.trim()
        };
        self.conn
            .execute(
                r#"
                INSERT INTO device_codes (device_code_hash, user_code, device_name, os, arch, expires_at_ms, approved_user_id, consumed)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0)
                "#,
                params![
                    device_code_hash,
                    user_code,
                    device_name,
                    os,
                    arch,
                    expires_at_ms as i64
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(DeviceCodeChallenge {
            device_code,
            user_code,
            device_name: device_name.to_string(),
            os: os.to_string(),
            arch: arch.to_string(),
            expires_at_ms,
        })
    }

    pub fn approve_device_code(&mut self, user_id: &str, user_code: &str) -> Result<(), String> {
        let normalized = user_code.trim().to_ascii_uppercase();
        let now = now_ms();
        let changed = self
            .conn
            .execute(
                r#"
                UPDATE device_codes
                SET approved_user_id = ?1
                WHERE user_code = ?2 AND consumed = 0 AND expires_at_ms >= ?3 AND approved_user_id IS NULL
                "#,
                params![user_id, normalized, now as i64],
            )
            .map_err(|error| error.to_string())?;
        if changed == 0 {
            return Err("invalid or expired device code".to_string());
        }
        Ok(())
    }

    pub fn poll_device_code(&mut self, device_code: &str) -> Result<DeviceCodeStatus, String> {
        let device_code_hash = hash_token(device_code);
        let now = now_ms();
        let row: Option<(String, String, String, i64, Option<String>, i64)> = self
            .conn
            .query_row(
                r#"
                SELECT device_name, os, arch, expires_at_ms, approved_user_id, consumed
                FROM device_codes
                WHERE device_code_hash = ?1
                "#,
                params![device_code_hash],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some((device_name, os, arch, expires_at_ms, approved_user_id, consumed)) = row else {
            return Ok(DeviceCodeStatus::Expired);
        };
        if consumed != 0 || (expires_at_ms.max(0) as u64) < now {
            return Ok(DeviceCodeStatus::Expired);
        }
        let Some(user_id) = approved_user_id else {
            return Ok(DeviceCodeStatus::Pending);
        };
        let user: RelayUser = self
            .conn
            .query_row(
                "SELECT user_id, username, created_at_ms FROM users WHERE user_id = ?1",
                params![user_id],
                |row| {
                    Ok(RelayUser {
                        user_id: row.get(0)?,
                        username: row.get(1)?,
                        created_at_ms: row.get::<_, i64>(2)?.max(0) as u64,
                    })
                },
            )
            .map_err(|error| error.to_string())?;
        let marked = self
            .conn
            .execute(
                "UPDATE device_codes SET consumed = 1 WHERE device_code_hash = ?1 AND consumed = 0",
                params![device_code_hash],
            )
            .map_err(|error| error.to_string())?;
        if marked == 0 {
            return Ok(DeviceCodeStatus::Expired);
        }
        let session_token = self.issue_session_token(&user.user_id, &device_name)?;
        let (device_id, device_token) =
            self.register_device(&user.user_id, None, &device_name, &os, &arch)?;
        Ok(DeviceCodeStatus::Approved {
            user,
            session_token,
            device_id,
            device_token,
        })
    }

    /// Resolve a `user_id` from either an active session token or an active device token.
    pub fn resolve_user_id_from_token(&self, token: &str) -> Result<Option<String>, String> {
        if let Some(user) = self.verify_session_token(token)? {
            return Ok(Some(user.user_id));
        }
        if let Some(device) = self.verify_device_token(token)? {
            return Ok(Some(device.user_id));
        }
        Ok(None)
    }

    pub fn get_bot_sync(&self, user_id: &str) -> Result<BotSyncPayload, String> {
        let mut bots = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT profile_json FROM account_bots WHERE user_id = ?1 ORDER BY bot_id ASC",
                )
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map(params![user_id], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?;
            for row in rows {
                let raw = row.map_err(|error| error.to_string())?;
                if let Ok(profile) = serde_json::from_str::<BotProfile>(&raw) {
                    bots.push(profile);
                }
            }
        }

        let mut deletions = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT bot_id, deleted_at_ms FROM account_bot_deletions WHERE user_id = ?1 ORDER BY bot_id ASC",
                )
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map(params![user_id], |row| {
                    Ok(BotDeletionTombstone {
                        bot_id: row.get(0)?,
                        deleted_at_ms: row.get::<_, i64>(1)?.max(0) as u64,
                    })
                })
                .map_err(|error| error.to_string())?;
            for row in rows {
                deletions.push(row.map_err(|error| error.to_string())?);
            }
        }

        let mut memories: BTreeMap<String, Vec<BotMemoryFile>> = BTreeMap::new();
        {
            let mut stmt = self
                .conn
                .prepare(
                    r#"
                    SELECT bot_id, path, content, updated_at_ms, deleted
                    FROM account_bot_memories
                    WHERE user_id = ?1
                    ORDER BY bot_id ASC, path ASC
                    "#,
                )
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map(params![user_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        BotMemoryFile {
                            path: row.get(1)?,
                            content: row.get(2)?,
                            updated_at_ms: row.get::<_, i64>(3)?.max(0) as u64,
                            deleted: row.get::<_, i64>(4)? != 0,
                        },
                    ))
                })
                .map_err(|error| error.to_string())?;
            for row in rows {
                let (bot_id, file) = row.map_err(|error| error.to_string())?;
                memories.entry(bot_id).or_default().push(file);
            }
        }

        Ok(BotSyncPayload {
            bots,
            deletions,
            memories,
        })
    }

    pub fn merge_bot_sync(
        &mut self,
        user_id: &str,
        incoming: &BotSyncPayload,
    ) -> Result<BotSyncPayload, String> {
        let tx = self
            .conn
            .transaction()
            .map_err(|error| error.to_string())?;

        for bot in &incoming.bots {
            if !is_safe_bot_id(&bot.bot_id) {
                continue;
            }
            let existing_deleted: Option<i64> = tx
                .query_row(
                    "SELECT deleted_at_ms FROM account_bot_deletions WHERE user_id = ?1 AND bot_id = ?2",
                    params![user_id, &bot.bot_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if existing_deleted.is_some_and(|ts| (ts.max(0) as u64) >= bot.updated_at_ms) {
                continue;
            }
            let existing_updated: Option<i64> = tx
                .query_row(
                    "SELECT updated_at_ms FROM account_bots WHERE user_id = ?1 AND bot_id = ?2",
                    params![user_id, &bot.bot_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if existing_updated.is_some_and(|ts| (ts.max(0) as u64) >= bot.updated_at_ms) {
                continue;
            }
            let profile_json = serde_json::to_string(bot).map_err(|error| error.to_string())?;
            tx.execute(
                r#"
                INSERT INTO account_bots (user_id, bot_id, profile_json, updated_at_ms)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(user_id, bot_id) DO UPDATE SET
                    profile_json = excluded.profile_json,
                    updated_at_ms = excluded.updated_at_ms
                "#,
                params![user_id, &bot.bot_id, profile_json, bot.updated_at_ms as i64],
            )
            .map_err(|error| error.to_string())?;
            let _ = tx.execute(
                "DELETE FROM account_bot_deletions WHERE user_id = ?1 AND bot_id = ?2",
                params![user_id, &bot.bot_id],
            );
        }

        for tombstone in &incoming.deletions {
            if !is_safe_bot_id(&tombstone.bot_id) {
                continue;
            }
            let existing_updated: Option<i64> = tx
                .query_row(
                    "SELECT updated_at_ms FROM account_bots WHERE user_id = ?1 AND bot_id = ?2",
                    params![user_id, &tombstone.bot_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if existing_updated.is_some_and(|ts| (ts.max(0) as u64) > tombstone.deleted_at_ms) {
                continue;
            }
            tx.execute(
                r#"
                INSERT INTO account_bot_deletions (user_id, bot_id, deleted_at_ms)
                VALUES (?1, ?2, ?3)
                ON CONFLICT(user_id, bot_id) DO UPDATE SET
                    deleted_at_ms = MAX(account_bot_deletions.deleted_at_ms, excluded.deleted_at_ms)
                "#,
                params![user_id, &tombstone.bot_id, tombstone.deleted_at_ms as i64],
            )
            .map_err(|error| error.to_string())?;
            let _ = tx.execute(
                "DELETE FROM account_bots WHERE user_id = ?1 AND bot_id = ?2",
                params![user_id, &tombstone.bot_id],
            );
            let _ = tx.execute(
                "DELETE FROM account_bot_memories WHERE user_id = ?1 AND bot_id = ?2",
                params![user_id, &tombstone.bot_id],
            );
        }

        for (bot_id, files) in &incoming.memories {
            if !is_safe_bot_id(bot_id) {
                continue;
            }
            let deleted_at: Option<i64> = tx
                .query_row(
                    "SELECT deleted_at_ms FROM account_bot_deletions WHERE user_id = ?1 AND bot_id = ?2",
                    params![user_id, bot_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            for file in files {
                if !is_safe_memory_filename(&file.path) {
                    continue;
                }
                if deleted_at.is_some_and(|ts| (ts.max(0) as u64) >= file.updated_at_ms) {
                    continue;
                }
                let existing_updated: Option<i64> = tx
                    .query_row(
                        r#"
                        SELECT updated_at_ms FROM account_bot_memories
                        WHERE user_id = ?1 AND bot_id = ?2 AND path = ?3
                        "#,
                        params![user_id, bot_id, &file.path],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| error.to_string())?;
                if existing_updated.is_some_and(|ts| (ts.max(0) as u64) >= file.updated_at_ms) {
                    continue;
                }
                tx.execute(
                    r#"
                    INSERT INTO account_bot_memories (user_id, bot_id, path, content, updated_at_ms, deleted)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    ON CONFLICT(user_id, bot_id, path) DO UPDATE SET
                        content = excluded.content,
                        updated_at_ms = excluded.updated_at_ms,
                        deleted = excluded.deleted
                    "#,
                    params![
                        user_id,
                        bot_id,
                        &file.path,
                        &file.content,
                        file.updated_at_ms as i64,
                        i64::from(file.deleted)
                    ],
                )
                .map_err(|error| error.to_string())?;
            }
        }

        tx.commit().map_err(|error| error.to_string())?;
        self.get_bot_sync(user_id)
    }
}

fn is_safe_bot_id(bot_id: &str) -> bool {
    let trimmed = bot_id.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn is_safe_memory_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.ends_with(".md")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
}

fn normalize_username(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.len() < 2 || trimmed.len() > 64 {
        return Err("username must be between 2 and 64 characters".to_string());
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@'))
    {
        return Err("username may only contain ASCII letters, digits, _, -, ., and @".to_string());
    }
    Ok(trimmed.to_string())
}

fn validate_password(password: &str) -> Result<(), String> {
    if password.len() < 8 {
        return Err("password must be at least 8 characters".to_string());
    }
    if password.len() > 256 {
        return Err("password is too long".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (std::path::PathBuf, RelayStore) {
        let dir = std::env::temp_dir().join(format!(
            "pig-relay-store-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        let store = RelayStore::open(&dir).expect("open relay store");
        (dir, store)
    }

    #[test]
    fn account_device_and_device_code_lifecycle() {
        let (dir, mut store) = temp_store();
        let user = store
            .register_user("alice", "super-secret-pass")
            .expect("register user");
        let authed = store
            .authenticate_user("ALICE", "super-secret-pass")
            .expect("authenticate case-insensitive username");
        assert_eq!(authed.user_id, user.user_id);

        let session = store
            .issue_session_token(&user.user_id, "laptop")
            .expect("issue session token");
        assert_eq!(
            store
                .verify_session_token(&session)
                .unwrap()
                .map(|u| u.user_id),
            Some(user.user_id.clone())
        );

        let challenge = store
            .start_device_code("Workstation", "linux", "x86_64")
            .expect("start device code");
        assert!(matches!(
            store.poll_device_code(&challenge.device_code).unwrap(),
            DeviceCodeStatus::Pending
        ));
        store
            .approve_device_code(&user.user_id, &challenge.user_code)
            .expect("approve device code");
        let approved = store
            .poll_device_code(&challenge.device_code)
            .expect("poll approved");
        let (device_id, device_token) = match approved {
            DeviceCodeStatus::Approved {
                device_id,
                device_token,
                ..
            } => (device_id, device_token),
            other => panic!("expected approved, got {other:?}"),
        };
        assert!(
            store
                .verify_device_token(&device_token)
                .unwrap()
                .is_some_and(|record| record.device_id == device_id)
        );
        // One-time consumption: second poll returns Expired.
        assert!(matches!(
            store.poll_device_code(&challenge.device_code).unwrap(),
            DeviceCodeStatus::Expired
        ));

        assert!(store.revoke_device(&user.user_id, &device_id).unwrap());
        assert!(store.verify_device_token(&device_token).unwrap().is_none());

        assert!(store.revoke_session_token(&session).unwrap());
        assert!(store.verify_session_token(&session).unwrap().is_none());

        let _ = std::fs::remove_dir_all(dir);
    }
}
