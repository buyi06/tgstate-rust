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

async fn logout(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // 失效服务端 session token，使所有已下发的 cookie（含可能被窃取的）立即作废。
    // 仅在确有 token 时轮换；未设密码场景 token 本就为空。
    let mut current = database::get_app_settings_from_db(&state.db_pool).unwrap_or_default();
    let has_token = current
        .get("SESSION_TOKEN")
        .and_then(|v| v.as_deref())
        .map_or(false, |t| !t.is_empty());
    if has_token {
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
