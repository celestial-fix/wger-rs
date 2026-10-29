//! `wger-rs healthcheck` / `wger-rs --healthcheck`.
//!
//! The scratch runtime image has no shell and no `wget`, so the Docker
//! `HEALTHCHECK` calls this subcommand instead. It reads the port from `BIND`
//! and probes `http://127.0.0.1:<port>/healthz`, exiting 0 on 200 and 1 on
//! anything else. It never loads the database or JWT configuration.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DEFAULT_BIND: &str = "0.0.0.0:8000";
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Probe using `BIND` from the environment (default `0.0.0.0:8000`).
pub async fn run() -> Result<(), String> {
    let bind = std::env::var("BIND")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BIND.to_string());
    let port = port_from_bind(&bind)?;
    check(port).await
}

/// Extract the listening port from a `host:port` bind address.
pub fn port_from_bind(bind: &str) -> Result<u16, String> {
    bind.parse::<SocketAddr>()
        .map(|addr| addr.port())
        .map_err(|_| "BIND is not a host:port socket address".to_string())
}

/// Probe the local server on `port`.
pub async fn check(port: u16) -> Result<(), String> {
    check_addr(SocketAddr::from(([127, 0, 0, 1], port)), PROBE_TIMEOUT).await
}

/// Probe `/healthz` at an explicit address. Split out so tests can use an
/// ephemeral port and a short timeout.
pub async fn check_addr(addr: SocketAddr, timeout: Duration) -> Result<(), String> {
    let probe = async {
        let mut stream = TcpStream::connect(addr)
            .await
            .map_err(|err| format!("connect {addr}: {err}"))?;
        let request = "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|err| format!("write: {err}"))?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(|err| format!("read: {err}"))?;
        let status = String::from_utf8_lossy(&response);
        let code = status
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .ok_or_else(|| "response has no status line".to_string())?;
        if code == "200" {
            Ok(())
        } else {
            Err(format!("healthz returned {code}"))
        }
    };
    tokio::time::timeout(timeout, probe)
        .await
        .map_err(|_| "healthz probe timed out".to_string())?
}
