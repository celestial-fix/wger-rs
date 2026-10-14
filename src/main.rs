use std::net::SocketAddr;
use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use wger_rs::api;
use wger_rs::auth;
use wger_rs::config::Config;
use wger_rs::state::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("config: {err}");
            std::process::exit(1);
        }
    };
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
    let state = AppState {
        pool,
        config,
        keys: Arc::new(keys),
    };
    let app = api::router().with_state(state);
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("bind {bind}: {err}");
            std::process::exit(1);
        }
    };
    tracing::info!(%bind, "listening");
    if let Err(err) = axum::serve(listener, app).await {
        eprintln!("server: {err}");
        std::process::exit(1);
    }
}
