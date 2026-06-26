use std::sync::{Arc, OnceLock};

use axum::extract::{Multipart, State};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use bytes::BytesMut;
use tokio::sync::Semaphore;

use crate::config;
use crate::constants;
use crate::database;
use crate::error::http_error;
use crate::state::AppState;
use crate::telegram::service::TelegramService;

/// Sanitize filename: extract basename, limit length, remove dangerous chars.
fn sanitize_filename(raw: &str) -> String {
    let name = std::path::Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload");
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '\0')
        .collect();
    if clean.is_empty() {
        return "upload".to_string();
    }
    // UTF-8-safe byte-length cap.
    if clean.len() <= 255 {
        return clean;
    }
    let mut cutoff = 0;
    for (idx, _) in clean.char_indices() {
        if idx > 255 {
            break;
        }
        cutoff = idx;
    }
    clean[..cutoff].to_string()
}

async fn upload_file(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, impl IntoResponse> {
    // 鉴权完全由 `auth_middleware` 负责：能到达这里，要么请求带着有效 session cookie，
    // 要么处于“尚未设置密码”的首启阶段（该阶段中间件会把除引导页外的一切挡在外面，
    // 所以正常情况下不会有未授权请求到达上传）。PicGo / X-Api-Key 鉴权已移除。
    let app_settings = config::get_app_settings(&state.settings, &state.db_pool);

    let bot_token = app_settings
        .get("BOT_TOKEN")
        .and_then(|v| v.as_deref())
        .unwrap_or("");
    let channel_name = app_settings
        .get("CHANNEL_NAME")
        .and_then(|v| v.as_deref())
        .unwrap_or("");

    if bot_token.is_empty() || channel_name.is_empty() {
        return Err(http_error(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "缺少 BOT_TOKEN 或 CHANNEL_NAME，无法上传",
            "cfg_missing",
        ));
    }

    let tg_service = TelegramService::new(
        bot_token.to_string(),
        channel_name.to_string(),
        state.http_client.clone(),
    );

    let mut upload_result: Option<Result<String, String>> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => {
                return Err(http_error(
                    axum::http::StatusCode::BAD_REQUEST,
                    "上传数据读取失败",
                    "multipart_error",
                ));
            }
        };
        if field.name() == Some("file") {
            let raw_filename = field.file_name().unwrap_or("upload").to_string();
            let filename = sanitize_filename(&raw_filename);
            upload_result = Some(
                stream_upload_to_telegram(&tg_service, field, &filename, &state.db_pool).await,
            );
        }
    }

    let short_id = upload_result
        .ok_or_else(|| http_error(axum::http::StatusCode::BAD_REQUEST, "未提供文件", "no_file"))?
        .map_err(|e| {
            tracing::error!("文件上传失败: {}", e);
            http_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "文件上传失败",
                "upload_failed",
            )
        })?;

    // 广播 add 事件：文件管理页（及其它打开的标签页）无需刷新即可实时出现新行。
    if let Ok(Some(meta)) = database::get_file_by_id(&state.db_pool, &short_id) {
        let ev = crate::events::build_file_event(
            "add",
            &meta.file_id,
            Some(&meta.filename),
            Some(meta.filesize),
            Some(&meta.upload_date),
            meta.short_id.as_deref(),
        );
        state.event_bus.publish(serde_json::to_string(&ev).unwrap_or_default());
    }

    let download_path = format!("/d/{}", short_id);
    Ok(Json(serde_json::json!({
        "file_id": short_id,
        "short_id": short_id,
        "download_path": download_path,
        "path": download_path,
        "url": download_path,
    })))
}

/// 解析大文件分块并发上传数（env `UPLOAD_CONCURRENCY`，钳制到 1..=16）。越大越快也越吃
/// 内存（峰值 ≈ 值 × 19.5MB）。
fn parse_upload_concurrency(raw: Option<String>, default: usize) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v >= 1 && *v <= 16)
        .unwrap_or(default)
}

fn upload_concurrency() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        parse_upload_concurrency(
            std::env::var("UPLOAD_CONCURRENCY").ok(),
            constants::MAX_CONCURRENT_CHUNK_UPLOADS,
        )
    })
}

/// 把 multipart 上传流式切块、**并发**上传到 Telegram。
///
/// 读流是单线程顺序的，但每凑满一个 `TELEGRAM_CHUNK_SIZE` 就立刻派发给并发上传任务，
/// 最多 `upload_concurrency()` 块同时在传（用信号量背压读循环，限制峰值内存）。所有块
/// 都走并发池、互不等待——重建只依赖 manifest，不依赖 Telegram 的 reply 关系，因此不再
/// 需要先同步上传一个“锚点”块（这正是相比旧实现的提速点：首块不再串行阻塞后续块）。
/// 并发完成是乱序的，按 index 重排后写 manifest。任一失败都尽力删除已上传块以免留孤儿。
async fn stream_upload_to_telegram(
    tg_service: &TelegramService,
    mut field: axum::extract::multipart::Field<'_>,
    filename: &str,
    db_pool: &database::DbPool,
) -> Result<String, String> {
    let chunk_size = constants::TELEGRAM_CHUNK_SIZE;
    let concurrency = upload_concurrency();
    let mut buffer = BytesMut::with_capacity(chunk_size);
    let mut total_size: usize = 0;

    let sem = Arc::new(Semaphore::new(concurrency));
    let mut tasks: Vec<tokio::task::JoinHandle<Result<(usize, String), String>>> = Vec::new();
    let mut chunk_index: usize = 0;

    loop {
        let bytes = match field.chunk().await {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => {
                // 读取上传流出错（客户端中断 / 超出 body 上限）：清理已上传块，
                // 绝不能把被截断的内容当作完整文件写入清单和数据库。
                let mut done = Vec::new();
                for task in std::mem::take(&mut tasks) {
                    if let Ok(Ok((_, c))) = task.await {
                        done.push(c);
                    }
                }
                cleanup_uploaded_chunks(tg_service, &done).await;
                return Err(format!("读取上传数据失败: {}", e));
            }
        };
        buffer.extend_from_slice(&bytes);
        total_size += bytes.len();

        while buffer.len() >= chunk_size {
            let data = buffer.split_to(chunk_size).freeze().to_vec();
            let idx = chunk_index;
            chunk_index += 1;
            let chunk_name = format!("{}.part{}", filename, idx + 1);
            // 先拿许可（背压），再 spawn，确保最多 concurrency 块在途。
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| e.to_string())?;
            let tg = tg_service.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = permit;
                let message = tg.send_document_with_retry(data, &chunk_name, None).await?;
                let doc = message
                    .document
                    .ok_or_else(|| "No document in chunk response".to_string())?;
                Ok((idx, format!("{}:{}", message.message_id, doc.file_id)))
            }));
        }
    }

    // 没凑满过一个完整块 → 小文件，单次直传。
    if chunk_index == 0 {
        if buffer.is_empty() {
            return Err("文件为空".into());
        }
        tracing::info!("直接上传文件: {} ({}字节)", filename, total_size);
        let data = buffer.freeze().to_vec();
        let message = tg_service
            .send_document_with_retry(data, filename, None)
            .await?;
        let doc = message.document.ok_or("No document in response")?;
        let composite_id = format!("{}:{}", message.message_id, doc.file_id);
        let short_id =
            database::add_file_metadata(db_pool, filename, &composite_id, total_size as i64)
                .map_err(|e| e.to_string())?;
        return Ok(short_id);
    }

    // 末尾不足一块的残余作为最后一块派发。
    if !buffer.is_empty() {
        let data = buffer.freeze().to_vec();
        let idx = chunk_index;
        let chunk_name = format!("{}.part{}", filename, idx + 1);
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| e.to_string())?;
        let tg = tg_service.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let message = tg.send_document_with_retry(data, &chunk_name, None).await?;
            let doc = message
                .document
                .ok_or_else(|| "No document in chunk response".to_string())?;
            Ok((idx, format!("{}:{}", message.message_id, doc.file_id)))
        }));
    }

    // 等待所有并发块完成，记录首个错误。
    let mut results: Vec<(usize, String)> = Vec::new();
    let mut first_err: Option<String> = None;
    for task in tasks {
        match task.await {
            Ok(Ok(pair)) => results.push(pair),
            Ok(Err(e)) => {
                first_err.get_or_insert(e);
            }
            Err(join_err) => {
                first_err.get_or_insert(format!("上传任务异常: {}", join_err));
            }
        }
    }

    if let Some(e) = first_err {
        let done: Vec<String> = results.into_iter().map(|(_, c)| c).collect();
        cleanup_uploaded_chunks(tg_service, &done).await;
        return Err(e);
    }

    // 并发完成乱序；按 index 重排，供下载端按序拼接。
    let chunk_ids = assemble_ordered_chunk_ids(results);

    tracing::info!(
        "分块上传完成: {} ({}MB, {} 块, 并发 {})",
        filename,
        total_size / (1024 * 1024),
        chunk_ids.len(),
        concurrency
    );

    let manifest = build_blob_manifest(filename, &chunk_ids);
    let manifest_name = format!("{}.manifest", filename);
    let manifest_msg = match tg_service
        .send_document_with_retry(manifest.into_bytes(), &manifest_name, None)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            cleanup_uploaded_chunks(tg_service, &chunk_ids).await;
            return Err(e);
        }
    };
    let manifest_message_id = manifest_msg.message_id;
    let doc = match manifest_msg.document {
        Some(d) => d,
        None => {
            let _ = tg_service.delete_message(manifest_message_id).await;
            cleanup_uploaded_chunks(tg_service, &chunk_ids).await;
            return Err("No document in manifest response".into());
        }
    };
    let manifest_composite = format!("{}:{}", manifest_message_id, doc.file_id);

    match database::add_file_metadata(db_pool, filename, &manifest_composite, total_size as i64) {
        Ok(short_id) => Ok(short_id),
        Err(e) => {
            // DB write failed after everything uploaded — roll back the channel.
            let _ = tg_service.delete_message(manifest_message_id).await;
            cleanup_uploaded_chunks(tg_service, &chunk_ids).await;
            Err(e.to_string())
        }
    }
}

/// Restore ascending chunk order (concurrent uploads finish out of order) and drop the index.
fn assemble_ordered_chunk_ids(mut results: Vec<(usize, String)>) -> Vec<String> {
    results.sort_by_key(|(idx, _)| *idx);
    results.into_iter().map(|(_, id)| id).collect()
}

/// Build the `tgstate-blob` manifest body: magic line, original filename, then
/// one `message_id:file_id` per chunk. `serve_file` parses this exact layout.
fn build_blob_manifest(filename: &str, chunk_ids: &[String]) -> String {
    let mut manifest = String::from("tgstate-blob\n");
    manifest.push_str(filename);
    manifest.push('\n');
    for cid in chunk_ids {
        manifest.push_str(cid);
        manifest.push('\n');
    }
    manifest
}

/// Extract the Telegram message_id from a `"message_id:file_id"` composite.
fn chunk_message_id(composite: &str) -> Option<i64> {
    composite
        .split_once(':')
        .and_then(|(mid, _)| mid.parse::<i64>().ok())
}

/// Best-effort deletion of already-uploaded chunks after a failed multi-chunk upload.
async fn cleanup_uploaded_chunks(tg_service: &TelegramService, composites: &[String]) {
    for composite in composites {
        if let Some(mid) = chunk_message_id(composite) {
            let _ = tg_service.delete_message(mid).await;
        }
    }
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/upload", post(upload_file))
}

#[cfg(test)]
mod tests {
    #[test]
    fn ordered_chunk_ids_sort_by_index_regardless_of_completion_order() {
        let out = super::assemble_ordered_chunk_ids(vec![
            (2, "30:c".to_string()),
            (0, "10:a".to_string()),
            (1, "20:b".to_string()),
        ]);
        assert_eq!(
            out,
            vec!["10:a".to_string(), "20:b".to_string(), "30:c".to_string()]
        );
    }

    #[test]
    fn blob_manifest_layout_matches_download_parser() {
        let chunk_ids = vec!["10:a".to_string(), "20:b".to_string()];
        let manifest = super::build_blob_manifest("my file.bin", &chunk_ids);
        let lines: Vec<&str> = manifest.lines().collect();
        assert_eq!(lines[0], "tgstate-blob");
        assert_eq!(lines[1], "my file.bin");
        assert_eq!(lines[2..].to_vec(), vec!["10:a", "20:b"]);
        // serve_file rejects manifests with fewer than 3 lines.
        assert!(lines.len() >= 3);
    }

    #[test]
    fn chunk_message_id_parses_leading_int_before_first_colon() {
        assert_eq!(super::chunk_message_id("123:ABCdef"), Some(123));
        assert_eq!(super::chunk_message_id("999"), None); // no colon
        assert_eq!(super::chunk_message_id("x:y"), None); // non-numeric id
        assert_eq!(super::chunk_message_id(":abc"), None); // empty message id
    }

    #[test]
    fn upload_concurrency_parses_and_clamps_to_1_16() {
        let d = crate::constants::MAX_CONCURRENT_CHUNK_UPLOADS;
        assert_eq!(super::parse_upload_concurrency(Some("5".into()), d), 5);
        assert_eq!(super::parse_upload_concurrency(Some(" 2 ".into()), d), 2);
        assert_eq!(super::parse_upload_concurrency(Some("1".into()), d), 1);
        assert_eq!(super::parse_upload_concurrency(Some("16".into()), d), 16);
        assert_eq!(super::parse_upload_concurrency(Some("0".into()), d), d); // below min
        assert_eq!(super::parse_upload_concurrency(Some("17".into()), d), d); // above max
        assert_eq!(super::parse_upload_concurrency(Some("abc".into()), d), d); // non-numeric
        assert_eq!(super::parse_upload_concurrency(None, d), d); // unset
    }
}
