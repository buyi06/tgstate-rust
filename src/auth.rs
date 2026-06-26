pub const COOKIE_NAME: &str = "tgstate_session";

use std::sync::OnceLock;

use rand::RngCore;

use crate::constants;

/// Generate a cryptographically random session token (32 bytes, hex-encoded -> 64 chars).
///
/// This is the canonical value stored in `app_settings.session_token` and set as the
/// session cookie. Because the token is independent of the password, cookies cannot be
/// predicted from the password, and rotating the password (or re-logging in) invalidates
/// prior sessions without touching the password hash.
pub fn generate_session_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn parse_truthy(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Read and cache the `COOKIE_SECURE` env override. When set to a truthy value
/// (`1`/`true`/`yes`/`on`), session cookies are always marked `Secure` regardless
/// of request detection.
fn cookie_secure_override() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("COOKIE_SECURE")
            .map(|v| parse_truthy(&v))
            .unwrap_or(false)
    })
}

/// Read and cache the `SESSION_MAX_AGE_SECS` env override; fall back to the constant.
fn session_max_age_secs() -> u32 {
    static CACHED: OnceLock<u32> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("SESSION_MAX_AGE_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(constants::SESSION_MAX_AGE_SECS)
    })
}

#[cfg(test)]
mod tests {
    use super::{build_share_cookie, generate_session_token, share_unlocked};

    #[test]
    fn generate_session_token_is_64_hex_chars() {
        let t = generate_session_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // Two calls should differ with overwhelming probability.
        assert_ne!(t, generate_session_token());
    }

    #[test]
    fn share_cookie_is_hex_only_and_round_trips() {
        // 真实 argon2 哈希含逗号（m=...,t=...,p=...）——正是这些字符过去会把
        // Set-Cookie 截断。验证派生出的 cookie 值是纯 hex，且能端到端解锁。
        let hash = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0$aGFzaGhhc2hoYXNoaGFzaA";
        let id = "abc123XYz0";

        let set_cookie = build_share_cookie(id, hash, true);
        // 取 `sp_<id>=<value>` 第一段，再取 `=` 右边的值。
        let first = set_cookie.split(';').next().unwrap();
        let value = first.split_once('=').unwrap().1;
        assert!(
            value.chars().all(|c| c.is_ascii_hexdigit()),
            "cookie 值必须是纯 hex，实际: {value}"
        );

        // 把它当作一条混在其它 cookie 中的请求 Cookie 头，应当解锁成功。
        let req_cookie = format!("foo=bar; {first}; baz=qux");
        assert!(share_unlocked(Some(&req_cookie), id, hash));
        // 哈希不一致时绝不解锁。
        assert!(!share_unlocked(Some(&req_cookie), id, "$argon2id$v=19$m=19456,t=2,p=1$other$other"));
        // 没有对应 cookie 时不解锁。
        assert!(!share_unlocked(Some("foo=bar"), id, hash));
    }
}

/// Build a session cookie string with security flags.
///
/// `is_https` is honored when true; the `COOKIE_SECURE` env var can force `Secure`
/// regardless. `SESSION_MAX_AGE_SECS` env controls the Max-Age (defaulting to
/// `constants::SESSION_MAX_AGE_SECS`).
pub fn build_cookie(value: &str, is_https: bool) -> String {
    let secure = if is_https || cookie_secure_override() {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}",
        COOKIE_NAME,
        value,
        session_max_age_secs(),
        secure
    )
}

/// Build a cookie that clears the session.
pub fn build_clear_cookie() -> String {
    format!(
        "{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        COOKIE_NAME
    )
}

/// Constant-time string comparison to prevent timing attacks.
pub fn secure_compare(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes()
        .iter()
        .zip(b.as_bytes().iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Hash a password using argon2.
pub fn hash_password(password: &str) -> Result<String, String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Verify a password against an argon2 hash.
pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::PasswordVerifier;
    use argon2::{Argon2, PasswordHash};
    let parsed = match PasswordHash::new(hash) {
        Ok(h) => h,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Check if a stored value is an argon2 hash (vs plaintext).
pub fn is_hashed(stored: &str) -> bool {
    stored.starts_with("$argon2")
}

/// Verify password: auto-detect hashed vs plaintext.
pub fn verify_password_auto(input: &str, stored: &str) -> bool {
    if is_hashed(stored) {
        verify_password(input, stored)
    } else {
        secure_compare(input, stored)
    }
}

// `ensure_upload_auth` 已随 PicGo / API-key 上传一并移除。上传鉴权现在完全由
// `auth_middleware` 的 session 检查负责：浏览器登录拿到 session cookie 后即可上传。

// --- 分享密码：每个受保护文件用一枚 `sp_<id>` cookie 记录“已解锁”状态 ---

/// 受保护分享文件的解锁 cookie 名。
pub fn share_cookie_name(id: &str) -> String {
    format!("sp_{}", id)
}

/// 从 Cookie 请求头里取出指定名字的 cookie 值。
pub fn extract_cookie_value<'a>(cookie_header: Option<&'a str>, name: &str) -> Option<&'a str> {
    let cookies = cookie_header?;
    for part in cookies.split(';') {
        if let Some((k, v)) = part.trim().split_once('=') {
            if k == name {
                return Some(v);
            }
        }
    }
    None
}

/// 由分享密码哈希派生出放进 cookie 的解锁凭据。argon2 哈希原文含逗号
/// （`m=...,t=...,p=...`）等 RFC 6265 不允许出现在 cookie value 里的字符，
/// 直接塞进 Set-Cookie 可能被浏览器/代理截断，导致解锁后仍反复要求密码。
/// 这里统一 hex 编码成纯 `[0-9a-f]`；写入与校验两端共用本函数，保证一致。
fn share_cookie_value(stored_hash: &str) -> String {
    hex::encode(stored_hash.as_bytes())
}

/// 判断请求是否已解锁某个有密码的分享文件：`sp_<id>` cookie 必须等于由存储的
/// argon2 哈希派生出的解锁凭据（见 `share_cookie_value`；哈希不可逆，作为解锁
/// 凭据放在 HttpOnly cookie 中是安全的）。
pub fn share_unlocked(cookie_header: Option<&str>, id: &str, stored_hash: &str) -> bool {
    match extract_cookie_value(cookie_header, &share_cookie_name(id)) {
        Some(v) => secure_compare(v, &share_cookie_value(stored_hash)),
        None => false,
    }
}

/// 构造分享解锁 cookie（值为 `share_cookie_value` 派生的 hex 凭据；HttpOnly，
/// SameSite=Lax 以便从分享页跳转下载）。
pub fn build_share_cookie(id: &str, stored_hash: &str, is_https: bool) -> String {
    let secure = if is_https { "; Secure" } else { "" };
    format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400{}",
        share_cookie_name(id),
        share_cookie_value(stored_hash),
        secure
    )
}
