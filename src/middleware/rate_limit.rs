use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::Mutex;

use crate::constants;

#[derive(Clone)]
struct RateEntry {
    count: u32,
    window_start: Instant,
}

#[derive(Clone)]
pub struct RateLimiter {
    /// (max_requests, window_duration)
    login: Arc<Mutex<HashMap<IpAddr, RateEntry>>>,
    upload: Arc<Mutex<HashMap<IpAddr, RateEntry>>>,
    api: Arc<Mutex<HashMap<IpAddr, RateEntry>>>,
    /// Public-facing download / share bucket. These routes are always public
    /// (no login required), so they need their own per-IP cap to prevent a
    /// single client from hammering Telegram via `/d/*` or `/share/*`.
    download: Arc<Mutex<HashMap<IpAddr, RateEntry>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            login: Arc::new(Mutex::new(HashMap::new())),
            upload: Arc::new(Mutex::new(HashMap::new())),
            api: Arc::new(Mutex::new(HashMap::new())),
            download: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

async fn check_rate(
    store: &Mutex<HashMap<IpAddr, RateEntry>>,
    ip: IpAddr,
    max_requests: u32,
    window: Duration,
) -> bool {
    let mut map = store.lock().await;
    let now = Instant::now();

    if map.len() >= constants::RATE_LIMIT_MAX_ENTRIES && !map.contains_key(&ip) {
        // 先清掉本窗口内已过期的条目。
        map.retain(|_, entry| now.duration_since(entry.window_start) < window);
        // 仍然满，则淘汰最旧的条目给新 IP 腾位置——绝不能直接拒绝新请求，
        // 否则攻击者用大量轮换 IP（如 IPv6 段）填满表后会让所有合法新用户被 429。
        while map.len() >= constants::RATE_LIMIT_MAX_ENTRIES {
            let oldest = map
                .iter()
                .min_by_key(|(_, e)| e.window_start)
                .map(|(k, _)| *k);
            match oldest {
                Some(k) => {
                    map.remove(&k);
                }
                None => break,
            }
        }
    }

    let entry = map.entry(ip).or_insert(RateEntry {
        count: 0,
        window_start: now,
    });

    if now.duration_since(entry.window_start) > window {
        entry.count = 1;
        entry.window_start = now;
        true
    } else {
        entry.count += 1;
        entry.count <= max_requests
    }
}

fn parse_truthy(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Read and cache the `TRUST_FORWARDED_FOR` env flag. Defaults to `false` —
/// meaning X-Forwarded-For / X-Real-IP are IGNORED and we always use the
/// actual TCP peer address for rate-limiting. Set to `1`/`true` only when
/// running behind a reverse proxy you control (Nginx/Caddy/Traefik).
fn trust_forwarded_for() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("TRUST_FORWARDED_FOR")
            .map(|v| parse_truthy(&v))
            .unwrap_or(false)
    })
}

fn extract_ip(request: &Request) -> IpAddr {
    if trust_forwarded_for() {
        if let Some(xff) = request.headers().get("x-forwarded-for") {
            if let Ok(s) = xff.to_str() {
                if let Some(first) = s.split(',').next() {
                    if let Ok(ip) = first.trim().parse::<IpAddr>() {
                        return ip;
                    }
                }
            }
        }
        if let Some(xri) = request.headers().get("x-real-ip") {
            if let Ok(s) = xri.to_str() {
                if let Ok(ip) = s.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }
    // Default: use the real TCP peer address injected by
    // `into_make_service_with_connect_info::<SocketAddr>()` in main.rs.
    if let Some(ConnectInfo(addr)) = request.extensions().get::<ConnectInfo<SocketAddr>>() {
        return addr.ip();
    }
    "127.0.0.1".parse().unwrap()
}

pub async fn rate_limit_middleware(
    State(limiter): State<RateLimiter>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let ip = extract_ip(&request);

    let allowed = if path.starts_with("/api/auth/login") {
        check_rate(&limiter.login, ip, constants::RATE_LIMIT_LOGIN_MAX, Duration::from_secs(constants::RATE_LIMIT_WINDOW_SECS)).await
    } else if path.starts_with("/api/upload") {
        check_rate(&limiter.upload, ip, constants::RATE_LIMIT_UPLOAD_MAX, Duration::from_secs(constants::RATE_LIMIT_WINDOW_SECS)).await
    } else if path.starts_with("/api/") {
        check_rate(&limiter.api, ip, constants::RATE_LIMIT_API_MAX, Duration::from_secs(constants::RATE_LIMIT_WINDOW_SECS)).await
    } else if path.starts_with("/d/") || path.starts_with("/share/") {
        check_rate(&limiter.download, ip, constants::RATE_LIMIT_DOWNLOAD_MAX, Duration::from_secs(constants::RATE_LIMIT_WINDOW_SECS)).await
    } else {
        true
    };

    if !allowed {
        tracing::warn!("Rate limit exceeded for {} on {}", ip, path);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(serde_json::json!({
                "status": "error",
                "code": "rate_limited",
                "message": "请求过于频繁，请稍后再试"
            })),
        )
            .into_response();
    }

    next.run(request).await
}

/// Periodically clean up expired entries (call from a background task)
pub async fn cleanup_expired(limiter: &RateLimiter) {
    let window = Duration::from_secs(constants::RATE_LIMIT_WINDOW_SECS);
    let now = Instant::now();

    for store in [&limiter.login, &limiter.upload, &limiter.api, &limiter.download] {
        let mut map = store.lock().await;
        map.retain(|_, entry| now.duration_since(entry.window_start) < window);
    }
}
