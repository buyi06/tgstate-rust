use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::Router;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

mod auth;
mod config;
mod database;
mod error;
mod events;
mod middleware;
mod routes;
mod state;
mod telegram;

use config::Settings;
use state::AppState;

#[tokio::main]
async fn main() {
    // Load .env file
    let _ = dotenvy::dotenv();

    // Init tracing
    let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(&log_level)),
        )
        .init();

    tracing::info!("应用启动");

    // Init settings
    let settings = Settings::from_env();

    // Init database
    database::init_db(&settings.data_dir);
    tracing::info!("数据库已初始化");

    // Create shared HTTP client
    let http_client = reqwest::Client::builder()
        .pool_max_idle_per_host(50)
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("Failed to create HTTP client");
    tracing::info!("共享的 HTTP 客户端已创建");

    // Init Tera templates
    let mut tera = tera::Tera::new("app/templates/**/*").expect("Failed to init Tera templates");
    tera.register_function("url_for", tera_url_for);

    // Build app state
    let app_settings = config::get_app_settings(&settings);
    let bot_ready = config::is_bot_ready(&app_settings);

    let state = Arc::new(AppState::new(
        settings,
        tera,
        http_client,
        app_settings,
        bot_ready,
    ));

    // Start bot if ready
    if bot_ready {
        if let Err(e) = state::start_bot(state.clone()).await {
            tracing::error!("启动机器人失败: {}", e);
            let mut bot = state.bot_state.lock().await;
            bot.bot_error = Some(e.to_string());
        }
    }

    // Build router
    let app = Router::new()
        .merge(routes::build_router(state.clone()))
        .nest_service("/static", ServeDir::new("app/static"))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 512)) // 512MB
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::auth::auth_middleware,
        ))
        .layer(axum::middleware::from_fn(
            middleware::security_headers::security_headers_middleware,
        ));

    let addr = "0.0.0.0:8000";
    tracing::info!("服务器监听: {}", addr);

    let listener = TcpListener::bind(addr).await.expect("Failed to bind");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state.clone()))
        .await
        .expect("Server error");

    tracing::info!("应用关闭");
}

async fn shutdown_signal(state: Arc<AppState>) {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("收到关闭信号");
    state::stop_bot(&state).await;
}

fn tera_url_for(
    args: &std::collections::HashMap<String, tera::Value>,
) -> tera::Result<tera::Value> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(tera::Value::String(format!("/static{}", path)))
}
