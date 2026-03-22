pub const COOKIE_NAME: &str = "tgstate_session";

use sha2::{Digest, Sha256};

pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Check upload auth. Returns Ok(()) if allowed, Err(status_code, message, code) if not.
pub fn ensure_upload_auth(
    has_referer: bool,
    cookie_value: Option<&str>,
    picgo_api_key: Option<&str>,
    pass_word: Option<&str>,
    submitted_key: Option<&str>,
) -> Result<(), (u16, &'static str, &'static str)> {
    let has_picgo = picgo_api_key.map_or(false, |k| !k.is_empty());
    let has_pwd = pass_word.map_or(false, |p| !p.is_empty());

    // Neither set: allow all
    if !has_picgo && !has_pwd {
        return Ok(());
    }

    // Only PICGO_API_KEY set
    if has_picgo && !has_pwd {
        if has_referer {
            return Ok(()); // Web requests allowed
        }
        // API request: check key
        if let Some(key) = submitted_key {
            if key == picgo_api_key.unwrap() {
                return Ok(());
            }
        }
        return Err((401, "无效的 API 密钥", "invalid_api_key"));
    }

    // Only PASS_WORD set
    if !has_picgo && has_pwd {
        if !has_referer {
            return Ok(()); // API requests allowed
        }
        // Web request: check cookie
        if let Some(cookie) = cookie_value {
            if cookie == pass_word.unwrap() {
                return Ok(());
            }
        }
        return Err((401, "需要网页登录", "login_required"));
    }

    // Both set
    if has_referer {
        // Web request: check cookie
        if let Some(cookie) = cookie_value {
            if cookie == pass_word.unwrap() {
                return Ok(());
            }
        }
        return Err((401, "需要网页登录", "login_required"));
    }
    // API request: check key
    if let Some(key) = submitted_key {
        if key == picgo_api_key.unwrap() {
            return Ok(());
        }
    }
    Err((401, "无效的 API 密钥", "invalid_api_key"))
}
