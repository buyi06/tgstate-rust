use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::{sha256_hex, COOKIE_NAME};
use crate::config;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct LoginRequest {
    password: String,
}

async fn login(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    let active_password = config::get_active_password(&state.settings);
    let input = payload.password.trim().to_string();

    match active_password {
        Some(ref pwd) if pwd.trim() == input => {
            let hash = sha256_hex(pwd.trim());
            let cookie = format!(
                "{}={}; HttpOnly; SameSite=Lax; Path=/",
                COOKIE_NAME, hash
            );
            (
                [(axum::http::header::SET_COOKIE, cookie)],
                Json(serde_json::json!({
                    "status": "ok",
                    "message": "登录成功"
                })),
            )
                .into_response()
        }
        _ => (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "status": "error",
                "message": "密码错误"
            })),
        )
            .into_response(),
    }
}

async fn logout() -> impl IntoResponse {
    let cookie = format!(
        "{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
        COOKIE_NAME
    );
    (
        [(axum::http::header::SET_COOKIE, cookie)],
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
