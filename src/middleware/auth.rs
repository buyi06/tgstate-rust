use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
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

fn redirect_or_401(path: &str, accept_html: bool, no_password: bool) -> Response {
    if accept_html && !path.starts_with("/api/") {
        // First-run (no admin password): send browsers to the onboarding page.
        // Otherwise send them to login.
        let dest = if no_password { "/welcome" } else { "/login" };
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = StatusCode::SEE_OTHER;
        resp.headers_mut()
            .insert(axum::http::header::LOCATION, dest.parse().unwrap());
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

fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map_or(false, |v| v == "https")
}

fn forbidden_csrf() -> Response {
    let mut resp = Response::new(Body::from(
        serde_json::json!({
            "status": "error",
            "code": "csrf_origin_mismatch",
            "message": "请求来源校验失败"
        })
        .to_string(),
    ));
    *resp.status_mut() = StatusCode::FORBIDDEN;
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        "application/json; charset=utf-8".parse().unwrap(),
    );
    resp
}

/// CSRF 纵深防御（配合 cookie 的 `SameSite=Strict`）：对会改变状态的请求方法，
/// 若带有 `Origin` 头，则其来源必须与 `Host` 一致。没有 `Origin` 的请求
/// （curl / PicGo 等非浏览器客户端）一律放行，避免破坏 API 用法。
fn origin_allowed(headers: &HeaderMap) -> bool {
    let origin = match headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        Some(o) => o,
        None => return true,
    };
    let host = match headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        Some(h) => h,
        None => return true,
    };
    origin
        .split_once("://")
        .map(|(_, authority)| authority == host)
        .unwrap_or(false)
}

/// Exact / prefix match for public allow-lists.
///
/// IMPORTANT: `"/"` must only match the root path itself. A naive
/// `path.starts_with("/")` would make every route public.
pub fn path_is_allowed(path: &str, allowed: &[&str]) -> bool {
    allowed.iter().any(|p| {
        if *p == "/" {
            path == "/"
        } else if p.ends_with('/') {
            // Prefix form, e.g. "/api/verify/"
            path == p.trim_end_matches('/') || path.starts_with(p)
        } else {
            path == *p || path.starts_with(&format!("{p}/"))
        }
    })
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

    // CSRF 纵深防御：状态变更请求若带 Origin 必须同源（配合 SameSite=Strict cookie）。
    let m = req.method().clone();
    let is_write = m == Method::POST || m == Method::PUT || m == Method::PATCH || m == Method::DELETE;
    if is_write && !origin_allowed(req.headers()) {
        return forbidden_csrf();
    }

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
    if path_is_allowed(&path, &always_public) {
        return next.run(req).await;
    }

    let (active_pwd, session_token) = load_settings_snapshot(&state);
    let no_password = active_pwd.as_deref().unwrap_or("").is_empty();

    // No password configured: only the first-run onboarding surface should be
    // publicly reachable. Everything else (upload/delete/list/settings) is denied
    // so an attacker cannot manage files before the owner finishes setup.
    if no_password {
        let public_no_auth = [
            "/welcome",
            "/login",
            "/api/auth/login",
            "/api/auth/logout",
            "/api/verify/",
            "/api/app-config",
            "/api/app-config/save",
            "/api/app-config/apply",
            "/api/set-password",
        ];
        if path_is_allowed(&path, &public_no_auth) {
            return next.run(req).await;
        }
        let headers = req.headers().clone();
        return redirect_or_401(&path, wants_html(&headers), true);
    }

    // Password configured: a narrow set of API routes is always public so that
    // the login form and logout endpoint remain usable. `/api/verify/*` is no
    // longer public once a password is set — it leaks bot/channel validity.
    let public_api = ["/api/auth/login", "/api/auth/logout"];
    if path_is_allowed(&path, &public_api) {
        return next.run(req).await;
    }
    // Login page itself must be reachable without auth so users can log in.
    if path == "/login" || path == "/pwd" {
        return next.run(req).await;
    }

    let headers = req.headers().clone();
    let cookie = extract_cookie(&headers, COOKIE_NAME);

    if check_session(cookie, active_pwd.as_deref(), session_token.as_deref()) {
        // Sliding expiration: re-issue the cookie with a fresh Max-Age on every
        // authenticated request, so active users stay logged in indefinitely.
        let secure = is_https(&headers);
        let token = session_token.as_deref().unwrap_or("").to_string();
        let mut resp = next.run(req).await;
        if !token.is_empty() {
            if let Ok(cookie_val) =
                HeaderValue::from_str(&auth::build_cookie(&token, secure))
            {
                resp.headers_mut()
                    .append(axum::http::header::SET_COOKIE, cookie_val);
            }
        }
        return resp;
    }

    redirect_or_401(&path, wants_html(&headers), false)
}

#[cfg(test)]
mod tests {
    use super::path_is_allowed;

    #[test]
    fn root_is_exact_only() {
        let allowed = ["/", "/welcome", "/api/set-password"];
        assert!(path_is_allowed("/", &allowed));
        assert!(!path_is_allowed("/api/files", &allowed));
        assert!(!path_is_allowed("/settings", &allowed));
        assert!(!path_is_allowed("/api/files/x", &allowed));
    }

    #[test]
    fn prefix_and_exact_forms() {
        let allowed = ["/api/verify/", "/api/app-config", "/welcome"];
        assert!(path_is_allowed("/api/verify", &allowed));
        assert!(path_is_allowed("/api/verify/bot", &allowed));
        assert!(path_is_allowed("/api/app-config", &allowed));
        assert!(path_is_allowed("/api/app-config/save", &allowed));
        assert!(path_is_allowed("/welcome", &allowed));
        assert!(!path_is_allowed("/api/files", &allowed));
        assert!(!path_is_allowed("/", &allowed));
    }
}
