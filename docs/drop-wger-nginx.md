# Running without `wger-nginx`

`wger-rs` replaces the `wger-nginx` (`nginx:stable`) container. It is now the
single container behind `nginx-proxy`, and performs every route, header, and
file-serving job the live `nginx.conf` performed.

The implementation lives in:

- `src/proxy.rs` — routing precedence, fixed upstreams, prefix stripping,
  streaming, WebSocket passthrough, body limit, hop-by-hop headers.
- `src/forwarded.rs` — `TRUSTED_PROXIES`, `X-Forwarded-*` normalisation, DNS
  re-resolution every 30 seconds.
- `src/config.rs` — `DJANGO_UI_URL`, `POWERSYNC_UPSTREAM_URL`, and the
  `UpstreamUrl` allowlist parser.
- `src/static_files.rs` — `/static/` and `/media/` (unchanged behaviour).
- `tests/proxy.rs` — routing, streaming, upgrades, 413, header trust, cookies.

## nginx directive coverage

| nginx directive / location | How `wger-rs` matches it | Test |
| --- | --- | --- |
| `upstream wger { server web:8000; }` | Local axum routes (`/healthz`, `/allauth/app/...`, cookie-less `/api/...`) answer before any proxy. | `routing_matches_nginx_precedence`, `session_cookie_selects_django_for_api_requests` |
| `upstream django_ui { server django-ui:8000; }` | `DJANGO_UI_URL` is parsed into a fixed `UpstreamUrl`; the connection address never comes from the request. Unset → 502. | `upstream_accepts_plain_http_origins`, `upstream_rejects_anything_but_a_bare_http_origin`, `missing_upstreams_are_502_and_ps_still_sets_accel_header` |
| `upstream powersync { server powersync:8080; }` | `POWERSYNC_UPSTREAM_URL` is a separate fixed upstream. `POWERSYNC_URL` remains the public URL returned to clients. | `routing_matches_nginx_precedence`, `every_ps_response_has_the_accel_header` |
| `map $http_x_forwarded_proto $final_forwarded_proto { default ...; '' $scheme; }` | `forwarded::normalize` keeps a trusted peer's `X-Forwarded-Proto`, defaults to `http` when absent; an untrusted peer always gets `http`. | `trusted_peer_appends_to_the_chain_and_keeps_the_scheme`, `untrusted_peer_cannot_spoof_forwarded_headers`, `trusted_peer_keeps_the_proxy_chain_and_untrusted_peer_is_overwritten` |
| `map $cookie_sessionid $api_upstream { '' wger; default django_ui; }` | For `/api/` paths a non-empty `sessionid` cookie is checked before local routing and forwards to Django. `sessionid=` empty stays local. | `session_cookie_selects_django_for_api_requests`, `session_cookie_split_requires_a_non_empty_value` |
| `location ^~ /allauth/app/ { proxy_pass http://wger; }` | Handled by the existing local routes; unmatched `/allauth/app/` paths are a local 404 and never reach Django. | `routing_matches_nginx_precedence` |
| `location ^~ /allauth/ { proxy_pass http://django_ui; }` | Fallback forwards to Django. | `routing_matches_nginx_precedence` |
| `location ^~ /api/ { proxy_pass http://$api_upstream; }` | Cookie split as above; with no cookie, the local API router answers. Unknown cookie-less API paths return the local 404 rather than falling through to Django. | `routing_matches_nginx_precedence`, `session_cookie_selects_django_for_api_requests` |
| `location / { proxy_pass http://django_ui; }` | Fallback forwards every other path (`/`, admin, UI pages). | `routing_matches_nginx_precedence` |
| `location /ps/ { proxy_pass http://powersync/; }` | `/ps` is stripped before forwarding (`/ps/sync/stream` → `/sync/stream`, `/ps/` → `/`). | `powersync_prefix_is_stripped_like_proxy_pass`, `routing_matches_nginx_precedence` |
| `proxy_set_header Host $http_host;` | The incoming `Host` is forwarded unchanged to every upstream, including `/ps/`; when absent, the fixed upstream authority is used. A spoofed `Host` cannot change the connection destination. | `host_spoofing_cannot_redirect_the_upstream` |
| `proxy_set_header X-Real-IP $remote_addr;` (+ trusted chain) | Untrusted peer → peer IP. Trusted peer → left-most incoming `X-Forwarded-For` entry, else the peer. | `trusted_peer_appends_to_the_chain_and_keeps_the_scheme`, `trusted_peer_keeps_the_proxy_chain_and_untrusted_peer_is_overwritten` |
| `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;` | Trusted peer appends itself to the supplied chain; untrusted peer replaces the chain with its own address. | `untrusted_peer_cannot_spoof_forwarded_headers`, `trusted_peer_appends_to_the_chain_and_keeps_the_scheme` |
| `proxy_set_header X-Forwarded-Proto $final_forwarded_proto;` | See the `final_forwarded_proto` row. | `untrusted_peer_cannot_spoof_forwarded_headers`, `trusted_peer_keeps_the_proxy_chain_and_untrusted_peer_is_overwritten` |
| `proxy_set_header X-Forwarded-Host $http_host;` | Always derived from the real `Host` header, never from a client-supplied `X-Forwarded-Host`. Removed from `/ps/` requests (nginx does not set it there). | `trusted_peer_appends_to_the_chain_and_keeps_the_scheme`, `routing_matches_nginx_precedence` |
| `proxy_http_version 1.1;` | The hyper client sends HTTP/1.1 upstream for all three locations. | `routing_matches_nginx_precedence`, `websocket_upgrade_passes_through_ps_and_django` |
| `proxy_read_timeout 86400s; proxy_send_timeout 86400s;` (`/`, `/api/`) | No read/send timeout is applied; the proxy waits as long as the upstream or client needs (≥ 86400s). | `powersync_streaming_flushes_the_first_chunk_immediately` (long-lived stream), `websocket_upgrade_passes_through_ps_and_django` |
| `proxy_buffering off; proxy_read_timeout 1d; proxy_send_timeout 1d;` (`/ps/`) | Response bodies are relayed as `hyper::body::Incoming` with no collect/buffer step; no total or idle timeout is configured. | `powersync_streaming_flushes_the_first_chunk_immediately` |
| `add_header X-Accel-Buffering no always;` (`/ps/`) | Added to every `/ps/` response, including 502/413/404. | `every_ps_response_has_the_accel_header`, `missing_upstreams_are_502_and_ps_still_sets_accel_header` |
| `proxy_set_header Upgrade $http_upgrade; proxy_set_header Connection "upgrade";` | A real upgrade (both `Upgrade` and `Connection: upgrade`) is forwarded and, after `101`, bytes are copied both ways with `copy_bidirectional`. Non-upgrade requests get `Connection: close`. | `websocket_upgrade_passes_through_ps_and_django`, `upgrade_requires_both_headers` |
| `proxy_redirect off;` | `Location` is copied from the upstream response unchanged. | `cookies_and_location_pass_through_untouched` |
| `location /static/ { alias /wger/static/; add_header Cache-Control "public, max-age=31536000, immutable" always; add_header Vary "Accept-Encoding" always; }` | `static_files` serves the `STATIC_ROOT` mount, answers `Vary: Accept-Encoding`, and uses the immutable year for content-hashed names. | `static_file_is_served_with_long_lived_cache_headers`, `static_precompressed_variants_are_used`, `static_hashed_assets_are_immutable` |
| `location /media/ { alias /wger/media/; }` | `static_files` serves the `MEDIA_ROOT` mount with a day of caching and an extension allowlist. | `media_file_is_served_with_a_day_of_caching`, `media_content_type_follows_the_extension_allowlist` |
| `client_max_body_size 100M;` | A global middleware rejects `Content-Length > 100 MiB` with 413; proxied bodies are wrapped in a streaming 100 MiB limiter; local extractors use `DefaultBodyLimit::max(100 MiB)`. | `oversized_content_length_is_rejected_before_connecting`, `streamed_overflow_is_413_on_a_proxied_route` |
| http-level `gzip off;` | No compression layer is installed. `Vary: Accept-Encoding` is still sent for precompressed static siblings. | `static_precompressed_variants_are_used` |
| http-level `sendfile on;` | Not applicable in Rust; `ServeDir` reads the file and hyper writes it to the socket. The observable status/headers/bytes are the same. | `static_file_is_served_with_long_lived_cache_headers`, `media_range_requests_still_work` |
| http-level `keepalive_timeout 65;` | No shorter client keep-alive timeout is configured; hyper keeps connections alive until either side closes. | `powersync_streaming_flushes_the_first_chunk_immediately` |

### Intended differences

- **`/static` lifetime for unhashed names.** nginx sends
  `public, max-age=31536000, immutable` for every `/static/` path. `wger-rs`
  sends the immutable year only for Django's content-hashed names
  (`app.<12 hex>.css` and its `.map`/`.gz`/`.br` variants) and one hour for
  unhashed names. This is deliberate: `app.css` keeps its name across deploys,
  so a year-long immutable response would pin a stale stylesheet. Hashed
  assets, which are what production pages reference, match nginx exactly.
- **`/media` caching.** nginx sends no `Cache-Control`; `wger-rs` sends
  `public, max-age=86400`, matching nginx's default one-day freshness heuristic
  in a form clients can see.
- **`X-Real-IP` behind a trusted proxy.** The live config sends
  `$remote_addr` (the nginx-proxy address). `wger-rs` sends the left-most
  `X-Forwarded-For` entry, as required by the drop-nginx contract, so Django
  logs the real client.
- **`Host` on untrusted peers.** A direct client can still set `Host`; that is
  forwarded because nginx `$http_host` did the same. What it cannot do is
  change the upstream connection: the authority comes only from the two fixed
  environment URLs.
- **Open proxy targets.** Absolute-form request targets and `CONNECT` are
  rejected with 400. nginx would not have proxied them either, but it answered
  with its own 400/405; the explicit rejection is tested.

## Forwarded-header trust

`TRUSTED_PROXIES` is a comma-separated list of IPs, CIDRs, and DNS hostnames.
Empty is the default and trusts nobody.

- Hostnames are validated at startup, resolved before the listener starts, and
  re-resolved every 30 seconds. A failed re-resolution keeps the last known
  addresses.
- Only when `ConnectInfo<SocketAddr>`'s peer is trusted are incoming
  `X-Forwarded-For`, `X-Forwarded-Proto`, `X-Forwarded-Host`, and `X-Real-IP`
  accepted. For an untrusted peer they are removed and replaced with the peer
  address and `http`.
- The peer is appended to `X-Forwarded-For` for trusted peers, matching
  `$proxy_add_x_forwarded_for`.
- `Host` is always forwarded unchanged.

## PowerSync transport used by the wger mobile client

[wger-project/flutter](https://github.com/wger-project/flutter) at commit
`a88ba0e` uses the PowerSync Dart SDK `2.5.0` with
`SyncOptions(httpClient: ...)` and no explicit connection method. That SDK
version only implements **HTTP streaming**: it sends
`POST <endpoint>/sync/stream` with
`Accept: application/vnd.powersync.bson-stream;q=0.9,application/x-ndjson;q=0.8`
and probes `<endpoint>/probes/liveness` (and
`<endpoint>/write-checkpoint2.json`). The WebSocket transport is used by the
web SDK and by the diagnostics app, not by that mobile SDK release.

`wger-rs` therefore supports both:

- `/ps/sync/stream` HTTP POST streaming with no buffering and
  `X-Accel-Buffering: no`.
- `/ps/...` WebSocket upgrades (`GET /ps/sync/stream`) with full HTTP/1.1
  `101 Switching Protocols` passthrough and bidirectional byte copy.

The real-container harness (`harness/runtime/proxy-check.sh`) exercises both
against `journeyapps/powersync-service:1.26.1`.

## Deploy steps

1. **Keep the database and volumes.** `wger-rs` does not migrate the schema.
   `wger-static` and `wger-media` are the same volumes the Django container
   already uses.
2. **Add the configuration.** In the `web` service:
   ```yaml
   environment:
     DJANGO_UI_URL: http://django-ui:8000
     POWERSYNC_UPSTREAM_URL: http://powersync:8080
     TRUSTED_PROXIES: nginx-proxy
     VIRTUAL_HOST: fitness.example.com
     VIRTUAL_PORT: "8000"
     LETSENCRYPT_HOST: fitness.example.com
   networks:
     default:
       aliases: [web]      # PowerSync fetches JWKS from http://web:8000
     proxy_network: {}
   ```
   `POWERSYNC_URL` stays the public URL returned to clients; the new
   `POWERSYNC_UPSTREAM_URL` is the internal origin only.
3. **Attach the proxy network.** Create or reuse the network `nginx-proxy`
   watches (`proxy_network` by default here):
   ```sh
   docker network inspect proxy_network >/dev/null 2>&1 || \
     docker network create proxy_network
   ```
4. **Mount the files.**
   ```yaml
   volumes:
     - wger-static:/static:ro
     - wger-media:/media:ro
   ```
5. **Start the replacement and stop nginx.**
   ```sh
   docker compose -f deploy/compose.yml up -d --build web
   curl -fsS http://127.0.0.1:8000/healthz        # if a loopback port is exposed
   docker compose -f /path/to/app-wger/docker-compose.yml stop nginx
   ```
   Start `web` before stopping nginx so the static/media mounts and health
   check are ready. nginx-proxy will discover the new container through
   `VIRTUAL_HOST`.
6. **Verify the routes.**
   ```sh
   curl -fsS https://fitness.example.com/healthz
   curl -fsS https://fitness.example.com/static/            # through django-ui/static
   curl -fsS https://fitness.example.com/api/v2/version/    # cookie-less → wger-rs
   curl -fsS -H 'Cookie: sessionid=x' https://fitness.example.com/api/v2/version/
   curl -fsS https://fitness.example.com/ps/probes/liveness
   ```
   The last request must return `X-Accel-Buffering: no`; the session cookie
   request must be answered by the Django UI.
7. **Roll back.** Stop `web`, start `wger-nginx`, and restore the previous
   `nginx` service. No database changes are made by this deployment.

### Health check

The image's `HEALTHCHECK` calls `/wger-rs healthcheck`, which probes
`http://127.0.0.1:<BIND port>/healthz`. `/healthz` is a local route and never
touches `DJANGO_UI_URL` or `POWERSYNC_UPSTREAM_URL`, so an upstream outage does
not mark the container unhealthy.

## Harness coverage notes

`harness/runtime/proxy-check.sh` runs real `wger/server:2.7` and
`journeyapps/powersync-service:1.26.1` behind the wger-rs binary.

- The throwaway PowerSync gets wger-rs's JWKS (fetched from
  `/api/v2/powersync-keys`) inline in `client_auth.jwks`, because containers
  cannot reliably reach a host process. Production still uses `jwks_uri` at
  `http://web:8000`.
- The WebSocket check asserts `101`, then sends a masked WebSocket ping and
  expects PowerSync's pong, proving bidirectional copy. It does not send an
  RSocket SETUP frame.
- The harness peer is always `127.0.0.1` (trusted). The untrusted-peer
  spoofing cases are covered by the cargo tests only.
