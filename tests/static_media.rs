//! Integration tests for the `/static` and `/media` mounts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use tower::ServiceExt;
use wger_rs::static_files;
use wger_rs::static_files::{
    MEDIA_CACHE_CONTROL, NO_STORE_CACHE_CONTROL, STATIC_CACHE_CONTROL,
    STATIC_UNHASHED_CACHE_CONTROL,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Throwaway directory tree removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("wger-rs-{tag}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(root.join("static")).expect("create static root");
        std::fs::create_dir_all(root.join("media")).expect("create media root");
        Self { root }
    }

    fn static_dir(&self) -> PathBuf {
        self.root.join("static")
    }

    fn media_dir(&self) -> PathBuf {
        self.root.join("media")
    }

    fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, bytes).expect("write fixture file");
        path
    }

    fn router(&self) -> Router {
        static_files::router(&self.static_dir(), &self.media_dir())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

async fn request(app: &Router, method: Method, uri: &str) -> Reply {
    request_with(app, method, uri, &[]).await
}

async fn request_with(app: &Router, method: Method, uri: &str, headers: &[(&str, &str)]) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::empty()).expect("build request");
    let response = app.clone().oneshot(request).await.expect("infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body")
        .to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

fn header(reply: &Reply, name: header::HeaderName) -> &str {
    reply
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

#[tokio::test]
async fn static_file_is_served_with_long_lived_cache_headers() {
    let fixture = Fixture::new("static-ok");
    fixture.write("static/app.80a6279921f8.css", b"body{color:red}");

    let reply = request(
        &fixture.router(),
        Method::GET,
        "/static/app.80a6279921f8.css",
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, b"body{color:red}");
    assert_eq!(header(&reply, header::CACHE_CONTROL), STATIC_CACHE_CONTROL);
    assert_eq!(header(&reply, header::VARY), "Accept-Encoding");
}

#[tokio::test]
async fn static_head_returns_headers_without_a_body() {
    let fixture = Fixture::new("static-head");
    fixture.write("static/app.js", b"console.log(1)");

    let reply = request(&fixture.router(), Method::HEAD, "/static/app.js").await;

    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.is_empty());
    assert_eq!(header(&reply, header::CONTENT_LENGTH), "14");
    assert_eq!(
        header(&reply, header::CACHE_CONTROL),
        STATIC_UNHASHED_CACHE_CONTROL
    );
}

#[tokio::test]
async fn static_missing_file_is_404() {
    let fixture = Fixture::new("static-missing");
    let reply = request(&fixture.router(), Method::GET, "/static/nope.css").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn static_directories_are_not_listed() {
    let fixture = Fixture::new("static-dir");
    fixture.write("static/nested/asset.txt", b"asset");

    let reply = request(&fixture.router(), Method::GET, "/static/nested/").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(!String::from_utf8_lossy(&reply.body).contains("asset.txt"));
}

#[tokio::test]
async fn static_precompressed_variants_are_used() {
    let fixture = Fixture::new("static-precompressed");
    fixture.write("static/app.js", b"plain");
    fixture.write("static/app.js.br", b"brotli-bytes");
    fixture.write("static/app.js.gz", b"gzip-bytes");

    let brotli = request_with(
        &fixture.router(),
        Method::GET,
        "/static/app.js",
        &[("accept-encoding", "br")],
    )
    .await;
    assert_eq!(brotli.status, StatusCode::OK);
    assert_eq!(header(&brotli, header::CONTENT_ENCODING), "br");
    assert_eq!(brotli.body, b"brotli-bytes");

    let gzip = request_with(
        &fixture.router(),
        Method::GET,
        "/static/app.js",
        &[("accept-encoding", "gzip")],
    )
    .await;
    assert_eq!(gzip.status, StatusCode::OK);
    assert_eq!(header(&gzip, header::CONTENT_ENCODING), "gzip");
    assert_eq!(gzip.body, b"gzip-bytes");
}

#[tokio::test]
async fn media_file_is_served_with_a_day_of_caching() {
    let fixture = Fixture::new("media-ok");
    fixture.write("media/probe.bin", b"0123456789");

    let reply = request(&fixture.router(), Method::GET, "/media/probe.bin").await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, b"0123456789");
    assert_eq!(header(&reply, header::CACHE_CONTROL), MEDIA_CACHE_CONTROL);
}

#[tokio::test]
async fn media_head_returns_headers_without_a_body() {
    let fixture = Fixture::new("media-head");
    fixture.write("media/video.mp4", b"synthetic");

    let reply = request(&fixture.router(), Method::HEAD, "/media/video.mp4").await;

    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.is_empty());
    assert_eq!(header(&reply, header::CONTENT_LENGTH), "9");
    assert_eq!(header(&reply, header::CACHE_CONTROL), MEDIA_CACHE_CONTROL);
    assert_eq!(header(&reply, header::CONTENT_TYPE), "video/mp4");
}

#[tokio::test]
async fn media_content_type_follows_the_extension_allowlist() {
    let fixture = Fixture::new("media-types");
    let cases = [
        ("photo.png", "image/png"),
        ("photo.jpg", "image/jpeg"),
        ("photo.jpeg", "image/jpeg"),
        ("PHOTO.JPG", "image/jpeg"),
        ("animation.gif", "image/gif"),
        ("photo.webp", "image/webp"),
        ("vector.svg", "image/svg+xml"),
        ("clip.mp4", "video/mp4"),
        ("clip.webm", "video/webm"),
        ("document.pdf", "application/pdf"),
    ];

    let router = fixture.router();
    for (name, expected) in cases {
        fixture.write(&format!("media/{name}"), b"x");
        let reply = request(&router, Method::GET, &format!("/media/{name}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{name}");
        assert_eq!(header(&reply, header::CONTENT_TYPE), expected, "{name}");
    }
}

#[tokio::test]
async fn media_unknown_extension_is_an_opaque_download() {
    let fixture = Fixture::new("media-unknown");
    let cases = [
        "probe.bin",
        "notes.txt",
        "app.css",
        "archive.tar.gz",
        "no-extension",
        "trailing.",
    ];

    let router = fixture.router();
    for name in cases {
        fixture.write(&format!("media/{name}"), b"x");
        let reply = request(&router, Method::GET, &format!("/media/{name}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{name}");
        assert_eq!(
            header(&reply, header::CONTENT_TYPE),
            "application/octet-stream",
            "{name}"
        );
    }
}

#[tokio::test]
async fn media_svg_is_sandboxed() {
    let fixture = Fixture::new("media-svg");
    fixture.write(
        "media/vector.svg",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    );
    fixture.write("media/photo.png", b"x");

    let router = fixture.router();
    let svg = request(&router, Method::GET, "/media/vector.svg").await;
    assert_eq!(svg.status, StatusCode::OK);
    assert_eq!(
        header(&svg, header::CONTENT_SECURITY_POLICY),
        "sandbox",
        "SVG must be served with a sandboxing CSP"
    );

    let png = request(&router, Method::GET, "/media/photo.png").await;
    assert_eq!(
        header(&png, header::CONTENT_SECURITY_POLICY),
        "",
        "only SVG carries the sandbox CSP"
    );
}

#[tokio::test]
async fn both_mounts_send_nosniff() {
    let fixture = Fixture::new("nosniff");
    fixture.write("static/app.css", b"body{}");
    fixture.write("media/photo.png", b"x");

    let router = fixture.router();
    for uri in ["/static/app.css", "/media/photo.png"] {
        let reply = request(&router, Method::GET, uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert_eq!(
            header(&reply, header::X_CONTENT_TYPE_OPTIONS),
            "nosniff",
            "{uri}"
        );
    }

    // Missing files are still answered by the mount, so they also carry it.
    for uri in ["/static/missing.css", "/media/missing.png"] {
        let reply = request(&router, Method::GET, uri).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            header(&reply, header::X_CONTENT_TYPE_OPTIONS),
            "nosniff",
            "{uri}"
        );
    }
}

#[tokio::test]
async fn cached_responses_are_not_marked_no_store() {
    let fixture = Fixture::new("cache-status");
    fixture.write("static/app.80a6279921f8.css", b"body{}");
    fixture.write("media/photo.png", b"x");

    let router = fixture.router();
    let hashed = request(&router, Method::GET, "/static/app.80a6279921f8.css").await;
    assert_eq!(header(&hashed, header::CACHE_CONTROL), STATIC_CACHE_CONTROL);

    let media = request(&router, Method::GET, "/media/photo.png").await;
    assert_eq!(header(&media, header::CACHE_CONTROL), MEDIA_CACHE_CONTROL);
}

#[tokio::test]
async fn errors_are_no_store() {
    let fixture = Fixture::new("no-store");
    fixture.write("static/app.css", b"body{}");
    fixture.write("media/photo.png", b"x");

    let router = fixture.router();

    // 404 from both mounts.
    for uri in ["/static/missing.css", "/media/missing.png"] {
        let reply = request(&router, Method::GET, uri).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            header(&reply, header::CACHE_CONTROL),
            NO_STORE_CACHE_CONTROL,
            "{uri}"
        );
    }

    // A method ServeDir rejects is not a file response either.
    let reply = request(&router, Method::POST, "/static/app.css").await;
    assert!(!reply.status.is_success());
    assert_eq!(
        header(&reply, header::CACHE_CONTROL),
        NO_STORE_CACHE_CONTROL
    );
}

#[tokio::test]
async fn not_modified_keeps_the_cache_policy() {
    let fixture = Fixture::new("not-modified-etag");
    fixture.write("static/app.80a6279921f8.css", b"body{}");

    let router = fixture.router();
    let first = request(&router, Method::GET, "/static/app.80a6279921f8.css").await;
    assert_eq!(first.status, StatusCode::OK);
    let etag = header(&first, header::ETAG).to_owned();
    assert!(!etag.is_empty(), "ServeDir must provide an ETag");

    let reply = request_with(
        &router,
        Method::GET,
        "/static/app.80a6279921f8.css",
        &[("if-none-match", &etag)],
    )
    .await;

    assert_eq!(reply.status, StatusCode::NOT_MODIFIED);
    assert_eq!(header(&reply, header::CACHE_CONTROL), STATIC_CACHE_CONTROL);
}

#[tokio::test]
async fn not_modified_via_if_modified_since_keeps_the_cache_policy() {
    let fixture = Fixture::new("not-modified-date");
    fixture.write("media/photo.png", b"x");

    let router = fixture.router();
    let first = request(&router, Method::GET, "/media/photo.png").await;
    assert_eq!(first.status, StatusCode::OK);
    let last_modified = header(&first, header::LAST_MODIFIED).to_owned();
    assert!(
        !last_modified.is_empty(),
        "ServeDir must send Last-Modified"
    );

    let reply = request_with(
        &router,
        Method::GET,
        "/media/photo.png",
        &[("if-modified-since", &last_modified)],
    )
    .await;

    assert_eq!(reply.status, StatusCode::NOT_MODIFIED);
    assert_eq!(header(&reply, header::CACHE_CONTROL), MEDIA_CACHE_CONTROL);
}

#[tokio::test]
async fn static_hashed_assets_are_immutable() {
    let fixture = Fixture::new("static-hashed");
    fixture.write("static/app.80a6279921f8.css", b"body{}");
    fixture.write("static/app.80a6279921f8.js", b"js");
    fixture.write("static/app.80a6279921f8.css.map", b"{}");
    fixture.write("static/app.80a6279921f8.js.gz", b"gzip");
    fixture.write("static/app.80a6279921f8.js.br", b"brotli");

    let router = fixture.router();
    for uri in [
        "/static/app.80a6279921f8.css",
        "/static/app.80a6279921f8.js",
        "/static/app.80a6279921f8.css.map",
        "/static/app.80a6279921f8.js.gz",
        "/static/app.80a6279921f8.js.br",
    ] {
        let reply = request(&router, Method::GET, uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert_eq!(
            header(&reply, header::CACHE_CONTROL),
            STATIC_CACHE_CONTROL,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn static_unhashed_assets_are_cached_for_an_hour() {
    let fixture = Fixture::new("static-unhashed");
    fixture.write("static/app.css", b"body{}");
    fixture.write("static/app.js", b"js");
    fixture.write("static/app.css.map", b"{}");

    let router = fixture.router();
    for uri in ["/static/app.css", "/static/app.js", "/static/app.css.map"] {
        let reply = request(&router, Method::GET, uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert_eq!(
            header(&reply, header::CACHE_CONTROL),
            STATIC_UNHASHED_CACHE_CONTROL,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn static_hash_detection_requires_twelve_hex_digits() {
    let fixture = Fixture::new("static-hash-shapes");
    // 11 and 13 hex digits, a non-hex character, and a hash-like stem without
    // an extension are all unhashed.
    for name in [
        "app.80a6279921f.css",
        "app.80a6279921f8a.css",
        "app.80a6279921fg.css",
        "pkg.80a6279921f8",
    ] {
        fixture.write(&format!("static/{name}"), b"x");
    }

    let router = fixture.router();
    for name in [
        "app.80a6279921f.css",
        "app.80a6279921f8a.css",
        "app.80a6279921fg.css",
        "pkg.80a6279921f8",
    ] {
        let reply = request(&router, Method::GET, &format!("/static/{name}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{name}");
        assert_eq!(
            header(&reply, header::CACHE_CONTROL),
            STATIC_UNHASHED_CACHE_CONTROL,
            "{name}"
        );
    }
}

#[tokio::test]
async fn missing_roots_return_404_instead_of_failing() {
    let router = static_files::router(
        Path::new("/definitely/not/a/static/root"),
        Path::new("/definitely/not/a/media/root"),
    );

    let static_reply = request(&router, Method::GET, "/static/app.css").await;
    assert_eq!(static_reply.status, StatusCode::NOT_FOUND);

    let media_reply = request(&router, Method::GET, "/media/probe.bin").await;
    assert_eq!(media_reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn traversal_never_escapes_the_mounts() {
    let fixture = Fixture::new("traversal");
    // A file beside the served roots that must never be reachable.
    fixture.write("secret.txt", b"top-secret");

    let attempts = [
        "/static/../secret.txt",
        "/static/..%2Fsecret.txt",
        "/static/%2e%2e/secret.txt",
        "/static/%2e%2e%2fsecret.txt",
        "/static/nested/../../secret.txt",
        "/static/../etc/passwd",
        "/static/..%2Fetc%2Fpasswd",
        "/static/%2e%2e/etc/passwd",
        "/static/%2e%2e%2fetc%2fpasswd",
        "/media/../secret.txt",
        "/media/..%2Fsecret.txt",
        "/media/%2e%2e/secret.txt",
        "/media/../etc/passwd",
    ];

    let router = fixture.router();
    for uri in attempts {
        let reply = request(&router, Method::GET, uri).await;
        assert_ne!(reply.status, StatusCode::OK, "{uri} must not be served");
        assert!(
            !reply.body.windows(9).any(|window| window == b"top-secret"),
            "{uri} leaked a file outside the mount"
        );
    }
}

#[tokio::test]
async fn media_range_requests_still_work() {
    let fixture = Fixture::new("media-range");
    fixture.write("media/probe.bin", b"0123456789");

    let reply = request_with(
        &fixture.router(),
        Method::GET,
        "/media/probe.bin",
        &[("range", "bytes=0-9")],
    )
    .await;

    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(reply.body, b"0123456789");
    assert_eq!(header(&reply, header::CACHE_CONTROL), MEDIA_CACHE_CONTROL);
}

#[tokio::test]
async fn the_mounts_do_not_leak_cache_headers_onto_other_routes() {
    let fixture = Fixture::new("fallback");
    fixture.write("static/app.css", b"body{}");

    // The API router is merged with the file mounts, so the mounts must not
    // add cache headers to the merged application's other responses.
    let api = Router::new().route("/api/v2/thing/", axum::routing::get(|| async { "{}" }));
    let app = api.merge(fixture.router());

    let reply = request(&app, Method::GET, "/api/v2/thing/").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(header(&reply, header::CACHE_CONTROL), "");

    // A path that matches no route at all falls back to the 404 handler.
    let reply = request(&app, Method::GET, "/api/v2/missing/").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(header(&reply, header::CACHE_CONTROL), "");
}

#[tokio::test]
async fn the_mounts_are_served_without_authentication() {
    let fixture = Fixture::new("noauth");
    fixture.write("static/app.css", b"body{}");
    fixture.write("media/probe.bin", b"0123456789");

    // A malformed credential must not change the outcome: the mounts are
    // outside the API's auth layer.
    let router = fixture.router();
    for (uri, expected) in [
        ("/static/app.css", "body{}"),
        ("/media/probe.bin", "0123456789"),
    ] {
        let reply = request_with(
            &router,
            Method::GET,
            uri,
            &[("authorization", "Bearer not-a-real-token")],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{uri} must not require auth");
        assert_eq!(reply.body, expected.as_bytes());
    }
}
