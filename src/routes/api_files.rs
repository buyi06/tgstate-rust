use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::stream::{self, StreamExt};
use serde::Deserialize;

use crate::config;
use crate::database;
use crate::error::http_error;
use crate::state::AppState;
use crate::telegram::service::TelegramService;

#[derive(Deserialize)]
pub struct DownloadQuery {
    download: Option<String>,
}

#[derive(Deserialize)]
pub struct BatchDeleteRequest {
    file_ids: Vec<String>,
}

#[derive(Deserialize)]
pub struct SharePasswordRequest {
    password: String,
}

fn get_telegram_service(state: &AppState) -> Result<TelegramService, impl IntoResponse> {
    let app_settings = config::get_app_settings(&state.settings, &state.db_pool);
    let token = app_settings
        .get("BOT_TOKEN")
        .and_then(|v| v.as_deref())
        .unwrap_or("")
        .to_string();
    let channel = app_settings
        .get("CHANNEL_NAME")
        .and_then(|v| v.as_deref())
        .unwrap_or("")
        .to_string();

    if token.is_empty() || channel.is_empty() {
        return Err(http_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Bot 未配置",
            "bot_not_configured",
        ));
    }

    Ok(TelegramService::new(
        token,
        channel,
        state.http_client.clone(),
    ))
}

/// 下载标识符的基本校验：非空、限长、无控制字符。短链、复合 file_id、旧版直链
/// 都先过这里，挡掉畸形输入。
fn invalid_identifier(s: &str) -> bool {
    s.is_empty() || s.len() > 128 || s.chars().any(|c| c.is_control() || c == '\0')
}

fn guess_content_type(filename: &str) -> String {
    let mime = mime_guess::from_path(filename).first_or_octet_stream();
    let mime_str = mime.to_string();

    // Add charset for text types
    if mime_str.starts_with("text/") && !mime_str.contains("charset") {
        format!("{}; charset=utf-8", mime_str)
    } else {
        mime_str
    }
}

fn content_disposition(filename: &str, force_download: bool) -> String {
    // Allow-list of extensions that are safe to render inline on the
    // download host. We deliberately EXCLUDE executable/active content such
    // as `svg`, `html`, `htm`, `xml`, `js` and `css`, which browsers will
    // execute scripts from. Those are served as attachments.
    let preview_extensions = [
        "jpg", "jpeg", "png", "gif", "webp", "bmp", "ico", "tiff", "mp4", "webm", "ogg", "mp3",
        "wav", "flac", "pdf", "txt", "json", "csv", "md", "log",
    ];

    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();

    let is_inline = !force_download && preview_extensions.contains(&ext.as_str());

    let encoded_name =
        percent_encoding::utf8_percent_encode(filename, percent_encoding::NON_ALPHANUMERIC)
            .to_string();

    if is_inline {
        format!("inline; filename*=UTF-8''{}", encoded_name)
    } else {
        format!("attachment; filename*=UTF-8''{}", encoded_name)
    }
}

#[allow(dead_code)]
fn chunk_download_failed_response(chunk_id: &str) -> Response {
    // Log the chunk_id for operators; do NOT include it in the response body
    // because it reveals internal manifest structure to clients.
    tracing::error!("chunk download failed: {}", chunk_id);
    http_error(
        StatusCode::BAD_GATEWAY,
        "文件下载失败",
        "chunk_download_failed",
    )
    .into_response()
}

async fn serve_file(
    state: &AppState,
    tg_service: &TelegramService,
    file_id: &str,
    filename: &str,
    headers: &HeaderMap,
    force_download: bool,
    is_head: bool,
) -> Response {
    // Parse composite file_id "message_id:real_file_id"
    let real_file_id = if let Some(pos) = file_id.find(':') {
        &file_id[pos + 1..]
    } else {
        file_id
    };

    // Get download URL
    let download_url = match tg_service.get_download_url(real_file_id).await {
        Ok(Some(url)) => url,
        Ok(None) => {
            return http_error(StatusCode::NOT_FOUND, "文件未找到或链接已过期", "not_found")
                .into_response()
        }
        Err(e) => {
            tracing::error!("获取下载链接失败: {}", e);
            return http_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "无法连接到 Telegram",
                "tg_error",
            )
            .into_response();
        }
    };

    let client = &state.http_client;

    // Peek first 128 bytes to check if manifest
    let peek_resp = match client
        .get(&download_url)
        .header("Range", "bytes=0-127")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("下载失败: {}", e);
            return http_error(StatusCode::BAD_GATEWAY, "无法下载文件", "download_error")
                .into_response();
        }
    };

    let peek_bytes = match peek_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("读取文件失败: {}", e);
            return http_error(StatusCode::BAD_GATEWAY, "读取文件失败", "read_error")
                .into_response();
        }
    };

    // Check if manifest
    if peek_bytes.starts_with(b"tgstate-blob\n") {
        // Download full manifest
        let full_resp = match client.get(&download_url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("下载清单失败: {}", e);
                return http_error(
                    StatusCode::BAD_GATEWAY,
                    "下载文件失败",
                    "download_error",
                )
                .into_response();
            }
        };
        let manifest_bytes = match full_resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("读取清单失败: {}", e);
                return http_error(
                    StatusCode::BAD_GATEWAY,
                    "读取文件失败",
                    "read_error",
                )
                .into_response();
            }
        };

        let manifest_str = String::from_utf8_lossy(&manifest_bytes);
        let lines: Vec<&str> = manifest_str.lines().collect();
        if lines.len() < 3 {
            return http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "清单文件格式错误",
                "manifest_error",
            )
            .into_response();
        }

        let original_filename = lines[1];
        let chunk_ids: Vec<String> = lines[2..].iter().map(|s| s.to_string()).collect();

        let ct = guess_content_type(original_filename);
        let cd = content_disposition(original_filename, force_download);

        if is_head {
            return Response::builder()
                .header("Content-Type", ct)
                .header("Content-Disposition", cd)
                .header("Accept-Ranges", "bytes")
                .header("X-Content-Type-Options", "nosniff")
                .body(Body::empty())
                .unwrap();
        }

        // 并发预取：每个 chunk 的 getFile + 建连在前一个 chunk 仍在传输时就发起，最多
        // PREFETCH 个在途；body 仍按 chunk 顺序流式输出（保序），单块不整体进内存。相比
        // 旧的“逐块串行 getFile + 串行下载”，块间往返被重叠掉，明显提速。
        const PREFETCH: usize = 3;
        let tg = tg_service.clone();
        let http = client.clone();
        let body_stream = async_stream::stream! {
            let mut fetches = Box::pin(stream::iter(chunk_ids.into_iter().map(|composite| {
                let tg = tg.clone();
                let http = http.clone();
                async move {
                    let real_id = match composite.split_once(':') {
                        Some((_, b)) => b.to_string(),
                        None => composite.clone(),
                    };
                    // 取下载 URL 并发起 GET；失败重试一次（刷新 URL）。
                    for attempt in 0..2 {
                        if let Ok(Some(url)) = tg.get_download_url(&real_id).await {
                            if let Ok(resp) = http.get(&url).send().await {
                                if resp.status().is_success() {
                                    return Some(resp);
                                }
                            }
                        }
                        if attempt == 0 {
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                    None
                }
            }))
            .buffered(PREFETCH));

            while let Some(maybe_resp) = fetches.next().await {
                let resp = match maybe_resp {
                    Some(r) => r,
                    None => {
                        yield Err::<bytes::Bytes, std::io::Error>(std::io::Error::other(
                            "chunk download failed".to_string(),
                        ));
                        return;
                    }
                };
                let mut bs = resp.bytes_stream();
                while let Some(chunk) = bs.next().await {
                    match chunk {
                        Ok(bytes) => yield Ok::<_, std::io::Error>(bytes),
                        Err(e) => {
                            yield Err::<bytes::Bytes, std::io::Error>(std::io::Error::other(
                                format!("chunk stream error: {}", e),
                            ));
                            return;
                        }
                    }
                }
            }
        };

        return Response::builder()
            .header("Content-Type", ct)
            .header("Content-Disposition", cd)
            .header("Accept-Ranges", "bytes")
            .header("X-Content-Type-Options", "nosniff")
            .body(Body::from_stream(body_stream))
            .unwrap();
    }

    // Regular file - stream from Telegram
    let ct = guess_content_type(filename);
    let cd = content_disposition(filename, force_download);

    // Handle Range request - proxy Range header to Telegram
    if let Some(range_header) = headers.get("range").and_then(|v| v.to_str().ok()) {
        let range_resp = match client
            .get(&download_url)
            .header("Range", range_header)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Range 请求失败: {}", e);
                return http_error(StatusCode::BAD_GATEWAY, "无法下载文件", "download_error")
                    .into_response();
            }
        };

        let status = range_resp.status();
        let mut builder = Response::builder()
            .status(status)
            .header("Content-Type", &ct)
            .header("Content-Disposition", &cd)
            .header("Accept-Ranges", "bytes")
            .header("X-Content-Type-Options", "nosniff");

        // Forward Content-Range and Content-Length from upstream
        if let Some(cr) = range_resp.headers().get("content-range") {
            builder = builder.header("Content-Range", cr);
        }
        if let Some(cl) = range_resp.headers().get("content-length") {
            builder = builder.header("Content-Length", cl);
        }

        let stream = range_resp.bytes_stream();
        return builder
            .body(Body::from_stream(stream.map(|r| {
                r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            })))
            .unwrap();
    }

    // Full file - stream
    let full_resp = match client.get(&download_url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("下载失败: {}", e);
            return http_error(StatusCode::BAD_GATEWAY, "无法下载文件", "download_error")
                .into_response();
        }
    };

    let mut builder = Response::builder()
        .header("Content-Type", ct)
        .header("Content-Disposition", cd)
        .header("Accept-Ranges", "bytes")
        .header("X-Content-Type-Options", "nosniff");

    if let Some(cl) = full_resp.headers().get("content-length") {
        builder = builder.header("Content-Length", cl);
    }

    if is_head {
        return builder.body(Body::empty()).unwrap();
    }

    let stream = full_resp.bytes_stream();
    builder
        .body(Body::from_stream(stream.map(|r| {
            r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        })))
        .unwrap()
}

async fn download_file_short(
    State(state): State<Arc<AppState>>,
    Path(identifier): Path<String>,
    Query(query): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Response {
    // Validate identifier format
    if invalid_identifier(&identifier) {
        return http_error(StatusCode::BAD_REQUEST, "无效的文件标识", "invalid_id").into_response();
    }

    let tg_service = match get_telegram_service(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let meta = database::get_file_by_id(&state.db_pool, &identifier);
    match meta {
        Ok(Some(f)) => {
            if let Some(ref hash) = f.share_password {
                let cookie_header = headers.get("cookie").and_then(|v| v.to_str().ok());
                let id_for_cookie = f.short_id.as_deref().unwrap_or(&f.file_id);
                if !crate::auth::share_unlocked(cookie_header, id_for_cookie, hash) {
                    return http_error(StatusCode::UNAUTHORIZED, "需要分享密码", "share_locked")
                        .into_response();
                }
            }
            let force_download = query
                .download
                .as_deref()
                .map_or(false, |v| v == "1" || v == "true");
            let is_head = false; // Will be handled by axum method routing
            serve_file(
                &state,
                &tg_service,
                &f.file_id,
                &f.filename,
                &headers,
                force_download,
                is_head,
            )
            .await
        }
        _ => http_error(StatusCode::NOT_FOUND, "文件未找到", "not_found").into_response(),
    }
}

async fn download_file_short_head(
    State(state): State<Arc<AppState>>,
    Path(identifier): Path<String>,
    Query(query): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Response {
    if invalid_identifier(&identifier) {
        return http_error(StatusCode::BAD_REQUEST, "无效的文件标识", "invalid_id").into_response();
    }
    let tg_service = match get_telegram_service(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let meta = database::get_file_by_id(&state.db_pool, &identifier);
    match meta {
        Ok(Some(f)) => {
            if let Some(ref hash) = f.share_password {
                let cookie_header = headers.get("cookie").and_then(|v| v.to_str().ok());
                let id_for_cookie = f.short_id.as_deref().unwrap_or(&f.file_id);
                if !crate::auth::share_unlocked(cookie_header, id_for_cookie, hash) {
                    return http_error(StatusCode::UNAUTHORIZED, "需要分享密码", "share_locked")
                        .into_response();
                }
            }
            let force_download = query
                .download
                .as_deref()
                .map_or(false, |v| v == "1" || v == "true");
            serve_file(
                &state,
                &tg_service,
                &f.file_id,
                &f.filename,
                &headers,
                force_download,
                true,
            )
            .await
        }
        _ => http_error(StatusCode::NOT_FOUND, "文件未找到", "not_found").into_response(),
    }
}

async fn download_file_legacy(
    State(state): State<Arc<AppState>>,
    Path((file_id, filename)): Path<(String, String)>,
    Query(query): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Response {
    if invalid_identifier(&file_id) {
        return http_error(StatusCode::BAD_REQUEST, "无效的文件标识", "invalid_id").into_response();
    }
    let tg_service = match get_telegram_service(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let force_download = query
        .download
        .as_deref()
        .map_or(false, |v| v == "1" || v == "true");
    serve_file(
        &state,
        &tg_service,
        &file_id,
        &filename,
        &headers,
        force_download,
        false,
    )
    .await
}

async fn download_file_legacy_head(
    State(state): State<Arc<AppState>>,
    Path((file_id, filename)): Path<(String, String)>,
    Query(query): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Response {
    if invalid_identifier(&file_id) {
        return http_error(StatusCode::BAD_REQUEST, "无效的文件标识", "invalid_id").into_response();
    }
    let tg_service = match get_telegram_service(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let force_download = query
        .download
        .as_deref()
        .map_or(false, |v| v == "1" || v == "true");
    serve_file(
        &state,
        &tg_service,
        &file_id,
        &filename,
        &headers,
        force_download,
        true,
    )
    .await
}

async fn get_files_list(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let files = database::get_all_files(&state.db_pool).unwrap_or_default();
    // 只暴露“是否有分享密码”这个布尔，绝不把 share_password 哈希发给前端。
    let out: Vec<serde_json::Value> = files
        .iter()
        .map(|f| {
            serde_json::json!({
                "filename": f.filename,
                "file_id": f.file_id,
                "filesize": f.filesize,
                "upload_date": f.upload_date,
                "short_id": f.short_id,
                "has_password": f.share_password.is_some(),
            })
        })
        .collect();
    Json(out)
}

async fn delete_file(
    State(state): State<Arc<AppState>>,
    Path(file_id): Path<String>,
) -> impl IntoResponse {
    let tg_service = match get_telegram_service(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    tracing::info!("正在删除文件: {}", file_id);

    let result = tg_service.delete_file_with_chunks(&file_id).await;

    if result.main_message_deleted {
        let db_deleted = database::delete_file_metadata(&state.db_pool, &file_id).unwrap_or(false);
        let db_status = if db_deleted {
            "deleted"
        } else {
            "not_found_in_db"
        };

        if result.failed_chunks.is_empty() {
            return Json(serde_json::json!({
                "status": "ok",
                "message": format!("文件 {} 已删除。", file_id),
                "details": {
                    "db": db_status,
                    "tg": result,
                }
            }))
            .into_response();
        } else {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "status": "error",
                    "code": "partial_failure",
                    "message": "部分分块删除失败",
                    "details": result,
                })),
            )
                .into_response();
        }
    }

    // TG deletion failed, try force-delete from DB
    let force_deleted = database::delete_file_metadata(&state.db_pool, &file_id).unwrap_or(false);
    if force_deleted {
        return Json(serde_json::json!({
            "status": "ok",
            "message": format!("文件 {} 已从数据库删除（Telegram 删除失败）。", file_id),
            "details": result,
        }))
        .into_response();
    }

    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "status": "error",
            "code": "delete_failed",
            "message": "删除失败",
            "details": result,
        })),
    )
        .into_response()
}

async fn batch_delete_files(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<BatchDeleteRequest>,
) -> impl IntoResponse {
    // Cap the number of IDs per request so callers cannot abuse batch delete
    // to issue an unbounded sequence of Telegram API requests.
    if payload.file_ids.len() > crate::constants::BATCH_DELETE_MAX {
        return http_error(
            StatusCode::BAD_REQUEST,
            "批量删除数量超过上限",
            "too_many_items",
        )
        .into_response();
    }

    let tg_service = match get_telegram_service(&state) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let mut deleted = Vec::new();
    let mut failed = Vec::new();

    for fid in &payload.file_ids {
        let result = tg_service.delete_file_with_chunks(fid).await;
        if result.main_message_deleted {
            database::delete_file_metadata(&state.db_pool, fid).ok();
            deleted.push(fid.clone());
        } else {
            // Try force delete from DB
            if database::delete_file_metadata(&state.db_pool, fid).unwrap_or(false) {
                deleted.push(fid.clone());
            } else {
                failed.push(fid.clone());
            }
        }
    }

    Json(serde_json::json!({
        "status": "completed",
        "deleted": deleted,
        "failed": failed,
    }))
    .into_response()
}

/// 设置 / 清除某个文件的分享密码（管理员操作，受 session 保护）。空密码表示清除。
async fn set_share_password(
    State(state): State<Arc<AppState>>,
    Path(file_id): Path<String>,
    Json(payload): Json<SharePasswordRequest>,
) -> Response {
    let pw = payload.password.trim().to_string();
    let has_pw = !pw.is_empty();

    let result = if !has_pw {
        database::set_share_password(&state.db_pool, &file_id, None)
    } else {
        // argon2 哈希是 CPU 密集型，放到阻塞线程池。
        let hashed = match tokio::task::spawn_blocking(move || crate::auth::hash_password(&pw)).await
        {
            Ok(Ok(h)) => h,
            _ => {
                return http_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "密码哈希失败",
                    "hash_error",
                )
                .into_response()
            }
        };
        database::set_share_password(&state.db_pool, &file_id, Some(&hashed))
    };

    match result {
        Ok(true) => Json(serde_json::json!({
            "status": "ok",
            "has_password": has_pw,
        }))
        .into_response(),
        Ok(false) => http_error(StatusCode::NOT_FOUND, "文件未找到", "not_found").into_response(),
        Err(e) => {
            tracing::error!("设置分享密码失败: {}", e);
            http_error(StatusCode::INTERNAL_SERVER_ERROR, "操作失败", "db_error").into_response()
        }
    }
}

async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/health", get(health_check))
        .route("/api/files", get(get_files_list))
        .route("/api/files/:file_id", delete(delete_file))
        .route("/api/batch_delete", post(batch_delete_files))
        .route("/api/files/:file_id/share-password", post(set_share_password))
        .route(
            "/d/:file_id/:filename",
            get(download_file_legacy).head(download_file_legacy_head),
        )
        .route(
            "/d/:identifier",
            get(download_file_short).head(download_file_short_head),
        )
}

#[cfg(test)]
mod tests {
    use super::chunk_download_failed_response;
    use axum::http::StatusCode;

    #[test]
    fn manifest_chunk_failure_returns_bad_gateway() {
        let response = chunk_download_failed_response("chunk-1");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
}
