use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::config;
use crate::database;
use crate::state::AppState;

fn page_cfg(state: &AppState) -> serde_json::Value {
    let app_settings = config::get_app_settings(&state.settings);
    let bot_token = app_settings
        .get("BOT_TOKEN")
        .and_then(|v| v.as_deref())
        .unwrap_or("")
        .trim();
    let channel = app_settings
        .get("CHANNEL_NAME")
        .and_then(|v| v.as_deref())
        .unwrap_or("")
        .trim();
    let bot_ready = !bot_token.is_empty() && !channel.is_empty();

    let mut missing = Vec::new();
    if bot_token.is_empty() {
        missing.push("BOT_TOKEN");
    }
    if channel.is_empty() {
        missing.push("CHANNEL_NAME");
    }

    // Check bot running state synchronously - use try_lock
    let bot_running = state
        .bot_state
        .try_lock()
        .map_or(false, |b| b.bot_running);

    serde_json::json!({
        "bot_ready": bot_ready,
        "bot_running": bot_running,
        "missing": missing,
    })
}

fn enrich_files(files: &[database::FileMetadata]) -> Vec<serde_json::Value> {
    files
        .iter()
        .map(|f| {
            let display_id = f
                .short_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&f.file_id);
            let filesize_mb = format!("{:.2}", f.filesize as f64 / (1024.0 * 1024.0));
            let upload_date_short = f.upload_date.split(' ').next().unwrap_or("").to_string();
            serde_json::json!({
                "file_id": f.file_id,
                "short_id": f.short_id.as_deref().unwrap_or(""),
                "filename": f.filename,
                "filesize": f.filesize,
                "filesize_mb": filesize_mb,
                "upload_date": f.upload_date,
                "upload_date_short": upload_date_short,
                "display_id": display_id,
            })
        })
        .collect()
}

fn render(state: &AppState, template: &str, ctx: &tera::Context) -> Response {
    match state.tera.render(template, ctx) {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("Template render error: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Template error: {}", e),
            )
                .into_response()
        }
    }
}

async fn welcome(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let ctx = tera::Context::new();
    render(&state, "welcome.html", &ctx)
}

async fn index(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg = page_cfg(&state);
    let files = database::get_all_files(&state.db_path()).unwrap_or_default();
    let enriched = enrich_files(&files);

    let mut ctx = tera::Context::new();
    ctx.insert("cfg", &cfg);
    ctx.insert("files", &enriched);
    ctx.insert("request_path", "/");
    render(&state, "index.html", &ctx)
}

async fn login(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut ctx = tera::Context::new();
    ctx.insert("request_path", "/login");
    render(&state, "pwd.html", &ctx)
}

async fn settings_page(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg = page_cfg(&state);
    let mut ctx = tera::Context::new();
    ctx.insert("cfg", &cfg);
    ctx.insert("request_path", "/settings");
    render(&state, "settings.html", &ctx)
}

async fn image_hosting(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg = page_cfg(&state);
    let files = database::get_all_files(&state.db_path()).unwrap_or_default();
    // Filter to image files only
    let image_exts = [
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg", ".bmp", ".ico", ".tiff",
    ];
    let images: Vec<_> = files
        .into_iter()
        .filter(|f| {
            let name = f.filename.to_lowercase();
            image_exts.iter().any(|ext| name.ends_with(ext))
        })
        .collect();
    let enriched = enrich_files(&images);

    let mut ctx = tera::Context::new();
    ctx.insert("cfg", &cfg);
    ctx.insert("files", &enriched);
    ctx.insert("request_path", "/image_hosting");
    render(&state, "image_hosting.html", &ctx)
}

async fn share_page(
    State(state): State<Arc<AppState>>,
    Path(file_id): Path<String>,
) -> impl IntoResponse {
    let meta = database::get_file_by_id(&state.db_path(), &file_id);
    let app_settings = config::get_app_settings(&state.settings);
    let base_url = app_settings
        .get("BASE_URL")
        .and_then(|v| v.as_deref())
        .unwrap_or("")
        .trim_end_matches('/');

    match meta {
        Ok(Some(f)) => {
            let display_id = f
                .short_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&f.file_id);
            let filename_encoded =
                percent_encoding::utf8_percent_encode(&f.filename, percent_encoding::NON_ALPHANUMERIC).to_string();
            let relative_url = format!("/d/{}/{}", display_id, filename_encoded);
            let file_url = if base_url.is_empty() {
                relative_url.clone()
            } else {
                format!("{}{}", base_url, relative_url)
            };
            let filesize_mb = format!("{:.2}", f.filesize as f64 / (1024.0 * 1024.0));
            let upload_date_short = f.upload_date.split(' ').next().unwrap_or("").to_string();

            let file = serde_json::json!({
                "filename": f.filename,
                "filesize": f.filesize,
                "filesize_mb": filesize_mb,
                "upload_date": f.upload_date,
                "upload_date_short": upload_date_short,
                "file_url": file_url,
                "html_code": format!("<a href=\"{}\">{}</a>", file_url, f.filename),
                "markdown_code": format!("[{}]({})", f.filename, file_url),
            });

            let mut ctx = tera::Context::new();
            ctx.insert("file", &file);
            ctx.insert("request_path", &format!("/share/{}", file_id));
            render(&state, "download.html", &ctx)
        }
        _ => (axum::http::StatusCode::NOT_FOUND, "File not found").into_response(),
    }
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/welcome", get(welcome))
        .route("/", get(index))
        .route("/login", get(login))
        .route("/pwd", get(login))
        .route("/settings", get(settings_page))
        .route("/image_hosting", get(image_hosting))
        .route("/share/:file_id", get(share_page))
}
