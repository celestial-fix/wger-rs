//! Proxy contract: routing precedence, prefix stripping, streaming, upgrades,
//! the body limit, and forwarded-header trust. All upstreams are in-process
//! loopback servers; nothing here needs the network.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use http_body::Frame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;
use wger_rs::config::UpstreamUrl;
use wger_rs::forwarded::{IpNet, TrustedProxies};
use wger_rs::proxy::{self, Proxy, Timeouts};

const BODY_LIMIT: usize = 100 * 1024 * 1024;

#[derive(Clone, Debug)]
struct Captured {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    _body: Vec<u8>,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Clone)]
enum Reply {
    Static {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Chunks {
        chunks: Vec<(u64, Vec<u8>)>,
    },
    Upgrade,
    /// Read and discard the request until the peer closes.
    Sink,
}

impl Reply {
    fn ok(body: &str) -> Self {
        Self::Static {
            status: 200,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: body.as_bytes().to_vec(),
        }
    }
}

struct Fake {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Captured>>>,
    _task: tokio::task::JoinHandle<()>,
}

impl Fake {
    fn url(&self) -> UpstreamUrl {
        UpstreamUrl::parse("TEST_UPSTREAM_URL", &format!("http://{}", self.addr)).unwrap()
    }

    fn seen(&self) -> Vec<Captured> {
        self.seen.lock().unwrap().clone()
    }
}

async fn fake_upstream(reply: Reply) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_task = Arc::clone(&seen);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let reply = reply.clone();
            let seen = Arc::clone(&seen_task);
            tokio::spawn(async move {
                let sink = matches!(reply, Reply::Sink);
                let mut is_websocket = false;
                if let Some(captured) = read_request(&mut socket, sink).await {
                    is_websocket = captured
                        .header("upgrade")
                        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
                        && captured
                            .header("connection")
                            .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
                    seen.lock().unwrap().push(captured);
                }
                match reply {
                    Reply::Static {
                        status,
                        headers,
                        body,
                    } => {
                        let mut out = format!("HTTP/1.1 {status} X\r\n");
                        for (name, value) in headers {
                            out.push_str(&format!("{name}: {value}\r\n"));
                        }
                        out.push_str(&format!(
                            "Content-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        ));
                        let _ = socket.write_all(out.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                    }
                    Reply::Chunks { chunks } => {
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.powersync.bson-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                            )
                            .await;
                        for (delay_ms, chunk) in chunks {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            let _ = socket
                                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                                .await;
                            let _ = socket.write_all(&chunk).await;
                            let _ = socket.write_all(b"\r\n").await;
                            let _ = socket.flush().await;
                        }
                        let _ = socket.write_all(b"0\r\n\r\n").await;
                    }
                    Reply::Upgrade if !is_websocket => {
                        // Like the real PowerSync/Django: no upgrade headers,
                        // no WebSocket.
                        let _ = socket
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                            .await;
                    }
                    Reply::Upgrade => {
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
                            )
                            .await;
                        let mut buffer = [0u8; 4096];
                        loop {
                            match socket.read(&mut buffer).await {
                                Ok(0) | Err(_) => break,
                                Ok(read) => {
                                    if socket.write_all(&buffer[..read]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Reply::Sink => {
                        let mut buffer = [0u8; 64 * 1024];
                        loop {
                            match socket.read(&mut buffer).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    }
                }
            });
        }
    });
    Fake {
        addr,
        seen,
        _task: task,
    }
}

async fn read_request(stream: &mut TcpStream, sink: bool) -> Option<Captured> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    if sink {
        // Consume whatever remains (the body may be chunked or oversized).
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        return Some(Captured {
            method,
            target,
            headers,
            _body: Vec::new(),
        });
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(Captured {
        method,
        target,
        headers,
        _body: body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn local_router() -> Router {
    Router::new()
        .route("/healthz", get(|| async { "local-health" }))
        .route("/api/v2/local/", get(|| async { "local-api" }))
        .route(
            "/allauth/app/v1/auth/login",
            post(|| async { "local-auth" }),
        )
}

fn test_app(
    django: Option<UpstreamUrl>,
    powersync: Option<UpstreamUrl>,
    trusted: Vec<IpNet>,
) -> Router {
    test_app_with_timeouts(django, powersync, trusted, Timeouts::default())
}

fn test_app_with_timeouts(
    django: Option<UpstreamUrl>,
    powersync: Option<UpstreamUrl>,
    trusted: Vec<IpNet>,
    timeouts: Timeouts,
) -> Router {
    let trusted = Arc::new(TrustedProxies::from_networks(trusted));
    proxy::app(
        local_router(),
        Proxy::new(django, powersync, trusted).with_timeouts(timeouts),
    )
}

fn request(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    let has_host = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"));
    if !has_host {
        builder = builder.header("host", "localhost");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).unwrap()
}

fn from_trusted(mut req: Request<Body>, ip: &str) -> Request<Body> {
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(ip.parse().unwrap(), 40000)));
    req
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn routing_matches_nginx_precedence() {
    let django = fake_upstream(Reply::ok("django")).await;
    let powersync = fake_upstream(Reply::ok("powersync")).await;
    let app = test_app(Some(django.url()), Some(powersync.url()), Vec::new());

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/ps/sync/stream", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "powersync");
    let seen = powersync.seen();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].target, "/sync/stream");
    assert_eq!(seen[0].header("host"), Some("localhost"));
    assert_eq!(seen[0].header("x-forwarded-host"), None);

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/ps/write-checkpoint2.json", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(powersync.seen()[1].target, "/write-checkpoint2.json");

    for path in ["/", "/dashboard/", "/allauth/login/"] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path, &[]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(body_text(response).await, "django", "{path}");
    }
    let django_seen = django.seen();
    assert_eq!(django_seen.len(), 3);
    assert_eq!(django_seen[0].target, "/");
    assert_eq!(django_seen[1].target, "/dashboard/");
    assert_eq!(django_seen[2].target, "/allauth/login/");

    // Local routes win for /allauth/app/ and for cookie-less /api/.
    for (path, expected) in [
        ("/healthz", "local-health"),
        ("/api/v2/local/", "local-api"),
    ] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path, &[]))
            .await
            .unwrap();
        assert_eq!(body_text(response).await, expected, "{path}");
    }
    let response = app
        .clone()
        .oneshot(request(Method::POST, "/allauth/app/v1/auth/login", &[]))
        .await
        .unwrap();
    assert_eq!(body_text(response).await, "local-auth");
    assert_eq!(django.seen().len(), 3, "local routes must not be proxied");

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/allauth/app/unknown", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app
        .clone()
        .oneshot(request(Method::GET, "/api/v2/missing/", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(django.seen().len(), 3, "unknown API paths are local 404s");
}

#[tokio::test]
async fn session_cookie_selects_django_for_api_requests() {
    let django = fake_upstream(Reply::ok("django")).await;
    let app = test_app(Some(django.url()), None, Vec::new());

    let response = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v2/local/",
            &[("cookie", "csrftoken=x; sessionid=abc123")],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "django");
    assert_eq!(django.seen()[0].target, "/api/v2/local/");

    let response = app
        .oneshot(request(
            Method::GET,
            "/api/v2/local/",
            &[("cookie", "sessionid=")],
        ))
        .await
        .unwrap();
    assert_eq!(body_text(response).await, "local-api");
    assert_eq!(django.seen().len(), 1);
}

#[tokio::test]
async fn healthz_is_200_even_when_django_is_a_dead_port() {
    let dead = UpstreamUrl::parse("DJANGO_UI_URL", "http://127.0.0.1:1").unwrap();
    let app = test_app(Some(dead), None, Vec::new());

    let response = app
        .oneshot(request(Method::GET, "/healthz", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "local-health");
}

#[tokio::test]
async fn missing_upstreams_are_502_and_ps_still_sets_accel_header() {
    let app = test_app(None, None, Vec::new());

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/dashboard/", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(response.headers().get("x-accel-buffering"), None);

    let response = app
        .oneshot(request(Method::GET, "/ps/probes/liveness", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(response.headers().get("x-accel-buffering").unwrap(), "no");
}

#[tokio::test]
async fn host_spoofing_cannot_redirect_the_upstream() {
    let django = fake_upstream(Reply::ok("django")).await;
    let app = test_app(Some(django.url()), None, Vec::new());

    let response = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/dashboard/",
            &[("host", "evil.example")],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(django.seen()[0].header("host"), Some("evil.example"));

    for request in [
        request(Method::GET, "http://evil.example/dashboard/", &[]),
        request(Method::CONNECT, "evil.example:443", &[]),
    ] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(django.seen().len(), 1, "rejected targets must not connect");
}

#[tokio::test]
async fn trusted_peer_keeps_the_proxy_chain_and_untrusted_peer_is_overwritten() {
    let upstream = fake_upstream(Reply::ok("ok")).await;
    let trusted = vec![IpNet::parse("127.0.0.1").unwrap()];
    let app = test_app(Some(upstream.url()), None, trusted);

    let req = from_trusted(
        request(
            Method::GET,
            "/dashboard/",
            &[
                ("host", "fitness.example.test"),
                ("x-forwarded-for", "203.0.113.9"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-host", "evil.example"),
                ("x-real-ip", "203.0.113.9"),
            ],
        ),
        "127.0.0.1",
    );
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let seen = upstream.seen();
    assert_eq!(
        seen[0].header("x-forwarded-for"),
        Some("203.0.113.9, 127.0.0.1")
    );
    assert_eq!(seen[0].header("x-real-ip"), Some("203.0.113.9"));
    assert_eq!(seen[0].header("x-forwarded-proto"), Some("https"));
    assert_eq!(
        seen[0].header("x-forwarded-host"),
        Some("fitness.example.test")
    );

    let req = from_trusted(
        request(
            Method::GET,
            "/dashboard/",
            &[
                ("host", "fitness.example.test"),
                ("x-forwarded-for", "203.0.113.9"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-host", "evil.example"),
                ("x-real-ip", "203.0.113.9"),
            ],
        ),
        "198.51.100.5",
    );
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let seen = upstream.seen();
    assert_eq!(seen[1].header("x-forwarded-for"), Some("198.51.100.5"));
    assert_eq!(seen[1].header("x-real-ip"), Some("198.51.100.5"));
    assert_eq!(seen[1].header("x-forwarded-proto"), Some("http"));
    assert_eq!(
        seen[1].header("x-forwarded-host"),
        Some("fitness.example.test")
    );

    // nginx-proxy appends the real client to any X-Forwarded-For the client
    // sent. Walking from the right must ignore the spoofed 1.2.3.4.
    let req = from_trusted(
        request(
            Method::GET,
            "/dashboard/",
            &[
                ("host", "fitness.example.test"),
                ("x-forwarded-for", "1.2.3.4, 203.0.113.9"),
                ("x-forwarded-proto", "https"),
            ],
        ),
        "127.0.0.1",
    );
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let seen = upstream.seen();
    assert_eq!(
        seen[2].header("x-forwarded-for"),
        Some("1.2.3.4, 203.0.113.9, 127.0.0.1")
    );
    assert_eq!(seen[2].header("x-real-ip"), Some("203.0.113.9"));
}

#[tokio::test]
async fn cookies_and_location_pass_through_untouched() {
    let upstream = fake_upstream(Reply::Static {
        status: 302,
        headers: vec![
            ("Location".into(), "/accounts/login/?next=%2F".into()),
            (
                "Set-Cookie".into(),
                "csrftoken=abc; Secure; HttpOnly; SameSite=Lax".into(),
            ),
            (
                "Set-Cookie".into(),
                "sessionid=def; Secure; HttpOnly; SameSite=Lax; Path=/".into(),
            ),
        ],
        body: Vec::new(),
    })
    .await;
    let app = test_app(Some(upstream.url()), None, Vec::new());

    let response = app
        .oneshot(request(Method::GET, "/dashboard/", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response.headers().get(header::LOCATION).unwrap(),
        "/accounts/login/?next=%2F"
    );
    let cookies: Vec<_> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_string())
        .collect();
    assert_eq!(cookies.len(), 2);
    assert_eq!(cookies[0], "csrftoken=abc; Secure; HttpOnly; SameSite=Lax");
    assert_eq!(
        cookies[1],
        "sessionid=def; Secure; HttpOnly; SameSite=Lax; Path=/"
    );
}

#[tokio::test]
async fn oversized_content_length_is_rejected_before_connecting() {
    let upstream = fake_upstream(Reply::ok("ok")).await;
    let app = test_app(Some(upstream.url()), Some(upstream.url()), Vec::new());

    for path in ["/dashboard/", "/api/v2/local/", "/ps/sync/stream"] {
        let response = app
            .clone()
            .oneshot(request(
                Method::POST,
                path,
                &[("content-length", &(BODY_LIMIT + 1).to_string())],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{path}");
    }
    assert!(upstream.seen().is_empty());
}

struct Chunky {
    sent: usize,
    stop_after: usize,
    chunk: usize,
}

impl http_body::Body for Chunky {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.sent >= this.stop_after {
            return Poll::Ready(None);
        }
        let len = this.chunk;
        this.sent += len;
        Poll::Ready(Some(Ok(Frame::data(Bytes::from(vec![b'x'; len])))))
    }
}

#[tokio::test]
async fn streamed_overflow_is_413_on_a_proxied_route() {
    let upstream = fake_upstream(Reply::Sink).await;
    let app = test_app(Some(upstream.url()), None, Vec::new());

    let request = Request::builder()
        .method(Method::POST)
        .uri("/dashboard/")
        .body(Body::new(Chunky {
            sent: 0,
            stop_after: BODY_LIMIT + 65_536,
            chunk: 1024 * 1024,
        }))
        .unwrap();

    let response = tokio::time::timeout(Duration::from_secs(30), app.oneshot(request))
        .await
        .expect("proxy must finish the oversized stream")
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

struct Stalled;

impl http_body::Body for Stalled {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Pending
    }
}

#[tokio::test]
async fn stalled_request_body_is_timed_out() {
    let upstream = fake_upstream(Reply::Sink).await;
    let timeouts = Timeouts {
        client_body: Duration::from_millis(100),
        ..Timeouts::default()
    };
    let app = test_app_with_timeouts(Some(upstream.url()), None, Vec::new(), timeouts);

    let request = Request::builder()
        .method(Method::POST)
        .uri("/dashboard/")
        .header("content-length", "10")
        .body(Body::new(Stalled))
        .unwrap();

    let response = tokio::time::timeout(Duration::from_secs(3), app.oneshot(request))
        .await
        .expect("client_body_timeout must abort the request")
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn static_files_take_precedence_and_missing_files_are_not_proxied() {
    let root = std::env::temp_dir().join(format!(
        "wger-rs-proxy-static-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("static")).unwrap();
    std::fs::create_dir_all(root.join("media")).unwrap();
    std::fs::write(root.join("static/app.80a6279921f8.css"), b"body{}").unwrap();

    let django = fake_upstream(Reply::ok("django")).await;
    let trusted = Arc::new(TrustedProxies::from_networks(Vec::new()));
    let app = proxy::app(
        local_router().merge(wger_rs::static_files::router(
            &root.join("static"),
            &root.join("media"),
        )),
        Proxy::new(Some(django.url()), None, trusted),
    );

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/static/app.80a6279921f8.css", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "body{}");

    let response = app
        .oneshot(request(Method::GET, "/static/missing.css", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(django.seen().is_empty());

    let _ = std::fs::remove_dir_all(root);
}

async fn serve(app: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    serve_with_header_timeout(app, wger_rs::server::CLIENT_HEADER_TIMEOUT).await
}

async fn serve_with_header_timeout(
    app: Router,
    header_timeout: Duration,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        wger_rs::server::serve(listener, app, header_timeout)
            .await
            .unwrap();
    });
    (addr, handle)
}

async fn raw_request(addr: SocketAddr, request: &str) -> (String, Vec<u8>, TcpStream) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "connection closed before response headers");
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let leftover = buffer[end..].to_vec();
    (head, leftover, stream)
}

#[tokio::test]
async fn websocket_upgrade_passes_through_ps_and_django() {
    let powersync = fake_upstream(Reply::Upgrade).await;
    let django = fake_upstream(Reply::Upgrade).await;
    let app = test_app(Some(django.url()), Some(powersync.url()), Vec::new());
    let (addr, server) = serve(app).await;

    for path in ["/ps/sync/stream", "/ws/chat"] {
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        );
        let (head, _leftover, mut stream) = raw_request(addr, &request).await;
        assert!(
            head.starts_with("HTTP/1.1 101 Switching Protocols"),
            "{path}: {head}"
        );
        assert!(head.to_ascii_lowercase().contains("upgrade: websocket"));

        stream.write_all(b"ping-through-proxy").await.unwrap();
        let mut echoed = [0u8; 18];
        stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping-through-proxy", "{path}");
    }

    server.abort();
}

#[tokio::test]
async fn slow_client_request_headers_are_closed() {
    let app = test_app(None, None, Vec::new());
    let (addr, server) = serve_with_header_timeout(app, Duration::from_millis(100)).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .unwrap();

    let mut buffer = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buffer))
        .await
        .expect("client_header_timeout must close the connection")
        .unwrap();
    assert!(
        !buffer.starts_with(b"HTTP/1.1 200"),
        "a partial request must not be answered successfully"
    );

    server.abort();
}

#[tokio::test]
async fn idle_websocket_tunnel_is_closed() {
    let powersync = fake_upstream(Reply::Upgrade).await;
    let timeouts = Timeouts {
        tunnel_idle: Duration::from_millis(100),
        ..Timeouts::default()
    };
    let app = test_app_with_timeouts(None, Some(powersync.url()), Vec::new(), timeouts);
    let (addr, server) = serve(app).await;

    let request = "GET /ps/sync/stream HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
    let (head, _leftover, mut stream) = raw_request(addr, request).await;
    assert!(
        head.starts_with("HTTP/1.1 101 Switching Protocols"),
        "{head}"
    );

    let mut buffer = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buffer))
        .await
        .expect("an idle WebSocket tunnel must be closed")
        .unwrap();
    assert_eq!(read, 0, "expected EOF from the closed tunnel");

    server.abort();
}

#[tokio::test]
async fn powersync_streaming_flushes_the_first_chunk_immediately() {
    let powersync = fake_upstream(Reply::Chunks {
        chunks: vec![
            (0, b"checkpoint-1\n".to_vec()),
            (1500, b"checkpoint-2\n".to_vec()),
        ],
    })
    .await;
    let app = test_app(None, Some(powersync.url()), Vec::new());
    let (addr, server) = serve(app).await;

    let request = "POST /ps/sync/stream HTTP/1.1\r\nHost: localhost\r\nAccept: application/vnd.powersync.bson-stream;q=0.9,application/x-ndjson;q=0.8\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (head, leftover, mut stream) = raw_request(addr, request).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("x-accel-buffering: no"), "{head}");
    assert!(!lower.contains("content-length:"), "{head}");

    let mut first = leftover;
    let mut chunk = [0u8; 1024];
    let read = tokio::time::timeout(Duration::from_millis(700), stream.read(&mut chunk))
        .await
        .expect("first chunk must arrive before the upstream finishes")
        .unwrap();
    first.extend_from_slice(&chunk[..read]);
    assert!(
        String::from_utf8_lossy(&first).contains("checkpoint-1"),
        "first read: {first:?}"
    );

    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&first),
        String::from_utf8_lossy(&rest)
    );
    assert!(all.contains("checkpoint-2"), "{all}");

    server.abort();
}

#[tokio::test]
async fn every_ps_response_has_the_accel_header() {
    let powersync = fake_upstream(Reply::ok("ok")).await;
    let app = test_app(None, Some(powersync.url()), Vec::new());

    for path in ["/ps/probes/liveness", "/ps/write-checkpoint2.json"] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path, &[]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers().get("x-accel-buffering").unwrap(),
            "no",
            "{path}"
        );
    }
}

#[tokio::test]
async fn absolute_target_is_rejected_before_the_body_limit_counter_matters() {
    // A quick sanity check that in-process requests without ConnectInfo are
    // treated as untrusted rather than panicking on the extractor.
    let upstream = fake_upstream(Reply::ok("ok")).await;
    let app = test_app(Some(upstream.url()), None, Vec::new());
    let response = app
        .oneshot(request(Method::GET, "/dashboard/", &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(upstream.seen()[0].header("x-forwarded-proto"), Some("http"));
}
