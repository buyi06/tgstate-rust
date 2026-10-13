//! 账户与凭据存储（login_credentials 表）。
//!
//! 单管理员模型。存储四类凭据：
//! - USERNAME / PASSWORD：登录用（密码为 argon2 哈希，兼容历史明文过渡）
//! - TOTP_SECRET / TOTP_RECOVERY_HASHES：可选 2FA
//! - API_KEY_HASH：CLI 调用接口用的 API 密钥（只存 sha256，明文仅生成时展示一次）
//!
//! 全部走 `login_credentials` 单行表（id=1），沿用 app_settings 的“行存在与否”
//! 语义：None = 未配置，保持各调用点的分支简单。

use rusqlite::params;
use sha2::{Digest, Sha256};

use crate::auth;
use crate::error::AppErrorKind;

pub type DbPool = crate::database::DbPool;

/// A `login_credentials` row snapshot. Every field is optional; a row is
/// considered configured when `username` and `password_hash` are both present.
#[derive(Debug, Default, Clone)]
pub struct Credentials {
    pub username: Option<String>,
    pub password_hash: Option<String>,
    pub totp_secret: Option<String>,
    pub totp_recovery_hashes: Option<String>,
    pub api_key_hash: Option<String>,
}

impl Credentials {
    pub fn has_login(&self) -> bool {
        self.username.as_deref().map_or(false, |u| !u.is_empty())
            && self.password_hash.as_deref().map_or(false, |p| !p.is_empty())
    }
    pub fn has_totp(&self) -> bool {
        self.totp_secret.as_deref().map_or(false, |s| !s.is_empty())
    }
    pub fn has_api_key(&self) -> bool {
        self.api_key_hash.as_deref().map_or(false, |h| !h.is_empty())
    }
}

/// Ensure the table exists and has all columns（幂等，可在 init_db 后调用，
/// 也可独立调用——部署顺序不同也能正确建表）。
pub fn ensure_table(pool: &DbPool) {
    let conn = pool.get().expect("get conn for login_credentials init");
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS login_credentials (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            username TEXT,
            password_hash TEXT
        );",
    );
    ensure_column(&conn, "totp_secret", "ALTER TABLE login_credentials ADD COLUMN totp_secret TEXT");
    ensure_column(&conn, "totp_recovery_hashes", "ALTER TABLE login_credentials ADD COLUMN totp_recovery_hashes TEXT");
    ensure_column(&conn, "api_key_hash", "ALTER TABLE login_credentials ADD COLUMN api_key_hash TEXT");
}

fn ensure_column(conn: &rusqlite::Connection, col: &str, ddl: &str) {
    let has: bool = conn
        .prepare("PRAGMA table_info(login_credentials)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .any(|c| c.map_or(false, |c| c == col));
    if !has {
        if let Err(e) = conn.execute(ddl, []) {
            tracing::error!("login_credentials 迁移失败 ({}): {}", col, e);
        }
    }
}

/// Read the single credentials row; absent table/row → empty Credentials.
pub fn get(pool: &DbPool) -> Result<Credentials, AppErrorKind> {
    // 表可能还没建（老库升级路径第一次读到）——这里建一次。
    ensure_table(pool);
    let conn = pool.get()?;
    let result = conn.query_row(
        "SELECT username, password_hash, totp_secret, totp_recovery_hashes, api_key_hash FROM login_credentials WHERE id = 1",
        [],
        |row| {
            Ok(Credentials {
                username: row.get(0)?,
                password_hash: row.get(1)?,
                totp_secret: row.get(2)?,
                totp_recovery_hashes: row.get(3)?,
                api_key_hash: row.get(4)?,
            })
        },
    );
    match result {
        Ok(c) => Ok(c),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Credentials::default()),
        Err(e) => Err(e.into()),
    }
}

fn clean(v: Option<&str>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Overwrite the username/password columns. Pass hashed password values
/// only — callers hash before this function.
pub fn save_userpass(
    pool: &DbPool,
    username: Option<&str>,
    password_hash: Option<&str>,
) -> Result<(), AppErrorKind> {
    ensure_table(pool);
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO login_credentials (id, username, password_hash) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET username = ?1, password_hash = ?2",
        params![clean(username), clean(password_hash)],
    )?;
    Ok(())
}

/// Overwrite the 2FA columns.
pub fn save_totp(
    pool: &DbPool,
    secret: Option<&str>,
    recovery_hashes: Option<&str>,
) -> Result<(), AppErrorKind> {
    ensure_table(pool);
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO login_credentials (id) VALUES (1)
         ON CONFLICT(id) DO UPDATE SET totp_secret = ?1, totp_recovery_hashes = ?2",
        params![clean(secret), clean(recovery_hashes)],
    )?;
    Ok(())
}

/// Overwrite the API key hash.
pub fn save_api_key_hash(pool: &DbPool, hash: Option<&str>) -> Result<(), AppErrorKind> {
    ensure_table(pool);
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO login_credentials (id) VALUES (1)
         ON CONFLICT(id) DO UPDATE SET api_key_hash = ?1",
        params![clean(hash)],
    )?;
    Ok(())
}

/// Clear all credential columns and drop the 2FA/API-key state
/// (used by settings-page reset paths; keeps the row itself).
pub fn clear_all(pool: &DbPool) -> Result<(), AppErrorKind> {
    ensure_table(pool);
    let conn = pool.get()?;
    conn.execute(
        "UPDATE login_credentials SET username = NULL, password_hash = NULL, totp_secret = NULL, totp_recovery_hashes = NULL, api_key_hash = NULL WHERE id = 1",
        [],
    )?;
    Ok(())
}

/// Migration helper: seed the account from a legacy plaintext/argon2
/// `app_settings.pass_word` value and a username. Called at startup when
/// the new `login_credentials` row doesn't exist yet — carries the old
/// single-password world into username+password without a forced re-setup.
pub fn migrate_from_legacy(
    pool: &DbPool,
    legacy_password_hash: Option<&str>,
) -> Result<Option<String>, AppErrorKind> {
    let creds = get(pool)?;
    if creds.has_login() {
        return Ok(None); // already migrated
    }
    match legacy_password_hash {
        Some(h) if !h.trim().is_empty() => {
            // Default username for one-off migration: "buyi" per owner's
            // explicit request; it can be changed from settings later.
            save_userpass(pool, Some("buyi"), Some(h.trim()))?;
            tracing::info!("已把旧管理员密码迁移为账户（默认用户名 buyi）");
            Ok(Some("buyi".to_string()))
        }
        _ => Ok(None),
    }
}

/// sha256 hex digest — API 密钥与恢复码的存储形态。
/// API 密钥本身是 32 随机字节（256-bit），sha256 不存明文即可：
/// 高熵令牌无需 argon2（那是给低熵口令用的），sha256 查表/暴力皆不可行。
pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Generate a fresh API key: `tgsk_` + 43 chars base62 (≈ 256-bit)。
/// 只在生成时返回明文一次，之后只存 sha256。
pub fn generate_api_key() -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    let body: String = (0..43)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect();
    format!("tgsk_{}", body)
}

/// Constant-time API key check against the stored hash.
pub fn verify_api_key(input: &str, stored_hash: &str) -> bool {
    if input.is_empty() || stored_hash.is_empty() {
        return false;
    }
    auth::secure_compare(&sha256_hex(input), stored_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_roundtrip() {
        let key = generate_api_key();
        assert!(key.starts_with("tgsk_"));
        assert_eq!(key.len(), 48);
        let hash = sha256_hex(&key);
        assert!(verify_api_key(&key, &hash));
        assert!(!verify_api_key(&key, &sha256_hex("other")));
        assert!(!verify_api_key("", &hash));
    }

    #[test]
    fn sha256_hex_known_vector() {
        // sha256("abc") = ba7816bf...
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
