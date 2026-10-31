#!/usr/bin/env python3
"""Production-like login and PowerSync transport checks through wger-rs.

Standard library only. All requests go to the local wger-rs proxy; the proxy
talks to the harness Django and the throwaway PowerSync service.
"""

import argparse
import base64
import http.client
import re
import socket
import sys
import time
from urllib.parse import urlencode

PASS = []
FAIL = []


def ok(message):
    PASS.append(message)
    print(f"PASS {message}", flush=True)


def fail(message):
    FAIL.append(message)
    print(f"FAIL {message}", flush=True)


def check(condition, message):
    if condition:
        ok(message)
    else:
        fail(message)
    return condition


def split_base(base):
    if not base.startswith("http://"):
        raise SystemExit(f"only http:// bases are supported: {base}")
    hostport = base[len("http://") :].split("/", 1)[0]
    host, port = hostport.rsplit(":", 1)
    return host, int(port)


def headers_https(public_host, extra=None):
    headers = {
        "Host": public_host,
        "X-Forwarded-Proto": "https",
        "X-Forwarded-Host": public_host,
        "X-Real-IP": "203.0.113.7",
        "X-Forwarded-For": "203.0.113.7",
        "Accept": "text/html",
    }
    if extra:
        headers.update(extra)
    return headers


def parse_set_cookies(message):
    cookies = {}
    for raw in message.get_all("Set-Cookie") or []:
        first, *attributes = [part.strip() for part in raw.split(";")]
        if "=" not in first:
            continue
        name, value = first.split("=", 1)
        cookies.setdefault(name, []).append((value, attributes))
    return cookies


def cookie_has(cookies, name, attribute):
    entries = cookies.get(name)
    if not entries:
        return False
    return all(attribute.lower() in [a.lower() for a in attrs] for _, attrs in entries)


def login_page(host, port, public_host):
    conn = http.client.HTTPConnection(host, port, timeout=30)
    conn.request("GET", "/en/user/login", headers=headers_https(public_host))
    response = conn.getresponse()
    body = response.read()
    status = response.status
    cookies = parse_set_cookies(response.msg)
    conn.close()
    return status, body, cookies


def hidden_csrf(body):
    text = body.decode("utf-8", "replace")
    match = re.search(
        r"name=[\"']csrfmiddlewaretoken[\"']\s+value=[\"']([^\"']+)[\"']", text
    )
    if match:
        return match.group(1)
    match = re.search(
        r"value=[\"']([^\"']+)[\"']\s+name=[\"']csrfmiddlewaretoken[\"']", text
    )
    return match.group(1) if match else None


def login_post(host, port, public_host, csrf, origin, user, password):
    conn = http.client.HTTPConnection(host, port, timeout=30)
    body = urlencode(
        {
            "csrfmiddlewaretoken": csrf,
            "login": user,
            "password": password,
        }
    )
    headers = headers_https(
        public_host,
        {
            "Origin": origin,
            "Referer": f"https://{public_host}/en/user/login",
            "Content-Type": "application/x-www-form-urlencoded",
            "Cookie": f"csrftoken={csrf}",
        },
    )
    headers.pop("X-Forwarded-Host", None)
    headers["X-Forwarded-Host"] = public_host
    conn.request("POST", "/en/user/login", body=body, headers=headers)
    response = conn.getresponse()
    response.read()
    status = response.status
    cookies = parse_set_cookies(response.msg)
    location = response.getheader("Location")
    conn.close()
    return status, location, cookies


def check_login(base, public_host, user, password):
    host, port = split_base(base)

    status, body, cookies = login_page(host, port, public_host)
    check(status == 200, f"GET /en/user/login returns 200 (got {status})")
    check("csrftoken" in cookies, "login page sets a csrftoken cookie")
    check(
        cookie_has(cookies, "csrftoken", "Secure"),
        "csrftoken cookie has the Secure attribute",
    )
    if "sessionid" in cookies:
        check(
            cookie_has(cookies, "sessionid", "Secure"),
            "sessionid cookie has the Secure attribute on the login page",
        )

    csrf = hidden_csrf(body)
    if not csrf:
        csrf = cookies.get("csrftoken", [("", [])])[0][0]
    check(bool(csrf), "login page exposes a CSRF token")
    if not csrf:
        return

    status, location, cookies = login_post(
        host,
        port,
        public_host,
        csrf,
        f"https://{public_host}",
        user,
        password,
    )
    check(
        status in (302, 303),
        f"login POST with a trusted Origin succeeds (got {status})",
    )
    check(
        location is not None,
        f"login POST returns a Location header ({location})",
    )
    check("sessionid" in cookies, "successful login sets sessionid")
    check(
        cookie_has(cookies, "sessionid", "Secure"),
        "sessionid cookie has the Secure attribute after login",
    )
    if "csrftoken" in cookies:
        check(
            cookie_has(cookies, "csrftoken", "Secure"),
            "rotated csrftoken cookie has the Secure attribute",
        )

    status, body, cookies = login_page(host, port, public_host)
    csrf = hidden_csrf(body) or cookies.get("csrftoken", [("", [])])[0][0]
    if not csrf:
        fail("second login page did not expose a CSRF token")
        return
    status, location, _ = login_post(
        host,
        port,
        public_host,
        csrf,
        "https://evil.example",
        user,
        password,
    )
    check(
        status == 403,
        f"login POST with an untrusted Origin is rejected with 403 (got {status})",
    )
    check(location is None, "rejected login POST does not redirect")


def recv_headers(sock):
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(65536)
        if not chunk:
            break
        data += chunk
    head, _, rest = data.partition(b"\r\n\r\n")
    return head, rest


def recv_some(sock, timeout, first=4096):
    sock.settimeout(timeout)
    try:
        return sock.recv(first)
    except socket.timeout:
        return b""


def check_http_stream(base, public_host, token):
    host, port = split_base(base)
    request = (
        f"POST /ps/sync/stream HTTP/1.1\r\n"
        f"Host: {public_host}\r\n"
        f"Authorization: Bearer {token}\r\n"
        f"Accept: application/vnd.powersync.bson-stream;q=0.9,application/x-ndjson;q=0.8\r\n"
        f"Content-Length: 0\r\n"
        f"Connection: close\r\n\r\n"
    )
    sock = socket.create_connection((host, port), timeout=30)
    sock.sendall(request.encode())
    head, rest = recv_headers(sock)
    text = head.decode("latin-1")
    first_line = text.split("\r\n", 1)[0]
    check(" 200 " in first_line, f"POST /ps/sync/stream returns 200 ({first_line})")
    check(
        "x-accel-buffering: no" in text.lower(),
        "streaming response carries X-Accel-Buffering: no",
    )
    data = rest
    deadline = time.time() + 25
    while not data and time.time() < deadline:
        chunk = recv_some(sock, max(0.1, deadline - time.time()))
        if not chunk:
            break
        data += chunk
    check(len(data) > 0, f"streaming body delivers data incrementally ({len(data)} bytes)")
    if data:
        sample = data[:200].decode("latin-1", "replace")
        print(f"INFO first stream bytes: {sample!r}", flush=True)
        if b"checkpoint" in data.lower():
            ok("first stream bytes contain a checkpoint document")
        else:
            print(
                "INFO checkpoint marker not in the first bytes (binary BSON framing); "
                "the non-empty incremental read above is the assertion",
                flush=True,
            )
    sock.close()


def check_websocket(base, public_host, token):
    host, port = split_base(base)
    key = base64.b64encode(b"wger-rs-prxy-chk").decode()
    request = (
        f"GET /ps/sync/stream HTTP/1.1\r\n"
        f"Host: {public_host}\r\n"
        f"Authorization: Bearer {token}\r\n"
        f"Connection: Upgrade\r\n"
        f"Upgrade: websocket\r\n"
        f"Sec-WebSocket-Version: 13\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        f"Sec-WebSocket-Protocol: rsocket\r\n\r\n"
    )
    sock = socket.create_connection((host, port), timeout=30)
    sock.sendall(request.encode())
    head, _ = recv_headers(sock)
    text = head.decode("latin-1")
    first_line = text.split("\r\n", 1)[0]
    check(
        " 101 " in first_line,
        f"GET /ps/sync/stream WebSocket upgrade returns 101 ({first_line})",
    )
    check(
        "upgrade: websocket" in text.lower(),
        "101 response keeps the Upgrade: websocket header",
    )
    # Full RSocket SETUP framing is SDK-specific and outside this contract.
    # Instead prove bytes flow both ways through the tunnel: a masked WebSocket
    # ping must be answered with a pong by the real PowerSync server.
    mask = b"\x01\x02\x03\x04"
    payload = b"wger-rs"
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    sock.sendall(bytes([0x89, 0x80 | len(payload)]) + mask + masked)
    sock.settimeout(10)
    try:
        frame = sock.recv(64)
    except socket.timeout:
        frame = b""
    check(
        len(frame) >= 2 and frame[0] == 0x8A,
        f"WebSocket ping through /ps is answered by PowerSync with a pong ({frame[:2]!r})",
    )
    sock.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", required=True)
    parser.add_argument("--host", required=True)
    parser.add_argument("--user", default="admin")
    parser.add_argument("--password", default="s3cret-pass")
    parser.add_argument("--token", default="")
    args = parser.parse_args()

    check_login(args.base, args.host, args.user, args.password)
    if args.token:
        check_http_stream(args.base, args.host, args.token)
        check_websocket(args.base, args.host, args.token)
    else:
        fail("no PowerSync token was supplied")

    print(f"\n{len(PASS)} passed, {len(FAIL)} failed", flush=True)
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
