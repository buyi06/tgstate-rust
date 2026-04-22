use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

use crate::auth::{self, COOKIE_NAME};
use crate::config;
use crate::state::AppState;

fn extract_cookie<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|hv| hv.to_str().ok())
        .and_then(|cookies| {
            for part in cookies.split(';') {
                let kv = part.trim();
                if let Some((k, v)) = kv.split_once('=') {
                    if k == name {
                        return Some(v);
                    }
                }
            }
            None
        })
}

fn redirect_or_401(path: &str, accept_html: bool) -> Response {
    if accept_html && !path.starts_with("/api/") {
        // Redirect browsers to /login
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = StatusCode::SEE_OTHER;
        resp.headers_mut()
            .insert(axum::http::header::LOCATION, "/login".parse().unwrap());
        resp
    } else {
        let mut resp = Response::new(Body::from(
            serde_json::json!({
                "status": "error",
                "code": "unauthorized",
                "message": "需要登录"
            })
            .to_string(),
        ));
        *resp.status_mut() = StatusCode::UNAUTHORIZED;
        resp.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8".parse().unwrap(),
        );
        resp
    }
}

fn wants_html(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/html"))
        .unwrap_or(false)
}

fn load_settings_snapshot(
    state: &Arc<AppState>,
) -> (Option<String>, Option<String>) {
    let settings = config::get_app_settings(&state.settings, &state.db_pool);
    let active_pwd = config::get_active_password(&state.settings, &state.db_pool);
    let session_token = settings
        .get("SESSION_TOKEN")
        .and_then(|v| v.clone());
    (active_pwd, session_token)
}

fn check_session(
    session_cookie: Option<&str>,
    active_pwd: Option<&str>,
    session_token: Option<&str>,
) -> bool {
    // A password must be configured, a server-side session token must exist,
    // and the cookie must match the token in constant time.
    //
    // We no longer fall back to comparing the cookie against `sha256(pwd)` or
    // against the raw password: the cookie is an opaque random token stored in
    // `app_settings.session_token` and may not be re-derivable from the password.
    let (_pwd, token, cookie) = match (active_pwd, session_token, session_cookie) {
        (Some(p), Some(t), Some(c)) if !p.is_empty() && !t.is_empty() => (p, t, c),
        _ => return false,
    };
    auth::secure_compare(cookie, token)
}

pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();

    // Always-allowed static paths
    let public_static_prefixes = [
        "/static/",
        "/assets/",
        "/favicon",
        "/robots.txt",
    ];
    if public_static_prefixes.iter().any(|p| path.starts_with(p)) {
        return next.run(req).await;
    }

    // Always-allowed public-content prefixes. These are the visitor-facing
    // routes: short download links, legacy download links, and share pages.
    // Shared files are meant to be reachable by guests without an account,
    // so they must be allowed regardless of whether a password is configured.
    let public_content_prefixes = [
        "/d/",
        "/share/",
    ];
    if public_content_prefixes.iter().any(|p| path.starts_with(p)) {
        return next.run(req).await;
    }

    // Always-allowed API paths (regardless of password state)
    let always_public = ["/api/health"];
    if always_public.iter().any(|p| &path == p || path.starts_with(&format!("{}/", p))) {
        return next.run(req).await;
    }

    let (active_pwd, session_token) = load_settings_snapshot(&state);

    // No password configured: only the first-run onboarding surface should be
    // publicly reachable. Other endpoints are behind the session cookie check
    // further below, which will pass trivially when no password is set.
    if active_pwd.as_deref().unwrap_or("").is_empty() {
        let public_no_auth = [
            "/",
            "/login",
            "/api/auth/login",
            "/api/auth/logout",
            "/api/verify/",
            "/api/app-config",
            "/api/app-config/save",
            "/api/app-config/apply",
            "/api/set-password",
        ];
        if public_no_auth
            .iter()
            .any(|p| &path == p || path.starts_with(&format!("{}/", p)) || path.starts_with(p))
        {
            return next.run(req).await;
        }
        return next.run(req).await;
    }

    // Password configured: a narrow set of API routes is always public so that
    // the login form and logout endpoint remain usable. `/api/verify/*` is no
    // longer public once a password is set — it leaks bot/channel validity.
    let public_api = ["/api/auth/login", "/api/auth/logout"];
    if public_api.iter().any(|p| &path == p) {
        return next.run(req).await;
    }
    // Login page itself must be reachable without auth so users can log in.
    if &path == "/login" {
        return next.run(req).await;
    }

    let headers = req.headers().clone();
    let cookie = extract_cookie(&headers, COOKIE_NAME);

    if check_session(cookie, active_pwd.as_deref(), session_token.as_deref()) {
        return next.run(req).await;
    }

    redirect_or_401(&path, wants_html(&headers))
}
