use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth;
use crate::config;
use crate::database;
use crate::state::{self, AppState};

#[derive(Deserialize)]
pub struct LoginRequest {
    password: String,
}

fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map_or(false, |v| v == "https")
}

async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    let active_password = config::get_active_password(&state.settings, &state.db_pool);
    let input = payload.password.trim().to_string();

    // argon2 校验是 CPU 密集型，放到阻塞线程池，避免阻塞 async 运行时的工作线程。
    let verified = match active_password {
        Some(pwd) => {
            let input = input.clone();
            tokio::task::spawn_blocking(move || auth::verify_password_auto(&input, pwd.trim()))
                .await
                .unwrap_or(false)
        }
        None => false,
    };

    if verified {
        tracing::info!("登录成功");

        // Generate a fresh random session token, persist it alongside existing
        // settings so the middleware's server-side token check succeeds, then
        // set the cookie. This replaces the old sha256(password) cookie.
        let session_token = auth::generate_session_token();

        let mut merged = database::get_app_settings_from_db(&state.db_pool)
            .unwrap_or_default();
        merged.insert(
            "SESSION_TOKEN".to_string(),
            Some(session_token.clone()),
        );
        if let Err(e) = database::save_app_settings_to_db(&state.db_pool, &merged) {
            tracing::error!("保存会话令牌失败: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "status": "error",
                    "message": "服务器错误"
                })),
            )
                .into_response();
        }

        // Refresh in-memory app_settings snapshot (do NOT restart the bot).
        if let Err(e) = state::apply_runtime_settings(state.clone(), false).await {
            tracing::warn!("刷新运行时配置失败 (可忽略): {}", e);
        }

        let cookie = auth::build_cookie(&session_token, is_https(&headers));
        (
            [(axum::http::header::SET_COOKIE, cookie)],
            Json(serde_json::json!({
                "status": "ok",
                "message": "登录成功"
            })),
        )
            .into_response()
    } else {
        tracing::warn!("登录失败：密码错误");
        (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "status": "error",
                "message": "密码错误"
            })),
        )
            .into_response()
    }
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    // 仅当请求带有有效 session cookie 时才轮换服务端 SESSION_TOKEN。
    // 否则任何人 POST /api/auth/logout 都能把管理员踢下线。
    // 无有效会话时仍清 cookie，方便浏览器侧清理本地状态。
    let active_pwd = config::get_active_password(&state.settings, &state.db_pool);
    let mut current = database::get_app_settings_from_db(&state.db_pool).unwrap_or_default();
    let server_token = current
        .get("SESSION_TOKEN")
        .and_then(|v| v.as_deref())
        .unwrap_or("");
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
    let has_valid_session = !server_token.is_empty()
        && active_pwd.as_deref().map_or(false, |p| !p.is_empty())
        && auth::secure_compare(cookie, server_token);

    if has_valid_session {
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

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
}
