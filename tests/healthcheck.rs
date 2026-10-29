//! Integration tests for the `healthcheck` subcommand used by the scratch image.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_wger-rs");

/// Bind and immediately release a loopback port. Nothing listens there after.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Serve exactly one canned HTTP response, then stop.
fn one_shot_server(status_line: &'static str) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept probe");
        let mut buffer = [0u8; 1024];
        let _ = stream.read(&mut buffer);
        stream
            .write_all(
                format!("{status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .expect("write response");
    });
    (port, handle)
}

/// Serve one canned 200 response and return the request bytes the probe sent.
fn recording_one_shot_server() -> (u16, std::thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept probe");
        let mut buffer = [0u8; 1024];
        let read = stream.read(&mut buffer).expect("read request");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("write response");
        buffer[..read].to_vec()
    });
    (port, handle)
}

fn healthcheck(bind: &str, arg: &str) -> std::process::Output {
    Command::new(BIN)
        .arg(arg)
        .env("BIND", bind)
        // The probe must not need application configuration.
        .env_remove("DATABASE_URL")
        .env_remove("JWT_PRIVATE_KEY")
        .env_remove("JWT_PUBLIC_KEY")
        .output()
        .expect("run wger-rs healthcheck")
}

#[test]
fn healthcheck_fails_cleanly_when_nothing_is_listening() {
    let output = healthcheck(&format!("127.0.0.1:{}", closed_port()), "healthcheck");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("healthcheck:"), "stderr: {stderr}");
}

#[test]
fn healthcheck_fails_on_a_non_200_healthz() {
    let (port, server) = one_shot_server("HTTP/1.1 503 Service Unavailable");
    let output = healthcheck(&format!("0.0.0.0:{port}"), "healthcheck");
    server.join().expect("server thread");

    assert!(!output.status.success());
}

#[test]
fn healthcheck_succeeds_against_a_healthy_server() {
    let (port, server) = one_shot_server("HTTP/1.1 200 OK");
    let output = healthcheck(&format!("0.0.0.0:{port}"), "--healthcheck");
    server.join().expect("server thread");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn healthcheck_probes_the_healthz_path() {
    let (port, server) = recording_one_shot_server();
    let output = healthcheck(&format!("127.0.0.1:{port}"), "healthcheck");
    let request = String::from_utf8_lossy(&server.join().expect("server thread")).into_owned();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        request.starts_with("GET /healthz HTTP/1.1\r\n"),
        "request: {request:?}"
    );
}

#[test]
fn healthcheck_rejects_a_bind_without_a_port() {
    let output = healthcheck("not-a-socket-address", "healthcheck");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("BIND is not a host:port"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
