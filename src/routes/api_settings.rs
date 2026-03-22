use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::{sha256_hex, COOKIE_NAME};
use crate::config;
use crate::database;
use crate::error::http_error;
use crate::state::{self, AppState};

#[derive(Deserialize)]
pub struct PasswordRequest {
    password: String,
}

#[derive(Deserialize)]
pub struct AppConfigRequest {
    #[serde(rename = "BOT_TOKEN")]
    bot_token: Option<String>,
    #[serde(rename = "CHANNEL_NAME")]
    channel_name: Option<String>,
    #[serde(rename = "PASS_WORD")]
    pass_word: Option<String>,
    #[serde(rename = "BASE_URL")]
    base_url: Option<String>,
    #[serde(rename = "PICGO_API_KEY")]
    picgo_api_key: Option<String>,
}

#[derive(Deserialize)]
pub struct VerifyRequest {
    #[serde(rename = "BOT_TOKEN")]
    bot_token: Option<String>,
    #[serde(rename = "CHANNEL_NAME")]
    channel_name: Option<String>,
}

fn validate_config(cfg: &std::collections::HashMap<String, Option<String>>) -> Result<(), (axum::http::StatusCode, &'static str, &'static str)> {
    if let Some(Some(token)) = cfg.get("BOT_TOKEN") {
        let t = token.trim();
        if !t.is_empty() && (!t.contains(':') || t.len() < 20) {
            return Err((axum::http::StatusCode::BAD_REQUEST, "BOT_TOKEN 格式不正确", "invalid_bot_token"));
        }
    }
    if let Some(Some(channel)) = cfg.get("CHANNEL_NAME") {
        let c = channel.trim();
        if !c.is_empty() && !c.starts_with('@') && !c.starts_with("-100") {
            return Err((axum::http::StatusCode::BAD_REQUEST, "CHANNEL_NAME 格式不正确（@username 或 -100...）", "invalid_channel"));
        }
    }
    if let Some(Some(url)) = cfg.get("BASE_URL") {
        let u = url.trim();
        if !u.is_empty() && !u.starts_with("http://") && !u.starts_with("https://") {
            return Err((axum::http::StatusCode::BAD_REQUEST, "BASE_URL 必须以 http:// 或 https:// 开头", "invalid_base_url"));
        }
    }
    Ok(())
}

fn merge_config(
    existing: &std::collections::HashMap<String, Option<String>>,
    incoming: &AppConfigRequest,
) -> std::collections::HashMap<String, Option<String>> {
    let mut result = existing.clone();

    // Allow empty strings to clear fields (match Python behavior)
    if let Some(ref v) = incoming.bot_token {
        let v = v.trim().to_string();
        result.insert("BOT_TOKEN".into(), if v.is_empty() { None } else { Some(v) });
    }
    if let Some(ref v) = incoming.channel_name {
        let v = v.trim().to_string();
        result.insert("CHANNEL_NAME".into(), if v.is_empty() { None } else { Some(v) });
    }
    if let Some(ref v) = incoming.pass_word {
        let v = v.trim().to_string();
        result.insert("PASS_WORD".into(), if v.is_empty() { None } else { Some(v) });
    }
    if let Some(ref v) = incoming.base_url {
        let v = v.trim().to_string();
        result.insert("BASE_URL".into(), if v.is_empty() { None } else { Some(v) });
    }
    if let Some(ref v) = incoming.picgo_api_key {
        let v = v.trim().to_string();
        result.insert("PICGO_API_KEY".into(), if v.is_empty() { None } else { Some(v) });
    }
    result
}

async fn get_app_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let settings = config::get_app_settings(&state.settings);
    let bot = state.bot_state.lock().await;

    Json(serde_json::json!({
        "status": "ok",
        "cfg": {
            "BOT_TOKEN_SET": settings.get("BOT_TOKEN").and_then(|v| v.as_deref()).map_or(false, |v| !v.is_empty()),
            "CHANNEL_NAME": settings.get("CHANNEL_NAME").and_then(|v| v.as_deref()).unwrap_or(""),
            "PASS_WORD_SET": settings.get("PASS_WORD").and_then(|v| v.as_deref()).map_or(false, |v| !v.is_empty()),
            "BASE_URL": settings.get("BASE_URL").and_then(|v| v.as_deref()).unwrap_or(""),
            "PICGO_API_KEY_SET": settings.get("PICGO_API_KEY").and_then(|v| v.as_deref()).map_or(false, |v| !v.is_empty()),
        },
        "bot": {
            "ready": bot.bot_ready,
            "running": bot.bot_running,
            "error": bot.bot_error,
        }
    }))
}

async fn save_config_only(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AppConfigRequest>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    let db_path = state.db_path();
    let existing = database::get_app_settings_from_db(&db_path).unwrap_or_default();
    let merged = merge_config(&existing, &payload);

    if let Err((status, msg, code)) = validate_config(&merged) {
        return Err(http_error(status, msg, code));
    }

    database::save_app_settings_to_db(&db_path, &merged).map_err(|e| {
        http_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            &format!("保存失败: {}", e),
            "save_error",
        )
    })?;

    tracing::info!("配置已保存（未应用）");
    Ok(Json(serde_json::json!({
        "status": "ok",
        "message": "已保存（未应用）"
    })))
}

async fn save_and_apply(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AppConfigRequest>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    let db_path = state.db_path();
    let existing = database::get_app_settings_from_db(&db_path).unwrap_or_default();
    let merged = merge_config(&existing, &payload);

    if let Err((status, msg, code)) = validate_config(&merged) {
        return Err(http_error(status, msg, code));
    }

    database::save_app_settings_to_db(&db_path, &merged).map_err(|e| {
        http_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            &format!("保存失败: {}", e),
            "save_error",
        )
    })?;

    let _ = state::apply_runtime_settings(state.clone(), true).await;

    let bot = state.bot_state.lock().await;

    // Handle password cookie
    let pwd = merged.get("PASS_WORD").and_then(|v| v.as_deref()).unwrap_or("");
    let cookie = if !pwd.is_empty() {
        let hash = sha256_hex(pwd);
        format!("{}={}; HttpOnly; SameSite=Lax; Path=/", COOKIE_NAME, hash)
    } else {
        format!("{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0", COOKIE_NAME)
    };

    Ok((
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({
            "status": "ok",
            "message": "已保存并应用",
            "bot": {
                "ready": bot.bot_ready,
                "running": bot.bot_running,
            }
        })),
    ))
}

async fn reset_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db_path = state.db_path();
    database::reset_app_settings_in_db(&db_path).ok();
    let _ = state::apply_runtime_settings(state.clone(), true).await;
    tracing::warn!("配置已重置");

    let cookie = format!(
        "{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
        COOKIE_NAME
    );

    (
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({
            "status": "ok",
            "message": "配置已重置"
        })),
    )
}

async fn set_password(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<PasswordRequest>,
) -> Result<Json<serde_json::Value>, crate::error::AppError> {
    let db_path = state.db_path();
    let password = payload.password.trim().to_string();

    let mut current = database::get_app_settings_from_db(&db_path).unwrap_or_default();
    current.insert("PASS_WORD".into(), Some(password));

    database::save_app_settings_to_db(&db_path, &current).map_err(|_| {
        crate::error::AppError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "无法写入密码。",
            "write_password_failed",
        )
    })?;

    let _ = state::apply_runtime_settings(state.clone(), false).await;
    tracing::info!("密码已成功设置");

    Ok(Json(serde_json::json!({
        "status": "ok",
        "message": "密码已成功设置。"
    })))
}

async fn verify_bot(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VerifyRequest>,
) -> impl IntoResponse {
    let app_settings = config::get_app_settings(&state.settings);
    let token = payload
        .bot_token
        .or_else(|| app_settings.get("BOT_TOKEN").and_then(|v| v.clone()))
        .unwrap_or_default();

    if token.is_empty() {
        return Json(serde_json::json!({
            "status": "ok",
            "ok": false,
            "available": false,
            "message": "未提供 BOT_TOKEN"
        }));
    }

    let url = format!("https://api.telegram.org/bot{}/getMe", token);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();

    match client.get(&url).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(data) => {
                if data["ok"].as_bool() == Some(true) {
                    let username = data["result"]["username"]
                        .as_str()
                        .unwrap_or("unknown");
                    Json(serde_json::json!({
                        "status": "ok",
                        "ok": true,
                        "available": true,
                        "result": { "username": username }
                    }))
                } else {
                    Json(serde_json::json!({
                        "status": "ok",
                        "ok": false,
                        "available": false,
                        "message": data["description"].as_str().unwrap_or("Unknown error")
                    }))
                }
            }
            Err(e) => Json(serde_json::json!({
                "status": "ok",
                "ok": false,
                "available": false,
                "message": format!("解析响应失败: {}", e)
            })),
        },
        Err(e) => Json(serde_json::json!({
            "status": "ok",
            "ok": false,
            "available": false,
            "message": format!("连接失败: {}", e)
        })),
    }
}

async fn verify_channel(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VerifyRequest>,
) -> impl IntoResponse {
    let app_settings = config::get_app_settings(&state.settings);
    let token = payload
        .bot_token
        .or_else(|| app_settings.get("BOT_TOKEN").and_then(|v| v.clone()))
        .unwrap_or_default();
    let channel = payload
        .channel_name
        .or_else(|| app_settings.get("CHANNEL_NAME").and_then(|v| v.clone()))
        .unwrap_or_default();

    if token.is_empty() || channel.is_empty() {
        return Json(serde_json::json!({
            "status": "ok",
            "available": false,
            "message": "缺少 BOT_TOKEN 或 CHANNEL_NAME"
        }));
    }

    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();

    match client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": channel,
            "text": "tgState channel check"
        }))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(data) => {
                if data["ok"].as_bool() == Some(true) {
                    // Try to delete test message
                    if let Some(msg_id) = data["result"]["message_id"].as_i64() {
                        let del_url =
                            format!("https://api.telegram.org/bot{}/deleteMessage", token);
                        let _ = client
                            .post(&del_url)
                            .json(&serde_json::json!({
                                "chat_id": channel,
                                "message_id": msg_id
                            }))
                            .send()
                            .await;
                    }
                    Json(serde_json::json!({
                        "status": "ok",
                        "available": true
                    }))
                } else {
                    Json(serde_json::json!({
                        "status": "ok",
                        "available": false,
                        "message": data["description"].as_str().unwrap_or("Unknown error")
                    }))
                }
            }
            Err(e) => Json(serde_json::json!({
                "status": "ok",
                "available": false,
                "message": format!("解析响应失败: {}", e)
            })),
        },
        Err(e) => Json(serde_json::json!({
            "status": "ok",
            "available": false,
            "message": format!("连接失败: {}", e)
        })),
    }
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/app-config", get(get_app_config))
        .route("/api/app-config/save", post(save_config_only))
        .route("/api/app-config/apply", post(save_and_apply))
        .route("/api/reset-config", post(reset_config))
        .route("/api/set-password", post(set_password))
        .route("/api/verify/bot", post(verify_bot))
        .route("/api/verify/channel", post(verify_channel))
}
