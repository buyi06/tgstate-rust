use std::sync::Arc;

use axum::extract::{Multipart, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};

use crate::auth::{self, COOKIE_NAME};
use crate::config;
use crate::error::http_error;
use crate::state::AppState;
use crate::telegram::service::TelegramService;

async fn upload_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, impl IntoResponse> {
    let app_settings = config::get_app_settings(&state.settings);

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

    let picgo_key = app_settings
        .get("PICGO_API_KEY")
        .and_then(|v| v.as_deref());
    let pass_word = app_settings.get("PASS_WORD").and_then(|v| v.as_deref());

    // Get submitted key from header
    let header_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // If header key is available, do auth check now (before reading body)
    if header_key.is_some() || has_referer {
        if let Err((_, msg, code)) = auth::ensure_upload_auth(
            has_referer,
            cookie_value.as_deref(),
            picgo_key,
            pass_word,
            header_key.as_deref(),
        ) {
            return Err(http_error(
                axum::http::StatusCode::UNAUTHORIZED,
                msg,
                code,
            ));
        }
    }

    // Parse multipart body
    let mut form_key: Option<String> = None;
    let mut file_data: Option<(String, Vec<u8>)> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "key" {
            form_key = field.text().await.ok();
        } else if name == "file" {
            let filename = field.file_name().unwrap_or("upload").to_string();
            match field.bytes().await {
                Ok(bytes) => file_data = Some((filename, bytes.to_vec())),
                Err(e) => {
                    return Err(http_error(
                        axum::http::StatusCode::BAD_REQUEST,
                        &format!("读取文件失败: {}", e),
                        "read_error",
                    ));
                }
            }
        }
    }

    // Final auth check with form key (for API-only requests without referer/header key)
    if header_key.is_none() && !has_referer {
        let final_key = form_key.as_deref();
        if let Err((_, msg, code)) =
            auth::ensure_upload_auth(has_referer, cookie_value.as_deref(), picgo_key, pass_word, final_key)
        {
            return Err(http_error(
                axum::http::StatusCode::UNAUTHORIZED,
                msg,
                code,
            ));
        }
    }

    let (filename, data) = file_data.ok_or_else(|| {
        http_error(
            axum::http::StatusCode::BAD_REQUEST,
            "未提供文件",
            "no_file",
        )
    })?;

    tracing::info!("开始上传文件: {}", filename);

    // Write to temp file
    let temp_dir = tempfile::tempdir().map_err(|e| {
        http_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            &format!("创建临时目录失败: {}", e),
            "temp_error",
        )
    })?;
    let temp_path = temp_dir.path().join(&filename);
    std::fs::write(&temp_path, &data).map_err(|e| {
        http_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            &format!("写入临时文件失败: {}", e),
            "temp_error",
        )
    })?;

    // Upload to Telegram
    let tg_service = TelegramService::new(
        bot_token.to_string(),
        channel_name.to_string(),
        state.http_client.clone(),
    );

    let db_path = state.db_path();
    let short_id = tg_service
        .upload_file(temp_path.to_str().unwrap(), &filename, &db_path)
        .await
        .map_err(|e| {
            tracing::error!("文件上传失败: {} - {}", filename, e);
            http_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "文件上传失败。",
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

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/upload", post(upload_file))
}
