use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

use crate::auth::{self, COOKIE_NAME};
use crate::config;
use crate::constants;
use crate::database;
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

/// 是否已配置账户（用户名+密码）。要求 2 与登录流程共用：一旦设置了
/// 用户名，登录必须同时带对用户名和密码。
fn account_configured(state: &Arc<AppState>) -> bool {
    crate::credentials::get(&state.db_pool)
        .map(|c| c.has_login())
        .unwrap_or(false)
}

/// 从 Authorization 头解析 Bearer API 密钥；也接受 `X-Api-Key` 头，
/// 方便 curl 不想引号转义时使用。
fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(auth_hdr) = headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(token) = auth_hdr.strip_prefix("Bearer ").or_else(|| auth_hdr.strip_prefix("bearer ")) {
            let t = token.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 校验 API 密钥（sha256 后与 login_credentials.api_key_hash 恒定时间比对）。
fn api_key_valid(state: &Arc<AppState>, key: &str) -> bool {
    match crate::credentials::get(&state.db_pool) {
        Ok(creds) => creds
            .api_key_hash
            .as_deref()
            .map_or(false, |h| crate::credentials::verify_api_key(key, h)),
        Err(_) => false,
    }
}

/// API 密钥可访问的端点（CLI/程序化用）：文件清单、上传、删除、下载解析。
/// 注意：不含设置/2FA/账户管理端点——密钥泄漏也不能改密钥自身。
fn api_key_scope(path: &str) -> bool {
    let scoped = [
        "/api/files",
        "/api/upload",
        "/api/batch_delete",
        "/api/health",
    ];
    // /api/files/:id 删除、/api/files 本体都放行；其余管理端点不放行。
    scoped.iter().any(|p| path == *p || path.starts_with(&format!("{p}/")))
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

/// 持久会话校验：cookie 里的 token 在 sessions 表中存在且未过期。
/// 命中后由调用方做滑动续期（见 auth_middleware）。
fn persisted_session_valid(state: &Arc<AppState>, token: &str) -> bool {
    token.len() == 64
        && token.chars().all(|c| c.is_ascii_hexdigit())
        && matches!(database::session_get(&state.db_pool, token), Ok(Some(_)))
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

    let (active_pwd, _legacy_token) = load_settings_snapshot(&state);
    let no_password = active_pwd.as_deref().unwrap_or("").is_empty() && !account_configured(&state);

    // No password configured: only the first-run onboarding surface should be
    // publicly reachable. Everything else (upload/delete/list/settings) is denied
    // so an attacker cannot manage files before the owner finishes setup.
    if no_password {
        let public_no_auth = [
            "/welcome",
            "/login",
            "/api/auth/login",
            "/api/auth/logout",
            "/api/auth/status",
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

    // Account exists (username+password row) — legacy app_settings.pass_word
    // may be empty. Login/logout/status must stay reachable either way.
    let public_api = ["/api/auth/login", "/api/auth/logout", "/api/auth/status"];
    if path_is_allowed(&path, &public_api) {
        return next.run(req).await;
    }
    // Login page itself must be reachable without auth so users can log in.
    if path == "/login" || path == "/pwd" {
        return next.run(req).await;
    }

    let headers = req.headers().clone();

    // ---- 需求 4：API 密钥通道（CLI / 程序化访问）----
    // 运作在会话检查之前：无 cookie 的 curl 用 Authorization: Bearer tgsk_...
    // 即可访问受限范围内的 API。范围刻意窄（文件清单/上传/删除/健康检查），
    // 不含任何账户/2FA/设置管理端点。
    if let Some(api_key) = extract_api_key(&headers) {
        if api_key_valid(&state, &api_key) && api_key_scope(&path) {
            return next.run(req).await;
        }
        // 带了密钥但无效/越权 → 直接 401（不落到页面重定向；CLI 需要明确的 JSON 错误）。
        tracing::warn!("API 密钥校验失败或超出范围");
        return redirect_or_401(&path, false, false);
    }

    let cookie = extract_cookie(&headers, COOKIE_NAME);

    // ---- 需求 1：持久会话优先（sessions 表）。----
    // 命中后滑动续期并刷新 cookie Max-Age。旧 app_settings.session_token
    // 单令牌逻辑作为兼容期 fallback 保留（老浏览器上还挂着旧 cookie 时，
    // 升级后第一次请求仍可用，随后登录即迁到新表）。
    let persisted_ok = cookie
        .map(|c| persisted_session_valid(&state, c))
        .unwrap_or(false);

    if persisted_ok {
        let token = cookie.unwrap_or("").to_string();
        // 滑动续期（有绝对上限，见 session_sliding_renew）。
        let _ = database::session_sliding_renew(
            &state.db_pool,
            &token,
            auth::session_ttl_i64(),
            constants::SESSION_ABSOLUTE_MAX_SECS,
        );
        let secure = is_https(&headers);
        let mut resp = next.run(req).await;
        if let Ok(cookie_val) = HeaderValue::from_str(&auth::build_cookie(&token, secure)) {
            resp.headers_mut()
                .append(axum::http::header::SET_COOKIE, cookie_val);
        }
        return resp;
    }

    if let Some(c) = cookie.filter(|c| !c.is_empty()) {
        if check_session(Some(c), active_pwd.as_deref(), _legacy_token.as_deref()) {
            // 旧单令牌命中：趁这次请求透明迁移到 sessions 表。
            // 迁移失败不阻塞请求（按已认证放行），下次请求重试。
            let ua = headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok());
            let _ = database::session_upsert(
                &state.db_pool,
                c,
                auth::session_ttl_i64(),
                ua,
            );
            let _ = database::session_gc(&state.db_pool);
            let _ = database::save_app_settings_to_db(
                &state.db_pool,
                &database::get_app_settings_from_db(&state.db_pool)
                    .map(|mut m| {
                        m.insert("SESSION_TOKEN".to_string(), None);
                        m
                    })
                    .unwrap_or_default(),
            );
            let secure = is_https(&headers);
            let mut resp = next.run(req).await;
            if let Ok(cookie_val) = HeaderValue::from_str(&auth::build_cookie(c, secure)) {
                resp.headers_mut()
                    .append(axum::http::header::SET_COOKIE, cookie_val);
            }
            return resp;
        }
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
