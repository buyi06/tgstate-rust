use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth;
use crate::config;
use crate::credentials;
use crate::database;
use crate::error::http_error;
use crate::state::{self, AppState};
use crate::totp;

#[derive(Deserialize)]
pub struct PasswordRequest {
    pub password: String,
    #[serde(default)]
    pub username: Option<String>,
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
) -> Result<
    std::collections::HashMap<String, Option<String>>,
    (axum::http::StatusCode, &'static str, &'static str),
> {
    let mut result = existing.clone();

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
        if v.is_empty() {
            result.insert("PASS_WORD".into(), None);
            result.insert("SESSION_TOKEN".into(), None);
        } else {
            // Hash password and compute a cryptographically random session token.
            // The token is independent of the password, so sessions cannot be
            // forged from knowledge of the plaintext or hash. If hashing fails we
            // REJECT the update rather than falling back to plaintext storage.
            match auth::hash_password(&v) {
                Ok(hashed) => {
                    let session_token = auth::generate_session_token();
                    result.insert("PASS_WORD".into(), Some(hashed));
                    result.insert("SESSION_TOKEN".into(), Some(session_token));
                }
                Err(e) => {
                    tracing::error!("密码哈希失败: {}", e);
                    return Err((
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "密码哈希失败",
                        "hash_error",
                    ));
                }
            }
        }
    }
    if let Some(ref v) = incoming.base_url {
        let v = v.trim().to_string();
        result.insert("BASE_URL".into(), if v.is_empty() { None } else { Some(v) });
    }
    Ok(result)
}

fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map_or(false, |v| v == "https")
}

async fn get_app_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let settings = config::get_app_settings(&state.settings, &state.db_pool);
    let bot = state.bot_state.lock().await;

    Json(serde_json::json!({
        "status": "ok",
        "cfg": {
            "BOT_TOKEN_SET": settings.get("BOT_TOKEN").and_then(|v| v.as_deref()).map_or(false, |v| !v.is_empty()),
            "CHANNEL_NAME": settings.get("CHANNEL_NAME").and_then(|v| v.as_deref()).unwrap_or(""),
            "PASS_WORD_SET": settings.get("PASS_WORD").and_then(|v| v.as_deref()).map_or(false, |v| !v.is_empty()),
            "BASE_URL": settings.get("BASE_URL").and_then(|v| v.as_deref()).unwrap_or(""),
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
    let existing = database::get_app_settings_from_db(&state.db_pool).unwrap_or_default();
    // merge_config 在设置新密码时内部会做 argon2 哈希（CPU 密集），放到阻塞线程池执行。
    let merged = tokio::task::spawn_blocking(move || merge_config(&existing, &payload))
        .await
        .map_err(|_| {
            http_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "内部错误",
                "internal_error",
            )
        })?
        .map_err(|(status, msg, code)| http_error(status, msg, code))?;

    if let Err((status, msg, code)) = validate_config(&merged) {
        return Err(http_error(status, msg, code));
    }

    database::save_app_settings_to_db(&state.db_pool, &merged).map_err(|e| {
        tracing::error!("保存配置失败: {}", e);
        http_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "保存配置失败",
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
    headers: HeaderMap,
    Json(payload): Json<AppConfigRequest>,
) -> Result<impl IntoResponse, impl IntoResponse> {
    let existing = database::get_app_settings_from_db(&state.db_pool).unwrap_or_default();
    // merge_config 在设置新密码时内部会做 argon2 哈希（CPU 密集），放到阻塞线程池执行。
    let merged = tokio::task::spawn_blocking(move || merge_config(&existing, &payload))
        .await
        .map_err(|_| {
            http_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "内部错误",
                "internal_error",
            )
        })?
        .map_err(|(status, msg, code)| http_error(status, msg, code))?;

    if let Err((status, msg, code)) = validate_config(&merged) {
        return Err(http_error(status, msg, code));
    }

    database::save_app_settings_to_db(&state.db_pool, &merged).map_err(|e| {
        tracing::error!("保存配置失败: {}", e);
        http_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "保存配置失败",
            "save_error",
        )
    })?;

    let _ = state::apply_runtime_settings(state.clone(), true).await;

    let bot = state.bot_state.lock().await;

    // Handle password cookie using the server-side random session token.
    // We honor x-forwarded-proto so cookies get the Secure flag when a
    // trusted reverse proxy terminates TLS; the COOKIE_SECURE env var
    // (handled inside build_cookie) can force Secure regardless.
    let session_token = merged
        .get("SESSION_TOKEN")
        .and_then(|v| v.as_deref())
        .unwrap_or("");
    let pwd = merged
        .get("PASS_WORD")
        .and_then(|v| v.as_deref())
        .unwrap_or("");
    let secure = is_https(&headers);
    let cookie = if !pwd.is_empty() && !session_token.is_empty() {
        auth::build_cookie(session_token, secure)
    } else {
        // No password set OR no session token — clear any stale cookie.
        auth::build_clear_cookie()
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
    database::reset_app_settings_in_db(&state.db_pool).ok();
    let _ = state::apply_runtime_settings(state.clone(), true).await;
    tracing::warn!("配置已重置");

    let cookie = crate::auth::build_clear_cookie();

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
    headers: HeaderMap,
    Json(payload): Json<PasswordRequest>,
) -> Result<impl IntoResponse, crate::error::AppError> {
    let db_pool = &state.db_pool;
    let password = payload.password.trim().to_string();
    let username = payload.username.clone().unwrap_or_default().trim().to_string();

    // Hash the password with argon2. Simultaneously seed the account store
    // (demand 2: username + password). The legacy single-token channel in
    // app_settings is kept in sync for compatibility with old binary/clients.
    let pw_for_hash = password.clone();
    let hashed = tokio::task::spawn_blocking(move || auth::hash_password(&pw_for_hash))
        .await
        .map_err(|e| {
            tracing::error!("密码哈希任务失败: {}", e);
            crate::error::AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "密码哈希失败",
                "hash_error",
            )
        })?
        .map_err(|e| {
            tracing::error!("密码哈希失败: {}", e);
            crate::error::AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "密码哈希失败",
                "hash_error",
            )
        })?;

    if username.is_empty() {
        return Err(crate::error::AppError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "必须同时设置用户名和密码",
            "username_required",
        ));
    }

    // Write the account (username + argon2 hash) — this is what login checks.
    credentials::save_userpass(db_pool, Some(&username), Some(&hashed)).map_err(|_| {
        crate::error::AppError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "无法写入账户。",
            "write_account_failed",
        )
    })?;

    // Immediately issue a persisted session so the setter stays logged in
    // even before re-login (and across restarts).
    let session_token = auth::generate_session_token();
    let cookie = auth::build_cookie(&session_token, is_https(&headers));
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    database::session_upsert(
        db_pool,
        &session_token,
        auth::session_ttl_i64(),
        ua,
    )
    .map_err(|_| {
        crate::error::AppError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "无法建立会话。",
            "session_failed",
        )
    })?;

    // Legacy channel stays alive but empty: old binaries/flows that read
    // PASS_WORD still behave; new login path never touches it.
    let mut current = database::get_app_settings_from_db(db_pool).unwrap_or_default();
    current.insert("PASS_WORD".into(), Some(hashed));
    current.insert("SESSION_TOKEN".into(), None);

    database::save_app_settings_to_db(db_pool, &current).map_err(|_| {
        crate::error::AppError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "无法写入密码。",
            "write_password_failed",
        )
    })?;

    let _ = state::apply_runtime_settings(state.clone(), false).await;
    tracing::info!("账户（用户名+密码）已成功设置");

    Ok((
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({
            "status": "ok",
            "message": "账户已成功设置。"
        })),
    ))
}

async fn verify_bot(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VerifyRequest>,
) -> impl IntoResponse {
    let app_settings = config::get_app_settings(&state.settings, &state.db_pool);
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
            Err(e) => {
                tracing::warn!("verify_bot parse error: {}", e);
                Json(serde_json::json!({
                    "status": "ok",
                    "ok": false,
                    "available": false,
                    "message": "解析响应失败"
                }))
            }
        },
        Err(e) => {
            tracing::warn!("verify_bot connect error: {}", e);
            Json(serde_json::json!({
                "status": "ok",
                "ok": false,
                "available": false,
                "message": "连接失败"
            }))
        },
    }
}

async fn verify_channel(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VerifyRequest>,
) -> impl IntoResponse {
    let app_settings = config::get_app_settings(&state.settings, &state.db_pool);
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
            Err(e) => {
                tracing::warn!("verify_channel parse error: {}", e);
                Json(serde_json::json!({
                    "status": "ok",
                    "available": false,
                    "message": "解析响应失败"
                }))
            }
        },
        Err(e) => {
            tracing::warn!("verify_channel connect error: {}", e);
            Json(serde_json::json!({
                "status": "ok",
                "available": false,
                "message": "连接失败"
            }))
        },
    }
}

// ==================== 2FA（TOTP）与 API 密钥管理端点 ====================
//
// 全部处于 session 保护之下（中间件只放行 /api/auth/* 和静态资源），
// 即：必须先登录浏览器会话才能开/关 2FA、签发/吊销 API 密钥。

/// 生成 2FA：返回 base32 密钥、otpauth URL、二维码 SVG 和恢复码明文。
/// 前端拿码去验证器 App 绑定，输入当前动态码 POST /api/2fa/enable 确认。
/// 密钥先以 pending 态存内存（不落库）——未确认启用前登录流程不受影响。
static PENDING_2FA: std::sync::OnceLock<std::sync::Mutex<Option<PendingTotp>>> =
    std::sync::OnceLock::new();

#[derive(Clone)]
struct PendingTotp {
    secret: String,
    recovery_plain: Vec<String>,
    recovery_hashes: String,
    issued: std::time::Instant,
}

async fn two_fa_setup(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let creds = credentials::get(&state.db_pool).unwrap_or_default();
    let username = creds.username.clone().unwrap_or_else(|| "admin".into());

    let secret = totp::generate_secret_base32();
    let recovery = totp::generate_recovery_codes(8);
    let recovery_hashes = recovery
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(",");
    let url = totp::otpauth_url(&secret, &username, "tgState");

    // 服务端渲染二维码 SVG（qrcode crate），无需外部 JS，CSP 不破。
    let svg = qrcode::QrCode::new(url.as_bytes())
        .map(|code| {
            code.render::<qrcode::color::Svg>()
                .min_dimensions(200, 200)
                .dark_color(qrcode::color::Svg("#0f172a"))
                .light_color(qrcode::color::Svg("#ffffff"))
                .build()
        })
        .unwrap_or_default();

    *PENDING_2FA
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap() = Some(PendingTotp {
        secret: secret.clone(),
        recovery_plain: recovery.clone(),
        recovery_hashes: recovery_hashes.clone(),
        issued: std::time::Instant::now(),
    });

    Json(serde_json::json!({
        "status": "ok",
        "secret": secret,
        "otpauth_url": url,
        "qr_svg": svg,
        "recovery_codes": recovery,
    }))
}

#[derive(Deserialize)]
pub struct TwoFaEnableRequest {
    pub code: String,
}

/// 确认启用 2FA：用户输入验证器当前的 6 位码，服务端校验后把 pending 密钥落库。
async fn two_fa_enable(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<TwoFaEnableRequest>,
) -> impl IntoResponse {
    let pending = PENDING_2FA
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap()
        .take();
    let Some(p) = pending else {
        return http_error(
            axum::http::StatusCode::BAD_REQUEST,
            "请先调用 /api/2fa/setup 生成密钥",
            "no_pending_setup",
        )
        .into_response();
    };
    // 票据 10 分钟有效。
    if p.issued.elapsed() > std::time::Duration::from_secs(600) {
        return http_error(
            axum::http::StatusCode::BAD_REQUEST,
            "设置已过期，请重新生成",
            "setup_expired",
        )
        .into_response();
    }

    let code = payload.code.trim().to_string();
    let secret = p.secret.clone();
    let ok = tokio::task::spawn_blocking(move || totp::verify_totp(&secret, &code, 1))
        .await
        .unwrap_or(false);
    if !ok {
        // 校验失败时把 pending 放回去，用户可重试（同一次 setup 内）。
        *PENDING_2FA
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap() = Some(p.clone());
        return http_error(
            axum::http::StatusCode::BAD_REQUEST,
            "动态码错误，请确认验证器时间同步",
            "bad_code",
        )
        .into_response();
    }

    // 落库启用。恢复码只在这一次（enable 响应）里给用户看明文。
    if let Err(e) = credentials::save_totp(&state.db_pool, Some(&p.secret), Some(&p.recovery_hashes)) {
        tracing::error!("启用 2FA 失败: {}", e);
        return http_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "启用失败",
            "enable_failed",
        )
        .into_response();
    }

    Json(serde_json::json!({
        "status": "ok",
        "message": "两步验证已启用",
        "recovery_codes": p.recovery_plain,
    }))
}

/// 关闭 2FA（需要当前有效的 6 位码；防止会话被劫持后直接关掉第二因素）。
#[derive(Deserialize)]
pub struct TwoFaDisableRequest {
    pub code: Option<String>,
}

async fn two_fa_disable(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<TwoFaDisableRequest>,
) -> impl IntoResponse {
    let creds = credentials::get(&state.db_pool).unwrap_or_default();
    if !creds.has_totp() {
        return http_error(
            axum::http::StatusCode::BAD_REQUEST,
            "两步验证未启用",
            "not_enabled",
        )
        .into_response();
    }
    // 已启用状态下要求提供当前动态码（或恢复码）才能关。
    let code = payload.code.clone().unwrap_or_default();
    let secret = creds.totp_secret.clone().unwrap_or_default();
    let matched = if !code.trim().is_empty() {
        // 先按 TOTP 试，再按恢复码试（命中即销毁一张）。
        let c = code.clone();
        tokio::task::spawn_blocking(move || totp::verify_totp(&secret, &c, 1))
            .await
            .unwrap_or(false)
    } else {
        false
    };
    if !matched {
        // 恢复码路径。
        let normalized = code.trim().to_ascii_lowercase();
        let mut remaining: Vec<String> = Vec::new();
        let mut hit = false;
        if let Some(list) = creds.totp_recovery_hashes.as_deref() {
            for h in list.split(',').filter(|h| !h.trim().is_empty()) {
                if !hit && auth::secure_compare(&normalized, h.trim()) {
                    hit = true;
                } else {
                    remaining.push(h.trim().to_string());
                }
            }
        }
        if !hit {
            return http_error(
                axum::http::StatusCode::UNAUTHORIZED,
                "动态码或恢复码错误",
                "bad_code",
            )
            .into_response();
        }
        // 消耗了一张恢复码（保持启用状态由下面 disable 覆盖，无所谓）。
        let _ = credentials::save_totp(&state.db_pool, Some(""), Some(&remaining.join(",")));
    }

    if let Err(e) = credentials::save_totp(&state.db_pool, None, None) {
        tracing::error!("关闭 2FA 失败: {}", e);
        return http_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "关闭失败",
            "disable_failed",
        )
        .into_response();
    }
    Json(serde_json::json!({ "status": "ok", "message": "两步验证已关闭" }))
}

/// 2FA 状态查询（设置页回显）。
async fn two_fa_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let creds = credentials::get(&state.db_pool).unwrap_or_default();
    // username_exists 在响应里带上，方便设置页判断是否已建账户。
    Json(serde_json::json!({
        "status": "ok",
        "enabled": creds.has_totp(),
        "username": creds.username.as_deref().unwrap_or(""),
    }))
}

/// 生成新的 API 密钥：明文只在本次响应返回，库中只落 sha256。
async fn api_key_generate(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let key = credentials::generate_api_key();
    let hash = credentials::sha256_hex(&key);
    if let Err(e) = credentials::save_api_key_hash(&state.db_pool, Some(&hash)) {
        tracing::error!("保存 API 密钥哈希失败: {}", e);
        return http_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "生成失败",
            "generate_failed",
        )
        .into_response();
    }
    tracing::info!("API 密钥已生成（旧密钥已作废）");
    Json(serde_json::json!({
        "status": "ok",
        "api_key": key,
        "message": "密钥仅此一次展示，请立即保存到安全位置",
    }))
}

/// 吊销 API 密钥。
async fn api_key_revoke(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Err(e) = credentials::save_api_key_hash(&state.db_pool, None) {
        tracing::error!("吊销 API 密钥失败: {}", e);
        return http_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "吊销失败",
            "revoke_failed",
        )
        .into_response();
    }
    Json(serde_json::json!({ "status": "ok", "message": "API 密钥已吊销" }))
}

/// API 密钥状态（有没有配置过；永不回显任何密钥材料）。
async fn api_key_status(State(state): State<Arc<AppState>) -> impl IntoResponse {
    let creds = credentials::get(&state.db_pool).unwrap_or_default();
    Json(serde_json::json!({
        "status": "ok",
        "enabled": creds.has_api_key(),
    }))
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
        // 2FA 管理（session 保护）
        .route("/api/2fa/setup", post(two_fa_setup))
        .route("/api/2fa/enable", post(two_fa_enable))
        .route("/api/2fa/disable", post(two_fa_disable))
        .route("/api/2fa/status", get(two_fa_status))
        // API 密钥管理（session 保护）
        .route("/api/apikey/generate", post(api_key_generate))
        .route("/api/apikey/revoke", post(api_key_revoke))
        .route("/api/apikey/status", get(api_key_status))
}
