//! HTTP server loop with nginx-compatible request-header timeout.
//!
//! `axum::serve` does not expose hyper's `header_read_timeout`, so this module
//! builds the connection itself. It injects `ConnectInfo` per connection so
//! `forwarded::normalize` sees the same peer address axum would provide.

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::Extension;
use axum::Router;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tower::Service;

/// nginx `client_header_timeout` default.
pub const CLIENT_HEADER_TIMEOUT: Duration = Duration::from_secs(60);

/// Serve `app` until the listener fails. `header_timeout` applies to reading
/// the complete request head; a slow client is disconnected.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    header_timeout: Duration,
) -> std::io::Result<()> {
    loop {
        let (stream, remote_addr) = listener.accept().await?;
        let app = app.clone().layer(Extension(ConnectInfo::<SocketAddr>(remote_addr)));
        tokio::spawn(async move {
            if let Err(err) = stream.set_nodelay(true) {
                tracing::debug!(error = %err, "could not set TCP_NODELAY");
            }
            let io = TokioIo::new(stream);
            let mut builder = auto::Builder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(header_timeout);
            let service = service_fn(move |req: hyper::Request<Incoming>| {
                let mut app = app.clone();
                async move { app.call(req.map(Body::new)).await }
            });
            if let Err(err) = builder.serve_connection_with_upgrades(io, service).await {
                tracing::debug!(error = %err, "client connection closed");
            }
        });
    }
}
