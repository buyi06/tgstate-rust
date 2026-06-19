use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rand::Rng;
use rusqlite::params;
use std::collections::HashMap;
use std::path::Path;
use tracing;

use crate::constants;
use crate::error::AppErrorKind;

pub type DbPool = Pool<SqliteConnectionManager>;

pub fn db_path(data_dir: &str) -> String {
    std::fs::create_dir_all(data_dir).ok();
    Path::new(data_dir)
        .join("file_metadata.db")
        .to_string_lossy()
        .to_string()
}

pub fn init_db(data_dir: &str) -> DbPool {
    let path = db_path(data_dir);
    let manager = SqliteConnectionManager::file(&path);
    let pool = Pool::builder()
        .max_size(10)
        .min_idle(Some(2))
        .connection_customizer(Box::new(SqliteInitializer))
        .build(manager)
        .expect("Failed to create database pool");

    let conn = pool.get().expect("Failed to get connection for init");

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            filename TEXT NOT NULL,
            file_id TEXT NOT NULL UNIQUE,
            filesize INTEGER NOT NULL,
            upload_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            short_id TEXT UNIQUE,
            share_password TEXT
        );",
    )
    .expect("Failed to create files table");

    let has_short_id: bool = conn
        .prepare("PRAGMA table_info(files)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .any(|col| col.map_or(false, |c| c == "short_id"));

    if !has_short_id {
        tracing::info!("Migrating database: adding short_id column...");
        if let Err(e) = conn.execute("ALTER TABLE files ADD COLUMN short_id TEXT", []) {
            tracing::error!("Migration warning: Failed to add short_id column: {}", e);
        }
    }

    // Migration: per-file share password (argon2 hash, nullable).
    let has_share_password: bool = conn
        .prepare("PRAGMA table_info(files)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .any(|col| col.map_or(false, |c| c == "share_password"));
    if !has_share_password {
        tracing::info!("Migrating database: adding share_password column...");
        if let Err(e) = conn.execute("ALTER TABLE files ADD COLUMN share_password TEXT", []) {
            tracing::error!("Migration warning: Failed to add share_password column: {}", e);
        }
    }

    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_files_short_id ON files(short_id);",
    )
    .ok();

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_files_upload_date ON files(upload_date DESC);",
    )
    .ok();

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_settings (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            bot_token TEXT,
            channel_name TEXT,
            pass_word TEXT,
            base_url TEXT
        );",
    )
    .expect("Failed to create app_settings table");

    conn.execute(
        "INSERT OR IGNORE INTO app_settings (id) VALUES (1)",
        [],
    )
    .expect("Failed to init app_settings row");

    // Migration: add session_token column if missing
    let has_session_token: bool = conn
        .prepare("PRAGMA table_info(app_settings)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .any(|col| col.map_or(false, |c| c == "session_token"));

    if !has_session_token {
        tracing::info!("Migrating database: adding session_token column...");
        if let Err(e) = conn.execute("ALTER TABLE app_settings ADD COLUMN session_token TEXT", []) {
            tracing::error!("Migration warning: Failed to add session_token column: {}", e);
        }
    }

    tracing::info!("数据库已成功初始化");
    pool
}

#[derive(Debug)]
struct SqliteInitializer;

impl r2d2::CustomizeConnection<rusqlite::Connection, rusqlite::Error> for SqliteInitializer {
    fn on_acquire(&self, conn: &mut rusqlite::Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        Ok(())
    }
}

fn generate_short_id(length: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..length)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

pub fn add_file_metadata(
    pool: &DbPool,
    filename: &str,
    file_id: &str,
    filesize: i64,
) -> Result<String, AppErrorKind> {
    let conn = pool.get()?;

    for _ in 0..5 {
        let short_id = generate_short_id(constants::SHORT_ID_LENGTH);
        match conn.execute(
            "INSERT INTO files (filename, file_id, filesize, short_id) VALUES (?1, ?2, ?3, ?4)",
            params![filename, file_id, filesize, short_id],
        ) {
            Ok(_) => {
                // Only log a short_id prefix. `/d/<short_id>` is a public,
                // unauthenticated download endpoint; the short_id is therefore
                // a bearer capability and must not be logged in full.
                let short_id_preview = short_id.chars().take(2).collect::<String>();
                tracing::info!(
                    "已添加文件元数据: {}, short_id: {}***",
                    filename,
                    short_id_preview
                );
                return Ok(short_id);
            }
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                let existing: Option<String> = conn
                    .query_row(
                        "SELECT short_id FROM files WHERE file_id = ?1",
                        params![file_id],
                        |row| row.get(0),
                    )
                    .ok();

                if let Some(existing_sid) = existing {
                    if !existing_sid.is_empty() {
                        return Ok(existing_sid);
                    }
                    let new_sid = generate_short_id(constants::SHORT_ID_LENGTH);
                    conn.execute(
                        "UPDATE files SET short_id = ?1 WHERE file_id = ?2",
                        params![new_sid, file_id],
                    )?;
                    return Ok(new_sid);
                }
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppErrorKind::Other("Failed to generate unique short_id".into()))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FileMetadata {
    pub filename: String,
    pub file_id: String,
    pub filesize: i64,
    pub upload_date: String,
    pub short_id: Option<String>,
    // 分享密码的 argon2 哈希；绝不序列化给前端（只暴露 has_password 布尔）。
    #[serde(skip_serializing)]
    pub share_password: Option<String>,
}

pub fn get_all_files(pool: &DbPool) -> Result<Vec<FileMetadata>, AppErrorKind> {
    let conn = pool.get()?;
    let mut stmt = conn
        .prepare(
            "SELECT filename, file_id, filesize, upload_date, short_id, share_password FROM files ORDER BY upload_date DESC",
        )?;

    let files = stmt
        .query_map([], |row| {
            Ok(FileMetadata {
                filename: row.get(0)?,
                file_id: row.get(1)?,
                filesize: row.get(2)?,
                upload_date: row.get::<_, String>(3).unwrap_or_default(),
                short_id: row.get(4).ok(),
                share_password: row.get(5).ok(),
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(files)
}

pub fn get_file_by_id(pool: &DbPool, identifier: &str) -> Result<Option<FileMetadata>, AppErrorKind> {
    let conn = pool.get()?;
    let result = conn.query_row(
        "SELECT filename, filesize, upload_date, file_id, short_id, share_password FROM files WHERE short_id = ?1 OR file_id = ?1",
        params![identifier],
        |row| {
            Ok(FileMetadata {
                filename: row.get(0)?,
                filesize: row.get(1)?,
                upload_date: row.get::<_, String>(2).unwrap_or_default(),
                file_id: row.get(3)?,
                short_id: row.get(4).ok(),
                share_password: row.get(5).ok(),
            })
        },
    );

    match result {
        Ok(meta) => Ok(Some(meta)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_file_metadata(pool: &DbPool, file_id: &str) -> Result<bool, AppErrorKind> {
    let conn = pool.get()?;
    let rows = conn
        .execute("DELETE FROM files WHERE file_id = ?1", params![file_id])?;
    Ok(rows > 0)
}

/// 设置/清除某个文件的分享密码（传入已哈希的值，`None` 表示清除）。
/// identifier 可以是 short_id 或复合 file_id。返回是否命中了某一行。
pub fn set_share_password(
    pool: &DbPool,
    identifier: &str,
    hashed: Option<&str>,
) -> Result<bool, AppErrorKind> {
    let conn = pool.get()?;
    let rows = conn.execute(
        "UPDATE files SET share_password = ?1 WHERE short_id = ?2 OR file_id = ?2",
        params![hashed, identifier],
    )?;
    Ok(rows > 0)
}

pub fn delete_file_by_message_id(pool: &DbPool, message_id: i64) -> Result<Option<String>, AppErrorKind> {
    let conn = pool.get()?;
    let pattern = format!("{}:%", message_id);

    let file_id: Option<String> = conn
        .query_row(
            "SELECT file_id FROM files WHERE file_id LIKE ?1",
            params![pattern],
            |row| row.get(0),
        )
        .ok();

    if let Some(ref fid) = file_id {
        conn.execute("DELETE FROM files WHERE file_id = ?1", params![fid])?;
        tracing::info!(
            "已从数据库中删除与消息ID {} 关联的文件: {}",
            message_id,
            fid
        );
    }

    Ok(file_id)
}

pub fn get_app_settings_from_db(
    pool: &DbPool,
) -> Result<HashMap<String, Option<String>>, AppErrorKind> {
    let conn = pool.get()?;
    let result = conn.query_row(
        "SELECT bot_token, channel_name, pass_word, base_url, session_token FROM app_settings WHERE id = 1",
        [],
        |row| {
            let mut map = HashMap::new();
            map.insert("BOT_TOKEN".to_string(), row.get::<_, Option<String>>(0)?);
            map.insert("CHANNEL_NAME".to_string(), row.get::<_, Option<String>>(1)?);
            map.insert("PASS_WORD".to_string(), row.get::<_, Option<String>>(2)?);
            map.insert("BASE_URL".to_string(), row.get::<_, Option<String>>(3)?);
            map.insert("SESSION_TOKEN".to_string(), row.get::<_, Option<String>>(4)?);
            Ok(map)
        },
    );

    match result {
        Ok(map) => Ok(map),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(HashMap::new()),
        Err(e) => Err(e.into()),
    }
}

fn norm(v: Option<&str>) -> Option<String> {
    v.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save_app_settings_to_db(
    pool: &DbPool,
    payload: &HashMap<String, Option<String>>,
) -> Result<(), AppErrorKind> {
    let conn = pool.get()?;
    conn.execute(
        "UPDATE app_settings SET bot_token = ?1, channel_name = ?2, pass_word = ?3, base_url = ?4, session_token = ?5 WHERE id = 1",
        params![
            norm(payload.get("BOT_TOKEN").and_then(|v| v.as_deref())),
            norm(payload.get("CHANNEL_NAME").and_then(|v| v.as_deref())),
            norm(payload.get("PASS_WORD").and_then(|v| v.as_deref())),
            norm(payload.get("BASE_URL").and_then(|v| v.as_deref())),
            norm(payload.get("SESSION_TOKEN").and_then(|v| v.as_deref())),
        ],
    )?;
    Ok(())
}

pub fn reset_app_settings_in_db(pool: &DbPool) -> Result<(), AppErrorKind> {
    let mut payload = HashMap::new();
    payload.insert("BOT_TOKEN".to_string(), None);
    payload.insert("CHANNEL_NAME".to_string(), None);
    payload.insert("PASS_WORD".to_string(), None);
    payload.insert("BASE_URL".to_string(), None);
    payload.insert("SESSION_TOKEN".to_string(), None);
    save_app_settings_to_db(pool, &payload)
}
