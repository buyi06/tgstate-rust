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
use crate::state::{self, AppState};
use crate::totp;

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: Option<String>,
    pub password: String,
    /// 6 位 TOTP 码或 8 位恢复码；启用 2FA 后必填。
    #[serde(default)]
    pub totp: Option<String>,
    /// 服务端在“密码对但缺 2FA”时签发的短时票据，下一步带上它 + totp 重试。
    #[serde(default)]
    pub mfa_ticket: Option<String>,
}

fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map_or(false, |v| v == "https")
}

fn err_response(status: axum::http::StatusCode, code: &str, message: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "status": "error",
            "code": code,
            "message": message,
        })),
    )
        .into_response()
}

/// 内存态的“密码已验过、等 2FA”票据：token → 过期时刻。
/// 90 秒足够人工掏出验证器输码。服务重启即清空（可接受：重新输密码即可）。
struct MfaTickets {
    map: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl MfaTickets {
    fn global() -> &'static Self {
        static INSTANCE: std::sync::OnceLock<MfaTickets> = std::sync::OnceLock::new();
        INSTANCE.get_or_init(|| Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    fn issue(&self) -> String {
        let ticket = auth::generate_session_token();
        let mut m = self.map.lock().unwrap();
        // 顺手清理：防止未完成的两步登录堆积。
        m.retain(|_, exp| *exp > std::time::Instant::now());
        m.insert(ticket.clone(), std::time::Instant::now() + std::time::Duration::from_secs(90));
        ticket
    }

    fn take(&self, ticket: &str) -> bool {
        let mut m = self.map.lock().unwrap();
        match m.remove(ticket) {
            Some(exp) => exp > std::time::Instant::now(),
            None => false,
        }
    }
}

/// 校验用户名+密码（需求 2：用户名+密码双重登录）。
/// 返回 Ok(true/false)；Err 仅用于服务器内部错误。
async fn verify_userpass(
    state: &Arc<AppState>,
    username: &str,
    password: &str,
) -> Result<bool, String> {
    let creds = credentials::get(&state.db_pool).map_err(|e| e.to_string())?;
    if !creds.has_login() {
        // 账户未初始化（理论上来不到：中间件在无账户时会走首启流程；
        // 防御性兜底走旧密码通道，兼容“只设过旧密码、没建过账户”的库）。
        let active_password = config::get_active_password(&state.settings, &state.db_pool);
        let stored = match active_password {
            Some(p) => p.trim().to_string(),
            None => return Ok(false),
        };
        let stored_clone = stored.clone();
        let pw = password.to_string();
        return tokio::task::spawn_blocking(move || {
            auth::verify_password_auto(&pw, &stored_clone)
        })
        .await
        .map_err(|e| e.to_string());
    }

    let want_user = creds.username.clone().unwrap_or_default();
    if !auth::secure_compare(username.trim(), &want_user) {
        return Ok(false);
    }
    let stored_hash = creds.password_hash.clone().unwrap_or_default();
    let pw = password.trim().to_string();
    tokio::task::spawn_blocking(move || auth::verify_password_auto(&pw, stored_hash.trim()))
        .await
        .map_err(|e| e.to_string())
}

/// 校验 TOTP 码或消费一张恢复码。
/// 恢复码命中即从存储列表中移除（单次有效）。
async fn verify_second_factor(
    state: &Arc<AppState>,
    code: &str,
) -> Result<bool, String> {
    let creds = credentials::get(&state.db_pool).map_err(|e| e.to_string())?;
    let secret = creds.totp_secret.clone().unwrap_or_default();
    let code = code.trim().to_string();

    // 1) 正常 TOTP 码
    let s = secret.clone();
    let c = code.clone();
    let totp_ok = tokio::task::spawn_blocking(move || totp::verify_totp(&s, &c, 1))
        .await
        .map_err(|e| e.to_string())?;
    if totp_ok {
        return Ok(true);
    }

    // 2) 恢复码（8 hex）。大小写不敏感；命中即销毁。
    let normalized = code.to_ascii_lowercase();
    let mut remaining: Vec<String> = Vec::new();
    let mut hit = false;
    if let Some(list) = creds.totp_recovery_hashes.as_deref() {
        for h in list.split(',') {
            let h = h.trim();
            if h.is_empty() {
                continue;
            }
            if !hit && auth::secure_compare(&normalized, h) {
                hit = true;
            } else {
                remaining.push(h.to_string());
            }
        }
    }
    if hit {
        credentials::save_totp(
            &state.db_pool,
            Some(&secret),
            Some(&remaining.join(",")),
        )
        .map_err(|e| e.to_string())?;
        tracing::info!("2FA 恢复码已使用并作废（剩余 {} 张）", remaining.len());
    }
    Ok(hit)
}

/// 登录会话落地 + 下发 cookie。
async fn establish_session(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<axum::response::Response, String> {
    let session_token = auth::generate_session_token();
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    database::session_upsert(
        &state.db_pool,
        &session_token,
        auth::session_ttl_i64(),
        ua,
    )
    .map_err(|e| e.to_string())?;
    // 顺手 GC 过期会话（登录是低频操作，摊销成本可忽略）。
    let _ = database::session_gc(&state.db_pool);

    let cookie = auth::build_cookie(&session_token, is_https(headers));
    Ok((
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({
            "status": "ok",
            "message": "登录成功"
        })),
    )
        .into_response())
}

async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    // ---- 第一步（或唯一一步）：用户名 + 密码 ----
    let password_ok = if let Some(ticket) = payload.mfa_ticket.as_deref() {
        // 两步登录的第二步：凭票据跳过密码重验。
        MfaTickets::global().take(ticket)
    } else {
        let username = payload.username.clone().unwrap_or_default();
        let password = payload.password.clone();
        if username.trim().is_empty() {
            false
        } else {
            match verify_userpass(&state, &username, &password).await {
                Ok(ok) => ok,
                Err(e) => {
                    tracing::error!("登录校验内部错误: {}", e);
                    return err_response(
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        "服务器错误",
                    );
                }
            }
        }
    };

    if !password_ok {
        tracing::warn!("登录失败：用户名或密码错误");
        return err_response(
            axum::http::StatusCode::UNAUTHORIZED,
            "bad_credentials",
            "用户名或密码错误",
        );
    }

    // ---- 第二步（可选）：2FA ----
    let has_totp = credentials::get(&state.db_pool)
        .map(|c| c.has_totp())
        .unwrap_or(false);

    if has_totp {
        let code = payload.totp.clone().unwrap_or_default();
        if code.trim().is_empty() {
            // 密码对了但没带码：发一张 90s 票据，前端据此切到输码界面。
            let ticket = MfaTickets::global().issue();
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "status": "mfa_required",
                    "code": "mfa_required",
                    "message": "需要两步验证码",
                    "mfa_ticket": ticket,
                })),
            )
                .into_response();
        }
        match verify_second_factor(&state, &code).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!("登录失败：两步验证码错误");
                return err_response(
                    axum::http::StatusCode::UNAUTHORIZED,
                    "bad_totp",
                    "两步验证码错误",
                );
            }
            Err(e) => {
                tracing::error!("2FA 校验内部错误: {}", e);
                return err_response(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "服务器错误",
                );
            }
        }
    }

    tracing::info!("登录成功");
    match establish_session(&state, &headers).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!("建立会话失败: {}", e);
            err_response(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "服务器错误",
            )
        }
    }
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    // 仅当请求带有有效会话时才作废服务端会话（防止匿名 POST 把别人踢下线）。
    // 持久会话：按 cookie 里的 token 精确删除本浏览器这一行；
    // 旧单令牌：轮换 SESSION_TOKEN 作废（兼容期）。
    let active_pwd = config::get_active_password(&state.settings, &state.db_pool);
    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|hv| hv.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|part| {
                let kv = part.trim();
                kv.split_once('=')
                    .filter(|(k, _)| *k == auth::COOKIE_NAME)
                    .map(|(_, v)| v)
            })
        })
        .unwrap_or("");

    if !cookie.is_empty() {
        // 新表会话：删除该行。
        let _ = database::session_delete(&state.db_pool, cookie);
    }

    // 旧单令牌兼容：cookie 恰好等于 app_settings.session_token 时轮换之。
    let mut current = database::get_app_settings_from_db(&state.db_pool).unwrap_or_default();
    let server_token = current
        .get("SESSION_TOKEN")
        .and_then(|v| v.as_deref())
        .unwrap_or("");
    let has_valid_legacy = !server_token.is_empty()
        && active_pwd.as_deref().map_or(false, |p| !p.is_empty())
        && auth::secure_compare(cookie, server_token);
    if has_valid_legacy {
        current.insert(
            "SESSION_TOKEN".to_string(),
            Some(auth::generate_session_token()),
        );
        if let Err(e) = database::save_app_settings_to_db(&state.db_pool, &current) {
            tracing::error!("登出时轮换会话令牌失败: {}", e);
        } else {
            let _ = state::apply_runtime_settings(state.clone(), false).await;
        }
    }

    (
        [(axum::http::header::SET_COOKIE, auth::build_clear_cookie())],
        Json(serde_json::json!({
            "status": "ok",
            "message": "已退出登录"
        })),
    )
}

/// 登录页配置探测：告诉前端当前需要哪些字段（用户名/2FA），
/// 是否已有账户。公开端点，但只暴露布尔，不泄露用户名。
async fn auth_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let creds = credentials::get(&state.db_pool).unwrap_or_default();
    let legacy_pwd_set = config::get_active_password(&state.settings, &state.db_pool)
        .map_or(false, |p| !p.trim().is_empty());
    let configured = creds.has_login() || legacy_pwd_set;
    Json(serde_json::json!({
        "status": "ok",
        "configured": configured,
        "username_required": creds.has_login(),
        "mfa_enabled": creds.has_totp(),
    }))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/status", get(auth_status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mfa_ticket_roundtrip_and_single_use() {
        let tickets = MfaTickets {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        let t = tickets.issue();
        assert!(tickets.take(&t), "刚签发的票据应当有效");
        assert!(!tickets.take(&t), "票据必须单次有效");
        assert!(!tickets.take("nonexistent"), "不存在的票据无效");
    }
}
