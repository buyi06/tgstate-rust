/// Maximum upload body size (512 MB)
pub const MAX_UPLOAD_BODY_SIZE: usize = 512 * 1024 * 1024;

/// Telegram chunk size for large file uploads (~19.5 MB, under Telegram's 20MB limit)
pub const TELEGRAM_CHUNK_SIZE: usize = (19.5 * 1024.0 * 1024.0) as usize;

/// HTTP client timeout for file upload/download operations (seconds)
pub const HTTP_TIMEOUT_TRANSFER_SECS: u64 = 300;

/// HTTP client timeout for metadata/API operations (seconds)
pub const HTTP_TIMEOUT_METADATA_SECS: u64 = 30;

/// Rate limit: login attempts per window
pub const RATE_LIMIT_LOGIN_MAX: u32 = 5;
/// Rate limit: upload requests per window
pub const RATE_LIMIT_UPLOAD_MAX: u32 = 10;
/// Rate limit: general API requests per window
pub const RATE_LIMIT_API_MAX: u32 = 120;
/// Rate limit: public download/share requests per window. Covers `/d/*` and
/// `/share/*`. Higher than API because legitimate browsers issue many of
/// these per page load (HTML + thumbnails + Range requests for video).
pub const RATE_LIMIT_DOWNLOAD_MAX: u32 = 300;
/// Rate limit: window duration in seconds
pub const RATE_LIMIT_WINDOW_SECS: u64 = 60;

/// Rate limiter cleanup interval in seconds
pub const RATE_LIMIT_CLEANUP_INTERVAL_SECS: u64 = 120;

/// Maximum entries per rate limiter bucket before forced eviction
pub const RATE_LIMIT_MAX_ENTRIES: usize = 10_000;

/// SSE keepalive interval in seconds
pub const SSE_KEEPALIVE_SECS: u64 = 15;

/// Session cookie max-age in seconds (7 days). Combined with the sliding
/// refresh in `middleware::auth`, a user who visits the site at least once
/// a week stays logged in indefinitely; otherwise the cookie expires and
/// the user must log in again. May be overridden by env `SESSION_MAX_AGE_SECS`
/// at runtime (see `auth::build_cookie`). The previous 30-day default was
/// longer than SECURITY.md advertised and is shortened here.
pub const SESSION_MAX_AGE_SECS: u32 = 7 * 24 * 60 * 60;

/// Short ID length for file identifiers. Ten chars of 62-alphabet ≈ 60 bits of
/// entropy, which is practical-only enumeration-resistant for a single-admin
/// self-hosted tool.
pub const SHORT_ID_LENGTH: usize = 10;

/// Broadcast event bus capacity
pub const EVENT_BUS_CAPACITY: usize = 200;

/// Bot polling long-poll timeout in seconds
pub const BOT_POLL_TIMEOUT_SECS: u64 = 30;

/// Maximum number of file IDs accepted in a single batch-delete request.
pub const BATCH_DELETE_MAX: usize = 100;

/// Default number of file chunks uploaded to Telegram concurrently during a
/// single multi-chunk upload. Overridable at runtime via `UPLOAD_CONCURRENCY`.
/// Peak upload memory is roughly `(value + 1) × TELEGRAM_CHUNK_SIZE`, so the
/// default of 3 costs ~80MB in exchange for ~3× the serial upload throughput
/// whenever the bottleneck is the server→Telegram link rather than ingress.
pub const MAX_CONCURRENT_CHUNK_UPLOADS: usize = 3;

/// Max attempts for a single Telegram `sendDocument` (initial try + retries),
/// used to ride out transient network errors and HTTP 429 rate limiting.
pub const TG_UPLOAD_MAX_RETRIES: u32 = 4;
