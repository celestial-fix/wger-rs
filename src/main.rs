use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use wger_rs::api;
use wger_rs::auth;
use wger_rs::config::Config;
use wger_rs::forwarded::TrustedProxies;
use wger_rs::healthcheck;
use wger_rs::proxy::{self, Proxy};
use wger_rs::state::AppState;
use wger_rs::static_files;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    // Handled before config, tracing, or database setup: the scratch image has
    // no wget, so Docker calls the binary itself for health checks.
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("healthcheck") | Some("--healthcheck")
    ) {
        let code = match healthcheck::run().await {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("healthcheck: {err}");
                1
            }
        };
        std::process::exit(code);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("config: {err}");
            std::process::exit(1);
        }
    };
    warn_missing_root("STATIC_ROOT", &config.static_root);
    warn_missing_root("MEDIA_ROOT", &config.media_root);
    let static_root = config.static_root.clone();
    let media_root = config.media_root.clone();
    let keys = match auth::load_keys(&config.jwt_private_key, &config.jwt_public_key) {
        Ok(keys) => keys,
        Err(err) => {
            eprintln!("jwt: {err}");
            std::process::exit(1);
        }
    };
    let pool = match PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.database_url)
        .await
    {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("database: {err}");
            std::process::exit(1);
        }
    };
    let bind: SocketAddr = match config.bind.parse() {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("BIND {}: {err}", config.bind);
            std::process::exit(1);
        }
    };
    let trusted = match TrustedProxies::from_specs(&config.trusted_proxies) {
        Ok(trusted) => Arc::new(trusted),
        Err(err) => {
            eprintln!("config: {err}");
            std::process::exit(1);
        }
    };
    // Resolve hostname entries before accepting traffic, then keep them fresh:
    // container addresses change when the proxy is recreated.
    trusted.resolve().await;
    trusted.spawn_refresh();
    let proxy = Proxy::new(
        config.django_ui_url.clone(),
        config.powersync_upstream_url.clone(),
        Arc::clone(&trusted),
    );
    if proxy.django_missing() {
        tracing::warn!("DJANGO_UI_URL is unset; Django routes will return 502");
    }
    if proxy.powersync_missing() {
        tracing::warn!("POWERSYNC_UPSTREAM_URL is unset; /ps/ routes will return 502");
    }
    let state = AppState {
        pool,
        config,
        keys: Arc::new(keys),
    };
    // The file mounts are merged after state is provided, so they are outside
    // the API router and cannot inherit an auth or JSON middleware layer. The
    // proxy is the fallback, so every unmatched UI, admin, and WebSocket path
    // reaches Django.
    let app = api::router().with_state(state).merge(static_files::router(
        Path::new(&static_root),
        Path::new(&media_root),
    ));
    let app = proxy::app(app, proxy);
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("bind {bind}: {err}");
            std::process::exit(1);
        }
    };
    tracing::info!(%bind, "listening");
    if let Err(err) =
        wger_rs::server::serve(listener, app, wger_rs::server::CLIENT_HEADER_TIMEOUT).await
    {
        eprintln!("server: {err}");
        std::process::exit(1);
    }
}

/// A missing mount is not fatal: the routes answer 404 until it appears.
fn warn_missing_root(var: &str, path: &str) {
    if !Path::new(path).is_dir() {
        tracing::warn!(
            env = var,
            root = path,
            "directory is missing; requests under it will return 404"
        );
    }
}
