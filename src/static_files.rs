//! Read-only file mounts that the reverse proxy used to serve.
//!
//! Both mounts are unauthenticated and deliberately independent of the JSON
//! API: they are merged into the final router after state is provided, so no
//! API middleware or auth layer can wrap them.

use std::path::Path;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use sha2::{Digest, Sha256};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

/// Django's hashed asset names (`app.<12 hex>.css`), so the long lifetime is
/// safe: collectstatic gives changed content a new name.
pub const STATIC_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// Unhashed static paths keep their name across deploys, so they must be
/// revalidated hourly instead of pinned for a year.
pub const STATIC_UNHASHED_CACHE_CONTROL: &str = "public, max-age=3600";
/// Uploaded media can be replaced in place; one day is the nginx default.
pub const MEDIA_CACHE_CONTROL: &str = "public, max-age=86400";
/// Errors and other non-cacheable outcomes must not be remembered by clients.
pub const NO_STORE_CACHE_CONTROL: &str = "no-store";

/// Type served when the extension is outside the media allowlist.
const OCTET_STREAM: &str = "application/octet-stream";
/// SVG can carry script, so it is served with a sandbox CSP.
const SVG: &str = "image/svg+xml";
/// A hash segment as written by `ManifestStaticFilesStorage`.
const HASH_LEN: usize = 12;

/// Stateless router for `/static` and `/media`.
///
/// A missing directory is not an error: `ServeDir` answers 404 until the mount
/// appears. `append_index_html_on_directories(false)` disables directory
/// indices; `ServeDir` also rejects `..` and encoded traversal segments.
///
/// The response headers are attached with `route_layer`, so they cover only
/// the file responses. `layer` would also wrap the router's fallback, and the
/// merged application would then hand every unrelated API response a media
/// cache policy.
pub fn router(static_root: &Path, media_root: &Path) -> Router {
    let static_dir = ServeDir::new(static_root)
        .append_index_html_on_directories(false)
        .precompressed_gzip()
        .precompressed_br();
    let media_dir = ServeDir::new(media_root).append_index_html_on_directories(false);

    let statics = Router::new()
        .nest_service("/static", static_dir)
        .route_layer(middleware::from_fn(static_headers))
        // ServeDir does not add `Vary` for the precompressed variants in this
        // tower-http release, so set it explicitly for this mount.
        .route_layer(SetResponseHeaderLayer::overriding(
            header::VARY,
            HeaderValue::from_static("Accept-Encoding"),
        ));

    let media = Router::new()
        .nest_service("/media", media_dir)
        .route_layer(middleware::from_fn(media_headers));

    statics.merge(media)
}

/// `/static` headers: `nosniff` plus a cache policy that depends on whether
/// the file name is content-addressed.
async fn static_headers(request: Request, next: Next) -> Response {
    let if_none_match = request.headers().get(header::IF_NONE_MATCH).cloned();
    let hashed = is_hashed_asset(request.uri().path());
    let mut response = next.run(request).await;
    revalidate(&mut response, if_none_match.as_ref());

    let status = response.status();
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_policy(
            status,
            if hashed {
                STATIC_CACHE_CONTROL
            } else {
                STATIC_UNHASHED_CACHE_CONTROL
            },
        )),
    );
    response
}

/// `/media` headers: allowlisted `Content-Type`, `nosniff`, a sandbox for SVG,
/// and the same status-aware cache policy as `/static`.
async fn media_headers(request: Request, next: Next) -> Response {
    let if_none_match = request.headers().get(header::IF_NONE_MATCH).cloned();
    let content_type = media_content_type(request.uri().path());
    let mut response = next.run(request).await;
    revalidate(&mut response, if_none_match.as_ref());

    let status = response.status();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if content_type == SVG {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox"),
        );
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_policy(status, MEDIA_CACHE_CONTROL)),
    );
    response
}

/// Serve `304 Not Modified` for a matching `If-None-Match` and expose an
/// `ETag`.
///
/// `ServeDir` only handles `If-Modified-Since` in this tower-http release: it
/// sends `Last-Modified` but no `ETag`, so clients that revalidate with
/// `If-None-Match` would always refetch. The validator is derived from the
/// metadata the file response already carries, which keeps it identical for
/// the same file (and the same precompressed variant).
fn revalidate(response: &mut Response, if_none_match: Option<&HeaderValue>) {
    if response.status() != StatusCode::OK {
        return;
    }

    let Some(etag) = etag_for(response.headers()) else {
        return;
    };
    let matched = if_none_match.is_some_and(|candidate| matches_etag(candidate, &etag));
    if matched {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        response.headers_mut().remove(header::CONTENT_LENGTH);
        *response.body_mut() = Body::empty();
    }
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
}

/// Strong validator built from the served file's metadata. The response has
/// `Last-Modified` and `Content-Length` (and possibly an encoding), which
/// together change whenever the bytes on disk change.
fn etag_for(headers: &HeaderMap) -> Option<String> {
    let last_modified = headers.get(header::LAST_MODIFIED)?.to_str().ok()?;
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let content_encoding = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(last_modified.as_bytes());
    hasher.update(b"|");
    hasher.update(content_length.as_bytes());
    hasher.update(b"|");
    hasher.update(content_encoding.as_bytes());
    let digest = hex::encode(hasher.finalize());
    Some(format!("\"{}\"", &digest[..16]))
}

fn matches_etag(candidate: &HeaderValue, etag: &str) -> bool {
    let Ok(candidate) = candidate.to_str() else {
        return false;
    };
    candidate.split(',').any(|value| {
        let value = value.trim();
        value == "*" || value.strip_prefix("W/").unwrap_or(value) == etag
    })
}

/// Only successful file responses keep their cache policy; 404s, 405s, and
/// other errors are explicitly not stored.
fn cache_policy(status: StatusCode, cacheable: &'static str) -> &'static str {
    if status.is_success() || status == StatusCode::NOT_MODIFIED {
        cacheable
    } else {
        NO_STORE_CACHE_CONTROL
    }
}

/// Whether the request path names a content-addressed static asset.
///
/// `ManifestStaticFilesStorage` writes `app.80a6279921f8.css` but also keeps
/// the unhashed `app.css`. The hash is a 12 hex digit segment directly before
/// the extension; source maps and precompressed siblings add `.map`, `.gz`, or
/// `.br` after that name.
fn is_hashed_asset(path: &str) -> bool {
    let name = strip_asset_variants(file_name(path));

    let mut parts = name.rsplitn(3, '.');
    let (Some(extension), Some(hash), Some(stem)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    !extension.is_empty()
        && !stem.is_empty()
        && hash.len() == HASH_LEN
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Drop `.map`, `.gz`, and `.br` suffixes, including stacked ones, so the hash
/// of the underlying asset name can be checked.
fn strip_asset_variants(name: &str) -> &str {
    let stripped = name
        .strip_suffix(".map")
        .or_else(|| name.strip_suffix(".gz"))
        .or_else(|| name.strip_suffix(".br"));
    match stripped {
        Some(stripped) => strip_asset_variants(stripped),
        None => name,
    }
}

/// Last path segment, without allocating.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or_default()
}

/// `Content-Type` for `/media`, from the extension allowlist. Everything else
/// is an opaque download rather than whatever `mime_guess` would say.
fn media_content_type(path: &str) -> &'static str {
    let name = file_name(path);
    let Some((_, extension)) = name.rsplit_once('.') else {
        return OCTET_STREAM;
    };

    if extension.eq_ignore_ascii_case("png") {
        "image/png"
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if extension.eq_ignore_ascii_case("gif") {
        "image/gif"
    } else if extension.eq_ignore_ascii_case("webp") {
        "image/webp"
    } else if extension.eq_ignore_ascii_case("svg") {
        SVG
    } else if extension.eq_ignore_ascii_case("mp4") {
        "video/mp4"
    } else if extension.eq_ignore_ascii_case("webm") {
        "video/webm"
    } else if extension.eq_ignore_ascii_case("pdf") {
        "application/pdf"
    } else {
        OCTET_STREAM
    }
}
