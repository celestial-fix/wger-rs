//! HTTP routes. Slash and no-slash forms are both registered; neither redirects.

mod core;
mod exercise;
mod manager;
mod nutrition;
mod other;
mod routine;
mod upload;
mod writes;

use axum::routing::{get, post};
use axum::Router;

use crate::httpx::ping_db;
use crate::media::serve_media;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    let mut app = Router::new()
        .route("/healthz", get(ping_db))
        .route("/media/*path", get(serve_media))
        .route("/allauth/app/v1/auth/login", post(core::login))
        .route("/allauth/app/v1/tokens/refresh", post(core::refresh));

    for (path, handler) in core::routes() {
        app = app.route(path, handler);
    }
    for (path, handler) in exercise::routes() {
        app = app.route(path, handler);
    }
    for (path, handler) in manager::routes() {
        app = app.route(path, handler);
    }
    for (path, handler) in nutrition::routes() {
        app = app.route(path, handler);
    }
    for (path, handler) in other::routes() {
        app = app.route(path, handler);
    }
    for (path, handler) in upload::routes() {
        app = app.route(path, handler);
    }
    for (path, handler) in writes::routes() {
        app = app.route(path, handler);
    }
    app
}

/// Register both `/path` and `/path/` for a GET (and HEAD) handler.
pub fn get_both<H, T>(
    path: &str,
    handler: H,
) -> Vec<(&'static str, axum::routing::MethodRouter<AppState>)>
where
    H: axum::handler::Handler<T, AppState> + Clone + Send + Sync + 'static,
    T: 'static,
{
    // Paths are leaked once at startup. The set is fixed.
    let with = Box::leak(format!("{path}/").into_boxed_str());
    let without = path.trim_end_matches('/');
    let without: &'static str = Box::leak(without.to_string().into_boxed_str());
    let with_route = get(handler.clone()).head(handler.clone());
    let without_route = get(handler.clone()).head(handler);
    vec![(without, without_route), (with, with_route)]
}
