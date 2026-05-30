use std::sync::{Arc, OnceLock};

use axum::extract::{Multipart, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use bytes::BytesMut;
use tokio::sync::Semaphore;

use crate::auth::{self, COOKIE_NAME};
use crate::config;
use crate::constants;
use crate::database;
use crate::error::http_error;
use crate::state::AppState;
use crate::telegram::service::TelegramService;

#[derive(Debug, Default)]
struct UploadAuthProgress {
    auth_verified: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum UploadFieldError {
    FileBeforeAuth,
}

fn advance_upload_auth_state(
    mut state: UploadAuthProgress,
    prechecked_auth: bool,
    auth_optional: bool,
    field_name: &str,
    _field_value: Option<&str>,
) -> Result<UploadAuthProgress, UploadFieldError> {
    if prechecked_auth || auth_optional {
        state.auth_verified = true;
        return Ok(state);
    }

    if field_name == "key" {
        state.auth_verified = true;
        return Ok(state);
    }

    if field_name == "file" && !state.auth_verified {
        return Err(UploadFieldError::FileBeforeAuth);
    }

    Ok(state)
}

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
    // UTF-8-safe byte-length cap: `clean[..255]` would panic if byte 255
    // falls inside a multibyte character (e.g. a Chinese filename).
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
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, impl IntoResponse> {
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

    // Pre-check auth with header-only info (before consuming body)
    let has_referer = headers.get("referer").is_some();
    let cookie_value = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| {
                let c = c.trim();
                c.strip_prefix(&format!("{}=", COOKIE_NAME))
                    .map(|v| v.to_string())
            })
        });

    let picgo_key = app_settings.get("PICGO_API_KEY").and_then(|v| v.as_deref());
    let pass_word = app_settings.get("PASS_WORD").and_then(|v| v.as_deref());
    // Upload auth used to derive a sha256 of the password and feed that as
    // the expected cookie value. That comparison was always a no-op because
    // session cookies are random tokens (see `auth::generate_session_token`)
    // that are independent of the password hash. The auth middleware already
    // validates the session cookie against the stored SESSION_TOKEN before
    // the request ever reaches this handler, so we just forward the raw
    // password presence to `ensure_upload_auth` for the referer / picgo-key
    // branches. The cookie branch inside `ensure_upload_auth` is reachable
    // only for header-level requests the middleware already allowed.
    let pass_word_hash_ref = pass_word;

    let header_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let auth_optional = picgo_key.map_or(true, |k| k.is_empty())
        && pass_word_hash_ref.map_or(true, |p| p.is_empty());
    // Pre-check auth using only HEADER-available credentials: the session
    // cookie (browser login) and/or x-api-key (PicGo / API clients). These
    // are the only credentials that exist before we consume the multipart
    // body. Referer is client-controlled and auth.rs ignores it.
    //
    // The browser session cookie is the random SESSION_TOKEN from
    // app_settings, NOT the password. We verify it directly against the
    // stored token so a logged-in browser can upload without submitting a
    // form `key` field. `auth_middleware` uses the same comparison.
    let session_token_owned = app_settings
        .get("SESSION_TOKEN")
        .and_then(|v| v.clone());
    let cookie_valid = match (cookie_value.as_deref(), session_token_owned.as_deref()) {
        (Some(c), Some(t)) if !c.is_empty() && !t.is_empty() => {
            auth::secure_compare(c, t)
        }
        _ => false,
    };
    // ensure_upload_auth handles the x-api-key / PicGo path. We pass None
    // for the cookie because cookie validity is handled via `cookie_valid`
    // above — that function still compares cookie to the password hash,
    // which does not match the random session token used in v2.x.
    let prechecked_auth = cookie_valid
        || auth::ensure_upload_auth(
            has_referer,
            None,
            picgo_key,
            pass_word_hash_ref,
            header_key.as_deref(),
        )
        .is_ok();

    // Parse multipart body - stream file chunks to Telegram
    let mut form_key: Option<String> = None;
    let mut upload_result: Option<Result<String, String>> = None;
    let mut auth_progress = UploadAuthProgress {
        auth_verified: prechecked_auth || auth_optional,
    };

    let tg_service = TelegramService::new(
        bot_token.to_string(),
        channel_name.to_string(),
        state.http_client.clone(),
    );

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "key" {
            let key_text = field.text().await.ok();
            if !auth_progress.auth_verified {
                if let Err((_, msg, code)) = auth::ensure_upload_auth(
                    has_referer,
                    None,
                    picgo_key,
                    pass_word_hash_ref,
                    key_text.as_deref(),
                ) {
                    return Err(http_error(axum::http::StatusCode::UNAUTHORIZED, msg, code));
                }
            }
            auth_progress = advance_upload_auth_state(
                auth_progress,
                prechecked_auth,
                auth_optional,
                &name,
                key_text.as_deref(),
            )
            .map_err(|_| {
                http_error(
                    axum::http::StatusCode::UNAUTHORIZED,
                    "upload auth required before file field",
                    "file_before_auth",
                )
            })?;
            form_key = key_text;
        } else if name == "file" {
            auth_progress = advance_upload_auth_state(
                auth_progress,
                prechecked_auth,
                auth_optional,
                &name,
                None,
            )
            .map_err(|_| {
                http_error(
                    axum::http::StatusCode::UNAUTHORIZED,
                    "upload auth required before file field",
                    "file_before_auth",
                )
            })?;
            let raw_filename = field.file_name().unwrap_or("upload").to_string();
            let filename = sanitize_filename(&raw_filename);

            // Stream the file in chunks to Telegram
            upload_result = Some(
                stream_upload_to_telegram(&tg_service, field, &filename, &state.db_pool).await,
            );
        }
    }

    // Final auth check with form-level `key`. Only needed when header-level
    // credentials (cookie / x-api-key) did not already satisfy auth — e.g.
    // PicGo clients that authenticate by submitting PICGO_API_KEY in the
    // multipart body instead of as a header.
    if !prechecked_auth {
        let final_key = form_key.as_deref();
        if let Err((_, msg, code)) = auth::ensure_upload_auth(
            has_referer,
            None,
            picgo_key,
            pass_word_hash_ref,
            final_key,
        ) {
            return Err(http_error(axum::http::StatusCode::UNAUTHORIZED, msg, code));
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

    let download_path = format!("/d/{}", short_id);
    Ok(Json(serde_json::json!({
        "file_id": short_id,
        "short_id": short_id,
        "download_path": download_path,
        "path": download_path,
        "url": download_path,
    })))
}

/// Resolve the max number of chunks uploaded to Telegram concurrently.
/// Overridable via the `UPLOAD_CONCURRENCY` env var (clamped to 1..=16): higher
/// values trade memory (≈ value × 19.5MB) for throughput. `1` keeps memory low
/// while still overlapping reads with a single in-flight upload (pipeline mode).
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

/// Stream a multipart upload to Telegram with **bounded-concurrency** chunking.
///
/// The field is read sequentially (the multipart body is a single stream), but
/// each completed `TELEGRAM_CHUNK_SIZE` chunk is dispatched to a concurrent
/// upload task. At most `upload_concurrency()` chunks upload at once, so peak
/// memory is roughly `(concurrency + 1) × ~19.5MB` rather than the whole file.
/// Reading the next chunk overlaps with uploading in-flight ones — the main win
/// over the previous fully-serial implementation, which neither overlapped
/// read-with-upload nor uploaded chunks in parallel.
///
/// The first chunk is uploaded synchronously so its `message_id` can anchor the
/// reply-thread for the remaining chunks and the manifest (purely cosmetic in
/// the channel; reconstruction relies on the manifest, not the reply chain).
/// Concurrent completion is out of order, so chunks carry their index and are
/// re-sorted before the manifest is written. On any failure the chunks that did
/// upload are best-effort deleted so the channel is not left with orphans.
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
    // Chunks finished synchronously (currently only the anchor at index 0).
    let mut results: Vec<(usize, String)> = Vec::new();
    // Chunks uploading concurrently; each yields (index, "message_id:file_id").
    let mut tasks: Vec<tokio::task::JoinHandle<Result<(usize, String), String>>> = Vec::new();
    let mut chunk_index: usize = 0;
    let mut anchor_id: Option<i64> = None;

    while let Ok(Some(bytes)) = field.chunk().await {
        buffer.extend_from_slice(&bytes);
        total_size += bytes.len();

        while buffer.len() >= chunk_size {
            let data = buffer.split_to(chunk_size).freeze().to_vec();
            let idx = chunk_index;
            chunk_index += 1;
            let chunk_name = format!("{}.part{}", filename, idx + 1);

            if idx == 0 {
                // Anchor: upload synchronously to obtain the reply-thread root.
                let message = tg_service
                    .send_document_with_retry(data, &chunk_name, None)
                    .await?;
                anchor_id = Some(message.message_id);
                let doc = message.document.ok_or("No document in chunk response")?;
                results.push((idx, format!("{}:{}", message.message_id, doc.file_id)));
            } else {
                // Acquire the permit BEFORE spawning so no more than
                // `concurrency` chunks are buffered/in-flight at once. This also
                // back-pressures the read loop, bounding memory.
                let permit = sem
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| e.to_string())?;
                let tg = tg_service.clone();
                let reply = anchor_id;
                tasks.push(tokio::spawn(async move {
                    let _permit = permit;
                    let message = tg.send_document_with_retry(data, &chunk_name, reply).await?;
                    let doc = message
                        .document
                        .ok_or_else(|| "No document in chunk response".to_string())?;
                    Ok((idx, format!("{}:{}", message.message_id, doc.file_id)))
                }));
            }
        }
    }

    // No full chunk was produced → small (single-shot) file.
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

    // Dispatch the trailing partial chunk (if any) as the final chunk.
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
        let reply = anchor_id;
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let message = tg.send_document_with_retry(data, &chunk_name, reply).await?;
            let doc = message
                .document
                .ok_or_else(|| "No document in chunk response".to_string())?;
            Ok((idx, format!("{}:{}", message.message_id, doc.file_id)))
        }));
    }

    // Join all concurrent chunk uploads, capturing the first error if any.
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

    // Concurrent completion is out of order; restore chunk order for the manifest.
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
        .send_document_with_retry(manifest.into_bytes(), &manifest_name, anchor_id)
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

/// Restore ascending chunk order (concurrent uploads finish out of order) and
/// drop the index, yielding the `"message_id:file_id"` composites for the manifest.
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

/// Best-effort deletion of already-uploaded chunks after a failed multi-chunk
/// upload, so a partial upload does not leave orphaned messages in the channel.
/// Each entry is a `"message_id:file_id"` composite; only the message_id is used.
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
    use super::{advance_upload_auth_state, UploadAuthProgress, UploadFieldError};
    use crate::config::Settings;
    use crate::database;
    use crate::state::AppState;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use axum::Router;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tower::util::ServiceExt;

    #[test]
    fn ordered_chunk_ids_sort_by_index_regardless_of_completion_order() {
        // Concurrent uploads finish out of order; the manifest must still list
        // chunks in ascending index order so the download path concatenates
        // them correctly.
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

    fn test_state() -> Arc<AppState> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = std::env::temp_dir()
            .join(format!("tgstate-upload-test-{}", unique))
            .to_string_lossy()
            .to_string();

        let settings = Settings {
            bot_token: Some("123456:test-token".into()),
            channel_name: Some("@test_channel".into()),
            pass_word: Some("secret".into()),
            picgo_api_key: None,
            base_url: "http://127.0.0.1:8000".into(),
            _mode: "p".into(),
            _file_route: "/d/".into(),
            data_dir: data_dir.clone(),
        };

        let db_pool = database::init_db(&data_dir);
        let tera = tera::Tera::default();
        let http_client = reqwest::Client::new();
        let app_settings = crate::config::get_app_settings(&settings, &db_pool);
        Arc::new(AppState::new(
            settings,
            tera,
            http_client,
            db_pool,
            app_settings,
            true,
        ))
    }

    fn multipart_request_with_file_before_key() -> Request<Body> {
        let boundary = "X-BOUNDARY";
        let body = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--{b}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nsecret\r\n--{b}--\r\n",
            b = boundary
        );

        Request::builder()
            .method("POST")
            .uri("/api/upload")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={}", boundary),
            )
            .body(Body::from(body))
            .unwrap()
    }

    #[test]
    fn upload_requires_key_before_file_for_api_requests() {
        let state = UploadAuthProgress::default();
        let result = advance_upload_auth_state(state, false, false, "file", None);
        assert!(matches!(result, Err(UploadFieldError::FileBeforeAuth)));
    }

    #[test]
    fn upload_accepts_key_before_file_for_api_requests() {
        let state = UploadAuthProgress::default();
        let state = advance_upload_auth_state(state, false, false, "key", Some("secret")).unwrap();
        let state = advance_upload_auth_state(state, false, false, "file", None).unwrap();
        assert!(state.auth_verified);
    }

    #[tokio::test]
    async fn upload_route_rejects_file_field_before_auth() {
        let state = test_state();
        let app = Router::new()
            .merge(super::router())
            .with_state(state.clone());
        let response = app
            .oneshot(multipart_request_with_file_before_key())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("file_before_auth"), "unexpected body: {}", text);

        let files = database::get_all_files(&state.db_pool).unwrap();
        assert!(files.is_empty(), "unexpected files persisted: {:?}", files);
    }
}
