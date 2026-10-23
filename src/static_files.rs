//! Read-only file mounts that the reverse proxy used to serve.
//!
//! Both mounts are unauthenticated and deliberately independent of the JSON
//! API: they are merged into the final router after state is provided, so no
//! API middleware or auth layer can wrap them.

use std::path::Path;

use axum::http::{header, HeaderValue};
use axum::Router;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

/// Django's hashed asset names, so the long lifetime is safe.
pub const STATIC_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// Uploaded media can be replaced in place; one day is the nginx default.
pub const MEDIA_CACHE_CONTROL: &str = "public, max-age=86400";

/// Stateless router for `/static` and `/media`.
///
/// A missing directory is not an error: `ServeDir` answers 404 until the mount
/// appears. `append_index_html_on_directories(false)` disables directory
/// indices; `ServeDir` also rejects `..` and encoded traversal segments.
///
/// The cache headers are attached with `route_layer`, so they cover only the
/// file responses. `layer` would also wrap the router's fallback, and the
/// merged application would then hand every unrelated API 404 a media cache
/// policy. The nginx config this replaces scoped `add_header` to the
/// `/static/` and `/media/` locations, and `always` still covered the 404s
/// those locations produce for a missing file.
pub fn router(static_root: &Path, media_root: &Path) -> Router {
    let static_dir = ServeDir::new(static_root)
        .append_index_html_on_directories(false)
        .precompressed_gzip()
        .precompressed_br();
    let media_dir = ServeDir::new(media_root).append_index_html_on_directories(false);

    let statics = Router::new()
        .nest_service("/static", static_dir)
        .route_layer(cache_control(STATIC_CACHE_CONTROL))
        // ServeDir does not add `Vary` for the precompressed variants in this
        // tower-http release, so set it explicitly for this mount.
        .route_layer(SetResponseHeaderLayer::overriding(
            header::VARY,
            HeaderValue::from_static("Accept-Encoding"),
        ));

    let media = Router::new()
        .nest_service("/media", media_dir)
        .route_layer(cache_control(MEDIA_CACHE_CONTROL));

    statics.merge(media)
}

fn cache_control(value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static(value))
}
