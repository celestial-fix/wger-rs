//! Reverse-proxy behaviour that used to live in `wger-nginx`.
//!
//! Routing precedence, prefix stripping, streaming, WebSocket upgrades, the
//! body limit, and forwarded-header trust all live here so the service can be
//! the only container behind `nginx-proxy`.

use std::collections::HashSet;
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::async_trait;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, Version};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::config::UpstreamUrl;
use crate::forwarded::{self, TrustedProxies};

/// `client_max_body_size 100M;` (nginx megabytes, so MiB).
pub const MAX_REQUEST_BODY: usize = 100 * 1024 * 1024;

const X_ACCEL_BUFFERING: &str = "x-accel-buffering";
const X_FORWARDED_HOST: &str = "x-forwarded-host";

/// nginx-compatible timers for the proxy path.
///
/// `connect` matches `proxy_connect_timeout` (default 60s). `client_header`
/// and `client_body` match `client_header_timeout` and `client_body_timeout`
/// (also 60s by default). `tunnel_idle` is the day-long
/// `proxy_read_timeout`/`proxy_send_timeout` for streaming and WebSocket
/// tunnels, measured from the last byte in either direction.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub connect: Duration,
    pub client_header: Duration,
    pub client_body: Duration,
    pub tunnel_idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(60),
            client_header: Duration::from_secs(60),
            client_body: Duration::from_secs(60),
            tunnel_idle: Duration::from_secs(86_400),
        }
    }
}

/// Which fixed upstream a request belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Django,
    PowerSync,
}

/// Fixed, validated destinations plus the trusted-peer policy.
#[derive(Debug)]
pub struct Proxy {
    django: Option<UpstreamUrl>,
    powersync: Option<UpstreamUrl>,
    trusted: Arc<TrustedProxies>,
    timeouts: Timeouts,
}

impl Proxy {
    pub fn new(
        django: Option<UpstreamUrl>,
        powersync: Option<UpstreamUrl>,
        trusted: Arc<TrustedProxies>,
    ) -> Self {
        Self {
            django,
            powersync,
            trusted,
            timeouts: Timeouts::default(),
        }
    }

    /// Override the nginx-compatible timers (tests use short durations).
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    fn upstream(&self, route: Route) -> Option<&UpstreamUrl> {
        match route {
            Route::Django => self.django.as_ref(),
            Route::PowerSync => self.powersync.as_ref(),
        }
    }

    pub fn django_missing(&self) -> bool {
        self.django.is_none()
    }

    pub fn powersync_missing(&self) -> bool {
        self.powersync.is_none()
    }

    /// Forward one request. The destination is chosen from `route` only; the
    /// request target, `Host`, and `X-Forwarded-Host` are never consulted.
    pub async fn forward(&self, req: Request, route: Route) -> Response {
        let powersync = route == Route::PowerSync;
        let Some(upstream) = self.upstream(route) else {
            return text_error(
                StatusCode::BAD_GATEWAY,
                "Upstream is not configured.",
                powersync,
            );
        };

        let target = match upstream_target(req.uri(), powersync) {
            Ok(target) => target,
            Err(message) => {
                tracing::warn!(%message, "invalid upstream target");
                return text_error(
                    StatusCode::BAD_REQUEST,
                    "Invalid request target.",
                    powersync,
                );
            }
        };

        match forward_to(upstream, req, target, powersync, self.timeouts).await {
            Ok(response) => response,
            Err(message) => {
                tracing::warn!(upstream = %upstream.authority, error = %message, "upstream request failed");
                text_error(StatusCode::BAD_GATEWAY, "Bad Gateway", powersync)
            }
        }
    }
}

/// Build the complete application: local routes first, then the proxy as the
/// fallback. `/api/` requests with a `sessionid` bypass the local router, which
/// is the nginx `map $cookie_sessionid` decision made before routing.
pub fn app(local: Router, proxy: Proxy) -> Router {
    let proxy = Arc::new(proxy);
    local
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&proxy),
            client_body_timeout,
        ))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY))
        .layer(middleware::from_fn(intercept_api_session))
        .layer(middleware::from_fn(normalize_forwarded))
        .layer(Extension(Arc::clone(&proxy)))
        .layer(middleware::from_fn(reject_open_proxy))
        .layer(middleware::from_fn(enforce_content_length))
}

/// `ConnectInfo<SocketAddr>` without rejecting requests that did not come from
/// `axum::serve` (notably in-process tests). A missing peer is untrusted.
#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub Option<std::net::SocketAddr>);

#[async_trait]
impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0),
        ))
    }
}

async fn normalize_forwarded(
    Extension(proxy): Extension<Arc<Proxy>>,
    PeerAddr(peer): PeerAddr,
    mut req: Request,
    next: Next,
) -> Response {
    forwarded::normalize(req.headers_mut(), peer, &proxy.trusted);
    next.run(req).await
}

/// Apply nginx's `client_body_timeout`: each read may wait at most the
/// configured idle time for the next chunk.
async fn client_body_timeout(
    State(proxy): State<Arc<Proxy>>,
    req: Request,
    next: Next,
) -> Response {
    let (parts, body) = req.into_parts();
    let body = Body::new(TimeoutBody::new(body, proxy.timeouts.client_body));
    next.run(Request::from_parts(parts, body)).await
}

async fn intercept_api_session(
    Extension(proxy): Extension<Arc<Proxy>>,
    req: Request,
    next: Next,
) -> Response {
    if req.uri().path().starts_with("/api/") && has_session_cookie(req.headers()) {
        return proxy.forward(req, Route::Django).await;
    }
    next.run(req).await
}

/// Refuse absolute-form targets and `CONNECT`: both would turn the service
/// into an open proxy. Host-header spoofing is harmless because the upstream
/// is fixed, but these targets are not part of the contract at all.
async fn reject_open_proxy(req: Request, next: Next) -> Response {
    let uri = req.uri();
    let absolute = uri.scheme().is_some() || uri.authority().is_some();
    if req.method() == Method::CONNECT || absolute || uri.path() == "*" {
        return text_error(
            StatusCode::BAD_REQUEST,
            "This server does not accept absolute-form or CONNECT requests.",
            false,
        );
    }
    next.run(req).await
}

async fn enforce_content_length(req: Request, next: Next) -> Response {
    if content_length(req.headers()).is_some_and(|length| length > MAX_REQUEST_BODY as u64) {
        return text_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request Entity Too Large",
            false,
        );
    }
    next.run(req).await
}

async fn fallback(Extension(proxy): Extension<Arc<Proxy>>, req: Request) -> Response {
    let path = req.uri().path();
    if path.starts_with("/ps/") {
        return proxy.forward(req, Route::PowerSync).await;
    }
    if path.starts_with("/allauth/app/") || path.starts_with("/api/") {
        // These belong to wger-rs itself. The local router already had its
        // chance; an unmatched path is a 404 and must not fall through to the
        // Django UI (nginx uses `upstream wger` for both prefixes).
        return text_error(StatusCode::NOT_FOUND, "Not Found", false);
    }
    // `/allauth/`, `/`, admin, and every UI page go to Django.
    proxy.forward(req, Route::Django).await
}

fn has_session_cookie(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .any(|(name, value)| name.trim() == "sessionid" && !value.trim().is_empty())
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn upstream_target(uri: &Uri, powersync: bool) -> Result<Uri, String> {
    let mut path = uri.path();
    if path.is_empty() {
        path = "/";
    }
    let path = if powersync {
        // nginx `proxy_pass http://powersync/;` replaces the matched `/ps/`
        // prefix with `/`.
        let rest = path.strip_prefix("/ps").unwrap_or(path);
        if rest.is_empty() {
            "/"
        } else {
            rest
        }
    } else {
        path
    };
    let target = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    };
    target
        .parse()
        .map_err(|_| format!("cannot build upstream URI from {target}"))
}

fn onward(upstream: &UpstreamUrl, req: Request, target: Uri, powersync: bool) -> Request<Body> {
    let (parts, body) = req.into_parts();
    let upgrade = wants_upgrade(&parts.headers);
    let tokens = connection_tokens(&parts.headers);
    let host = parts
        .headers
        .get(header::HOST)
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| HeaderValue::from_str(&upstream.authority).ok());

    let mut headers = HeaderMap::new();
    for (name, value) in parts.headers.iter() {
        if name == header::UPGRADE {
            if !upgrade {
                continue;
            }
        } else if is_hop_by_hop(name) || tokens.contains(name.as_str()) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    if let Some(host) = host {
        headers.insert(header::HOST, host);
    }
    if powersync {
        // nginx intentionally does not send X-Forwarded-Host to PowerSync.
        headers.remove(X_FORWARDED_HOST);
    }
    if upgrade {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    } else {
        headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    }

    let mut request = Request::new(body);
    *request.method_mut() = parts.method;
    *request.uri_mut() = target;
    *request.version_mut() = Version::HTTP_11;
    *request.headers_mut() = headers;
    request
}

async fn forward_to(
    upstream: &UpstreamUrl,
    mut req: Request,
    target: Uri,
    powersync: bool,
    timeouts: Timeouts,
) -> Result<Response, String> {
    let upgrade = wants_upgrade(req.headers());
    let client_upgrade = extract_on_upgrade(&mut req).filter(|_| upgrade);
    let request = onward(upstream, req, target, powersync);

    let stream = match tokio::time::timeout(
        timeouts.connect,
        TcpStream::connect((upstream.host.as_str(), upstream.port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => return Err(format!("connect {}: {err}", upstream.authority)),
        Err(_) => {
            tracing::warn!(
                upstream = %upstream.authority,
                "upstream connect timed out"
            );
            return Ok(text_error(
                StatusCode::GATEWAY_TIMEOUT,
                "Gateway Timeout",
                powersync,
            ));
        }
    };
    let _ = stream.set_nodelay(true);
    let io = TokioIo::new(stream);
    let (mut sender, conn) = http1::handshake(io)
        .await
        .map_err(|err| format!("handshake {}: {err}", upstream.authority))?;
    let overflow = Arc::new(AtomicBool::new(false));
    let request = with_limit(request, Arc::clone(&overflow));

    if upgrade {
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
    } else {
        tokio::spawn(async move {
            let _ = conn.await;
        });
    }

    let result = sender.send_request(request).await;
    if overflow.load(Ordering::SeqCst) {
        return Ok(text_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request Entity Too Large",
            powersync,
        ));
    }
    let upstream_response = result.map_err(|err| format!("request: {err}"))?;

    if upstream_response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let mut upstream_response = upstream_response;
        let upstream_upgrade = hyper::upgrade::on(&mut upstream_response);
        let response = relay(upstream_response, powersync);
        if let Some(client_upgrade) = client_upgrade {
            tokio::spawn(tunnel(
                client_upgrade,
                upstream_upgrade,
                timeouts.tunnel_idle,
            ));
        }
        return Ok(response);
    }

    Ok(relay(upstream_response, powersync))
}

fn extract_on_upgrade(req: &mut Request) -> Option<OnUpgrade> {
    req.extensions_mut().remove::<OnUpgrade>()
}

async fn tunnel(client: OnUpgrade, upstream: OnUpgrade, idle: Duration) {
    match tokio::try_join!(client, upstream) {
        Ok((client, upstream)) => {
            let activity = Arc::new(Activity::new());
            let mut client = ActiveIo::new(TokioIo::new(client), Arc::clone(&activity));
            let mut upstream = ActiveIo::new(TokioIo::new(upstream), Arc::clone(&activity));
            tokio::select! {
                result = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {
                    if let Err(err) = result {
                        tracing::debug!(error = %err, "WebSocket tunnel closed with an error");
                    }
                }
                () = idle_watchdog(activity, idle) => {
                    tracing::debug!(
                        idle_seconds = idle.as_secs(),
                        "WebSocket tunnel closed after idle timeout"
                    );
                }
            }
        }
        Err(err) => tracing::debug!(error = %err, "WebSocket upgrade failed"),
    }
}

/// Last time a byte moved in either direction through a tunnel.
struct Activity {
    last: Mutex<tokio::time::Instant>,
}

impl Activity {
    fn new() -> Self {
        Self {
            last: Mutex::new(tokio::time::Instant::now()),
        }
    }

    fn touch(&self) {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        *last = tokio::time::Instant::now();
    }

    fn touched_since(&self, since: tokio::time::Instant) -> bool {
        let last = *self.last.lock().unwrap_or_else(|e| e.into_inner());
        last > since
    }

    fn last(&self) -> tokio::time::Instant {
        *self.last.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Returns once no byte has moved for `idle`. Sleeps until the current
/// deadline and extends it whenever traffic arrived in the meantime.
async fn idle_watchdog(activity: Arc<Activity>, idle: Duration) {
    loop {
        let last = activity.last();
        tokio::time::sleep_until(last + idle).await;
        if activity.touched_since(last) {
            continue;
        }
        return;
    }
}

/// An IO wrapper that records reads and writes so the tunnel watchdog can tell
/// whether the connection is idle.
struct ActiveIo<T> {
    inner: T,
    activity: Arc<Activity>,
}

impl<T> ActiveIo<T> {
    fn new(inner: T, activity: Arc<Activity>) -> Self {
        Self { inner, activity }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ActiveIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.activity.touch();
        }
        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ActiveIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = result {
            if written > 0 {
                this.activity.touch();
            }
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(written)) = result {
            if written > 0 {
                this.activity.touch();
            }
        }
        result
    }
}

fn relay(response: hyper::Response<Incoming>, powersync: bool) -> Response {
    let status = response.status();
    let (parts, body) = response.into_parts();
    let headers = response_headers(&parts.headers, status);
    let body = if status == StatusCode::SWITCHING_PROTOCOLS {
        Body::empty()
    } else {
        Body::new(body)
    };
    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    if powersync {
        insert_static(out.headers_mut(), X_ACCEL_BUFFERING, "no");
    }
    out
}

fn response_headers(input: &HeaderMap, status: StatusCode) -> HeaderMap {
    let switching = status == StatusCode::SWITCHING_PROTOCOLS;
    let tokens = connection_tokens(input);
    let mut headers = HeaderMap::new();
    for (name, value) in input.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        if name == header::UPGRADE || name == header::CONNECTION {
            if !switching {
                continue;
            }
        } else if tokens.contains(name.as_str()) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    if switching {
        if let Some(upgrade) = input.get(header::UPGRADE) {
            headers.insert(header::UPGRADE, upgrade.clone());
        }
        insert_static(&mut headers, "connection", "upgrade");
    }
    headers
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn connection_tokens(headers: &HeaderMap) -> HashSet<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

fn wants_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
        && connection_tokens(headers)
            .iter()
            .any(|token| token == "upgrade")
}

fn with_limit(request: Request<Body>, overflow: Arc<AtomicBool>) -> Request<LimitedBody> {
    request.map(|body| LimitedBody {
        inner: body,
        remaining: MAX_REQUEST_BODY as u64,
        overflow,
    })
}

fn text_error(status: StatusCode, message: &'static str, powersync: bool) -> Response {
    let mut response = (status, message).into_response();
    if powersync {
        insert_static(response.headers_mut(), X_ACCEL_BUFFERING, "no");
    }
    response
}

fn insert_static(headers: &mut HeaderMap, name: &'static str, value: &'static str) {
    headers.insert(
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    );
}

type BoxError = Box<dyn StdError + Send + Sync>;

/// Fails a request body read that waits too long for the next frame. The
/// deadline restarts after every frame, which is the per-chunk idle behaviour
/// of nginx's `client_body_timeout`.
struct TimeoutBody {
    inner: Body,
    timeout: Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl TimeoutBody {
    fn new(inner: Body, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            sleep: None,
        }
    }
}

impl HttpBody for TimeoutBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(frame) => {
                this.sleep = None;
                Poll::Ready(frame.map(|result| result.map_err(|err| Box::new(err) as BoxError)))
            }
            Poll::Pending => {
                let timeout = this.timeout;
                let sleep = this
                    .sleep
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
                if sleep.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Some(Err("client request body timed out".into())));
                }
                Poll::Pending
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Streams the request body but fails once it exceeds the nginx limit. The
/// shared flag lets the caller answer 413 instead of surfacing a 502.
struct LimitedBody {
    inner: Body,
    remaining: u64,
    overflow: Arc<AtomicBool>,
}

impl HttpBody for LimitedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let len = data.len() as u64;
                    if len > this.remaining {
                        this.overflow.store(true, Ordering::SeqCst);
                        return Poll::Ready(Some(Err("request body exceeds 100 MiB".into())));
                    }
                    this.remaining -= len;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(Box::new(err)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cookie_split_requires_a_non_empty_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("csrftoken=abc; sessionid=deadbeef"),
        );
        assert!(has_session_cookie(&headers));

        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("csrftoken=abc; sessionid="),
        );
        assert!(!has_session_cookie(&headers));

        headers.insert(header::COOKIE, HeaderValue::from_static("other=1"));
        assert!(!has_session_cookie(&headers));
    }

    #[test]
    fn powersync_prefix_is_stripped_like_proxy_pass() {
        let cases = [
            ("/ps/sync/stream", "/sync/stream"),
            ("/ps/write-checkpoint2.json", "/write-checkpoint2.json"),
            ("/ps/probes/liveness", "/probes/liveness"),
            ("/ps/", "/"),
        ];
        for (input, expected) in cases {
            let uri: Uri = input.parse().unwrap();
            let target = upstream_target(&uri, true).unwrap();
            assert_eq!(target.path(), expected, "{input}");
        }
    }

    #[test]
    fn query_strings_and_django_paths_are_preserved() {
        let uri: Uri = "/api/v2/routine/?limit=10".parse().unwrap();
        let target = upstream_target(&uri, false).unwrap();
        assert_eq!(target.to_string(), "/api/v2/routine/?limit=10");
    }

    #[test]
    fn absolute_form_is_detected_by_uri_parts() {
        let uri: Uri = "http://evil.example/api/v2/x".parse().unwrap();
        assert!(uri.scheme().is_some() && uri.authority().is_some());
    }

    #[test]
    fn upgrade_requires_both_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(!wants_upgrade(&headers));
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        assert!(wants_upgrade(&headers));
    }

    #[test]
    fn timeouts_match_nginx_defaults() {
        let timeouts = Timeouts::default();
        assert_eq!(timeouts.connect, Duration::from_secs(60));
        assert_eq!(timeouts.client_header, Duration::from_secs(60));
        assert_eq!(timeouts.client_body, Duration::from_secs(60));
        assert_eq!(timeouts.tunnel_idle, Duration::from_secs(86_400));
    }
}
