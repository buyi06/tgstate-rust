use rand::Rng;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;
use tracing;

pub fn db_path(data_dir: &str) -> String {
    std::fs::create_dir_all(data_dir).ok();
    Path::new(data_dir)
        .join("file_metadata.db")
        .to_string_lossy()
        .to_string()
}

fn connect(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    Ok(conn)
}

pub fn init_db(data_dir: &str) {
    let path = db_path(data_dir);
    let conn = connect(&path).expect("Failed to open database");

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            filename TEXT NOT NULL,
            file_id TEXT NOT NULL UNIQUE,
            filesize INTEGER NOT NULL,
            upload_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            short_id TEXT UNIQUE
        );",
    )
    .expect("Failed to create files table");

    // Migration: add short_id column if missing
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

    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_files_short_id ON files(short_id);",
    )
    .ok();

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_settings (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            bot_token TEXT,
            channel_name TEXT,
            pass_word TEXT,
            picgo_api_key TEXT,
            base_url TEXT
        );",
    )
    .expect("Failed to create app_settings table");

    conn.execute(
        "INSERT OR IGNORE INTO app_settings (id) VALUES (1)",
        [],
    )
    .expect("Failed to init app_settings row");

    tracing::info!("数据库已成功初始化");
}

fn generate_short_id(length: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..length)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

pub fn add_file_metadata(
    db_path: &str,
    filename: &str,
    file_id: &str,
    filesize: i64,
) -> Result<String, String> {
    let conn = connect(db_path).map_err(|e| e.to_string())?;

    for _ in 0..5 {
        let short_id = generate_short_id(6);
        match conn.execute(
            "INSERT INTO files (filename, file_id, filesize, short_id) VALUES (?1, ?2, ?3, ?4)",
            params![filename, file_id, filesize, short_id],
        ) {
            Ok(_) => {
                tracing::info!("已添加文件元数据: {}, short_id: {}", filename, short_id);
                return Ok(short_id);
            }
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                // Check if file_id collision
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
                    // Old data without short_id
                    let new_sid = generate_short_id(6);
                    conn.execute(
                        "UPDATE files SET short_id = ?1 WHERE file_id = ?2",
                        params![new_sid, file_id],
                    )
                    .map_err(|e| e.to_string())?;
                    return Ok(new_sid);
                }
                // short_id collision, retry
                continue;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("Failed to generate unique short_id".into())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FileMetadata {
    pub filename: String,
    pub file_id: String,
    pub filesize: i64,
    pub upload_date: String,
    pub short_id: Option<String>,
}

pub fn get_all_files(db_path: &str) -> Result<Vec<FileMetadata>, String> {
    let conn = connect(db_path).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT filename, file_id, filesize, upload_date, short_id FROM files ORDER BY upload_date DESC",
        )
        .map_err(|e| e.to_string())?;

    let files = stmt
        .query_map([], |row| {
            Ok(FileMetadata {
                filename: row.get(0)?,
                file_id: row.get(1)?,
                filesize: row.get(2)?,
                upload_date: row.get::<_, String>(3).unwrap_or_default(),
                short_id: row.get(4).ok(),
            })
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

    Ok(files)
}

pub fn get_file_by_id(db_path: &str, identifier: &str) -> Result<Option<FileMetadata>, String> {
    let conn = connect(db_path).map_err(|e| e.to_string())?;
    let result = conn.query_row(
        "SELECT filename, filesize, upload_date, file_id, short_id FROM files WHERE short_id = ?1 OR file_id = ?1",
        params![identifier],
        |row| {
            Ok(FileMetadata {
                filename: row.get(0)?,
                filesize: row.get(1)?,
                upload_date: row.get::<_, String>(2).unwrap_or_default(),
                file_id: row.get(3)?,
                short_id: row.get(4).ok(),
            })
        },
    );

    match result {
        Ok(meta) => Ok(Some(meta)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

pub fn delete_file_metadata(db_path: &str, file_id: &str) -> Result<bool, String> {
    let conn = connect(db_path).map_err(|e| e.to_string())?;
    let rows = conn
        .execute("DELETE FROM files WHERE file_id = ?1", params![file_id])
        .map_err(|e| e.to_string())?;
    Ok(rows > 0)
}

pub fn delete_file_by_message_id(db_path: &str, message_id: i64) -> Result<Option<String>, String> {
    let conn = connect(db_path).map_err(|e| e.to_string())?;
    let pattern = format!("{}:%", message_id);

    let file_id: Option<String> = conn
        .query_row(
            "SELECT file_id FROM files WHERE file_id LIKE ?1",
            params![pattern],
            |row| row.get(0),
        )
        .ok();

    if let Some(ref fid) = file_id {
        conn.execute("DELETE FROM files WHERE file_id = ?1", params![fid])
            .map_err(|e| e.to_string())?;
        tracing::info!(
            "已从数据库中删除与消息ID {} 关联的文件: {}",
            message_id,
            fid
        );
    }

    Ok(file_id)
}

pub fn get_app_settings_from_db(
    db_path: &str,
) -> Result<HashMap<String, Option<String>>, String> {
    let conn = connect(db_path).map_err(|e| e.to_string())?;
    let result = conn.query_row(
        "SELECT bot_token, channel_name, pass_word, picgo_api_key, base_url FROM app_settings WHERE id = 1",
        [],
        |row| {
            let mut map = HashMap::new();
            map.insert("BOT_TOKEN".to_string(), row.get::<_, Option<String>>(0)?);
            map.insert("CHANNEL_NAME".to_string(), row.get::<_, Option<String>>(1)?);
            map.insert("PASS_WORD".to_string(), row.get::<_, Option<String>>(2)?);
            map.insert("PICGO_API_KEY".to_string(), row.get::<_, Option<String>>(3)?);
            map.insert("BASE_URL".to_string(), row.get::<_, Option<String>>(4)?);
            Ok(map)
        },
    );

    match result {
        Ok(map) => Ok(map),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(HashMap::new()),
        Err(e) => Err(e.to_string()),
    }
}

fn norm(v: Option<&str>) -> Option<String> {
    v.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save_app_settings_to_db(
    db_path: &str,
    payload: &HashMap<String, Option<String>>,
) -> Result<(), String> {
    let conn = connect(db_path).map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE app_settings SET bot_token = ?1, channel_name = ?2, pass_word = ?3, picgo_api_key = ?4, base_url = ?5 WHERE id = 1",
        params![
            norm(payload.get("BOT_TOKEN").and_then(|v| v.as_deref())),
            norm(payload.get("CHANNEL_NAME").and_then(|v| v.as_deref())),
            norm(payload.get("PASS_WORD").and_then(|v| v.as_deref())),
            norm(payload.get("PICGO_API_KEY").and_then(|v| v.as_deref())),
            norm(payload.get("BASE_URL").and_then(|v| v.as_deref())),
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn reset_app_settings_in_db(db_path: &str) -> Result<(), String> {
    let mut payload = HashMap::new();
    payload.insert("BOT_TOKEN".to_string(), None);
    payload.insert("CHANNEL_NAME".to_string(), None);
    payload.insert("PASS_WORD".to_string(), None);
    payload.insert("PICGO_API_KEY".to_string(), None);
    payload.insert("BASE_URL".to_string(), None);
    save_app_settings_to_db(db_path, &payload)
}
