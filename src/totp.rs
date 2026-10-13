//! RFC 6238 TOTP（基于时间的一次性密码）实现，供可选 2FA 使用。
//!
//! 自带实现而非引入 `totp-rs` 一类的 crate：这里只需要 HOTP 核心 +
//! ±1 时间窗容差 + otpauth:// URL，几十行就能写完，少一个依赖面。
//!
//! 参考：RFC 4226（HOTP）、RFC 6238（TOTP，默认 SHA1/6 位/30s）。

use data_encoding::BASE32_NOPADDING;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha1::Sha1;

const BASE32_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// 生成 20 字节（160-bit）随机密钥，返回 base32（无填充）编码。
/// 20 字节正是 SHA1 摘要长度，也是各家验证器 App 的默认推荐长度。
pub fn generate_secret_base32() -> String {
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    BASE32_NOPADDING.encode(&bytes)
}

/// base32 密钥 → 原始字节。非法输入返回 None（调用方按“验证失败”处理）。
fn decode_secret(secret_b32: &str) -> Option<Vec<u8>> {
    // 容错：验证器偶尔会带填充符/空格/小写。
    let cleaned: String = secret_b32
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=' && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    BASE32_NOPADDING.decode(cleaned.as_bytes()).ok()
}

/// RFC 4226 HOTP：HMAC-SHA1(secret, counter) → 动态截断 → 6 位十进制。
fn hotp(secret: &[u8], counter: u64) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();

    let offset = (digest[19] & 0x0f) as usize;
    let bin = ((u32::from(digest[offset]) & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    format!("{:06}", bin % 1_000_000)
}

/// 当前 Unix 时间（秒）。独立成函数便于测试注入。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 校验 6 位 TOTP 码，±`window` 个 30s 时间窗容差（通常 1）。
/// 使用恒定时间比较，且所有分支都做完整比较，不泄露时序信息。
pub fn verify_totp(secret_b32: &str, code: &str, window: i64) -> bool {
    let code = code.trim();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let Some(secret) = decode_secret(secret_b32) else {
        return false;
    };
    let counter_base = (now_secs() / 30) as i64;
    let mut ok = false;
    for delta in -window..=window {
        let expected = hotp(&secret, (counter_base + delta) as u64);
        // 恒定时间比较；聚合到 ok，保证迭代次数与结果无关。
        ok |= crate::auth::secure_compare(code, &expected);
    }
    ok
}

/// 生成 otpauth:// URL（Google Authenticator 等验证器扫码用的标准格式）。
pub fn otpauth_url(secret_b32: &str, account: &str, issuer: &str) -> String {
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
        percent_encoding::utf8_percent_encode(issuer, percent_encoding::NON_ALPHANUMERIC),
        percent_encoding::utf8_percent_encode(account, percent_encoding::NON_ALPHANUMERIC),
        secret_b32,
        percent_encoding::utf8_percent_encode(issuer, percent_encoding::NON_ALPHANUMERIC),
    )
}

/// 生成 n 组 `xxxx-xxxx` 形式的恢复码（_hex 编码，无 0/o/1/i 混淆位无所谓，
/// 恢复码本身 8 hex 字符 ≈ 32 bit 熵 × 单次有效 + 限速，足够）。
pub fn generate_recovery_codes(n: usize) -> Vec<String> {
    (0..n).map(|_| crate::auth::generate_recovery_code()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4226 附录 D 的测试向量（SHA1, 6 位）。
    #[test]
    fn hotp_matches_rfc4226_vectors() {
        let secret = BASE32_NOPADDING.decode(b"GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap();
        let expected = ["755224", "287082", "359152", "969429", "338314"];
        for (i, exp) in expected.iter().enumerate() {
            assert_eq!(&hotp(&secret, i as u64), exp, "counter {}", i);
        }
    }

    #[test]
    fn verify_accepts_neighbor_window_and_rejects_garbage() {
        let secret = generate_secret_base32();
        // 无法确定当前码（依赖系统时间），但可以验证：
        // 非法输入一律 false。
        assert!(!verify_totp(&secret, "12345", 1));
        assert!(!verify_totp(&secret, "1234567", 1));
        assert!(!verify_totp(&secret, "abcdef", 1));
        assert!(!verify_totp(&secret, "12345 6", 1));
        assert!(!verify_totp("not-base32!!", "123456", 1));
        // 密钥正确时，当前时刻的码在 ±1 窗口内必然通过。
        let bytes = decode_secret(&secret).unwrap();
        let code = hotp(&bytes, now_secs() / 30);
        assert!(verify_totp(&secret, &code, 1));
    }

    #[test]
    fn otpauth_url_contains_secret_and_issuer() {
        let url = otpauth_url("ABC234", "buyi", "tgState");
        assert!(url.starts_with("otpauth://totp/tgState:buyi?"));
        assert!(url.contains("secret=ABC234"));
        assert!(url.contains("issuer=tgState"));
    }
}
