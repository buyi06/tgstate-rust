use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::auth::{sha256_hex, COOKIE_NAME};
use crate::config;
use crate::error::error_payload;
use crate::state::AppState;

fn get_cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| {
                let c = c.trim();
                if let Some(val) = c.strip_prefix(&format!("{}=", name)) {
                    Some(val.to_string())
                } else {
                    None
                }
            })
        })
}

pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();

    // Public paths that never require auth
    let public_prefixes = ["/static", "/api", "/d", "/favicon.ico"];
    let is_public = public_prefixes.iter().any(|p| path.starts_with(p));

    let active_password = config::get_active_password(&state.settings);

    match active_password {
        None => {
            // No password set: only allow /welcome, /settings, and public paths
            if path == "/welcome" || path == "/settings" || is_public {
                return next.run(request).await;
            }
            return Redirect::temporary("/welcome").into_response();
        }
        Some(ref active_pwd) => {
            // Password is set: normal auth flow
            if path == "/welcome" {
                return Redirect::temporary("/").into_response();
            }

            if is_public {
                // Check protected API endpoints
                let protected_api_prefixes = [
                    "/api/upload",
                    "/api/delete",
                    "/api/files",
                    "/api/batch_delete",
                    "/api/app-config",
                    "/api/reset-config",
                    "/api/set-password",
                ];

                let is_protected_api = protected_api_prefixes
                    .iter()
                    .any(|p| path.starts_with(p));

                if is_protected_api {
                    let session = get_cookie_value(request.headers(), COOKIE_NAME);
                    let token = sha256_hex(active_pwd);

                    let is_auth = session
                        .as_ref()
                        .map_or(false, |s| s == &token || s == active_pwd);

                    if !is_auth {
                        return (
                            StatusCode::UNAUTHORIZED,
                            axum::Json(serde_json::json!({
                                "detail": error_payload("需要网页登录", "login_required", None)
                            })),
                        )
                            .into_response();
                    }
                }

                return next.run(request).await;
            }

            // Protected pages
            let protected_pages = ["/", "/image_hosting", "/files", "/settings"];
            let is_protected_page = protected_pages.iter().any(|p| path == *p);

            let session = get_cookie_value(request.headers(), COOKIE_NAME);
            let token = sha256_hex(active_pwd);
            let is_auth = session
                .as_ref()
                .map_or(false, |s| s == &token || s == active_pwd);

            if is_protected_page {
                if !is_auth {
                    return Redirect::temporary("/login").into_response();
                }
                return next.run(request).await;
            }

            // Login pages: redirect to / if already authenticated
            if path == "/login" || path == "/pwd" {
                if is_auth {
                    return Redirect::temporary("/").into_response();
                }
                return next.run(request).await;
            }

            // Everything else: pass through
            next.run(request).await
        }
    }
}
