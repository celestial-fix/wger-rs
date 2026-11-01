#!/bin/bash
# PowerSync + production-like Django checks through the real wger-rs proxy.
#
# Runs after the existing contract checks. All containers and ports are
# throwaway and loopback-only. No host bind mounts are used: config files are
# copied in with `docker cp`.
set -euo pipefail

DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$DIR" rev-parse --show-toplevel)
STATE="$REPO/harness/runtime/.state"
PROXY_STATE="$STATE/proxy"

HARNESS_NAME="${HARNESS_NAME:-wger-rs-contract}"
HARNESS_PG_PORT="${HARNESS_PG_PORT:-55432}"
WGER_RS_BIN="${WGER_RS_BIN:-$REPO/wger-rs}"
PS_IMAGE="${PS_IMAGE:-journeyapps/powersync-service:1.26.1}"

PG_CONTAINER="${HARNESS_NAME}-pg"
DJANGO_PROXY_CONTAINER="${HARNESS_NAME}-django-proxy"
POWERSYNC_CONTAINER="${HARNESS_NAME}-powersync-check"
NETWORK="${HARNESS_NAME}-net"

export HARNESS_NAME HARNESS_PG_PORT
export HARNESS_JWT_ENV="$STATE/jwt.env"

COMPOSE=(docker compose --project-directory "$DIR" -f "$DIR/docker-compose.yml" --project-name "${HARNESS_NAME}-harness")

pick_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

HARNESS_DJANGO_PROXY_PORT="${HARNESS_DJANGO_PROXY_PORT:-$(pick_port)}"
HARNESS_PROXY_PORT="${HARNESS_PROXY_PORT:-$(pick_port)}"
HARNESS_POWERSYNC_PORT="${HARNESS_POWERSYNC_PORT:-$(pick_port)}"
export HARNESS_DJANGO_PROXY_PORT HARNESS_PROXY_PORT HARNESS_POWERSYNC_PORT

teardown() {
  if [ -f "$PROXY_STATE/wger-rs.pid" ]; then
    kill "$(cat "$PROXY_STATE/wger-rs.pid")" 2>/dev/null || true
    rm -f "$PROXY_STATE/wger-rs.pid"
  fi
  docker rm -f "$POWERSYNC_CONTAINER" >/dev/null 2>&1 || true
  "${COMPOSE[@]}" --profile proxy rm -sf django-proxy >/dev/null 2>&1 || true
}

if [ "${1:-}" = "--down" ]; then
  teardown
  rm -rf "$PROXY_STATE" || true
  echo "Proxy checks torn down."
  exit 0
fi

mkdir -p "$PROXY_STATE/static"
teardown

if [ ! -x "$WGER_RS_BIN" ]; then
  echo "wger-rs binary not found/executable: $WGER_RS_BIN (set WGER_RS_BIN)" >&2
  exit 1
fi
if [ ! -f "$STATE/media-host/wger-harness-ids.json" ]; then
  echo "missing $STATE/media-host/wger-harness-ids.json; run run-all.sh first" >&2
  exit 1
fi

# The pg container must be decoding logically for PowerSync.
WAL_LEVEL=$(docker exec "$PG_CONTAINER" psql -U wger -d wger -tAc 'SHOW wal_level' | tr -d '[:space:]')
if [ "$WAL_LEVEL" != "logical" ]; then
  echo "harness postgres wal_level=$WAL_LEVEL, expected logical" >&2
  exit 1
fi

# Ensure the publication contains the table used by the minimal sync rules.
docker exec "$PG_CONTAINER" psql -U wger -d wger -tAc \
  "SELECT 1 FROM pg_publication WHERE pubname = 'powersync'" | grep -q 1 || \
  docker exec "$PG_CONTAINER" psql -U wger -d wger -c \
    "CREATE PUBLICATION powersync FOR TABLE core_language" >/dev/null
docker exec "$PG_CONTAINER" psql -U wger -d wger -c \
  "ALTER PUBLICATION powersync ADD TABLE core_language" >/dev/null 2>&1 || true

# ---------------------------------------------------------------------------
# Production-like Django: TLS terminated upstream, trusted forwarded headers.
# ---------------------------------------------------------------------------
install -m 0644 "$DIR/proxycheck_settings.py" "$PROXY_STATE/proxycheck_settings.py"
"${COMPOSE[@]}" --profile proxy create django-proxy >/dev/null
docker cp "$PROXY_STATE/proxycheck_settings.py" \
  "${DJANGO_PROXY_CONTAINER}:/home/wger/src/proxycheck_settings.py"
"${COMPOSE[@]}" --profile proxy start django-proxy >/dev/null

for _ in $(seq 1 90); do
  if curl -sf -o /dev/null -H 'Host: fitness.example.test' \
    "http://127.0.0.1:${HARNESS_DJANGO_PROXY_PORT}/api/v2/version/"; then
    break
  fi
  sleep 2
done
curl -sf -o /dev/null -H 'Host: fitness.example.test' \
  "http://127.0.0.1:${HARNESS_DJANGO_PROXY_PORT}/api/v2/version/" || {
  echo "proxy Django did not start" >&2
  docker logs --tail 120 "$DJANGO_PROXY_CONTAINER" >&2 || true
  exit 1
}
echo "proxy Django up on :${HARNESS_DJANGO_PROXY_PORT}"

# The templates need the singleton GymConfig row (normally created by
# `wger bootstrap`); the contract database is seeded without it.
docker exec "$DJANGO_PROXY_CONTAINER" sh -c \
  'cd /home/wger/src && python3 manage.py shell -c "from wger.config.models import GymConfig; GymConfig.objects.get_or_create(pk=1)"' \
  >/dev/null 2>&1

# ---------------------------------------------------------------------------
# wger-rs proxy (host process, so its peer address is 127.0.0.1).
# ---------------------------------------------------------------------------
set -a
# shellcheck disable=SC1090
. "$STATE/media-host/wger-harness.env"
set +a
export DATABASE_URL="postgres://wger:wger@127.0.0.1:${HARNESS_PG_PORT}/wger"
export BIND="0.0.0.0:${HARNESS_PROXY_PORT}"
export STATIC_ROOT="$PROXY_STATE/static"
export MEDIA_ROOT="$STATE/media-host"
export SITE_URL="http://127.0.0.1:${HARNESS_PROXY_PORT}"
export POWERSYNC_URL="http://127.0.0.1:${HARNESS_PROXY_PORT}/ps/"
export DJANGO_UI_URL="http://127.0.0.1:${HARNESS_DJANGO_PROXY_PORT}"
export POWERSYNC_UPSTREAM_URL="http://127.0.0.1:${HARNESS_POWERSYNC_PORT}"
export TRUSTED_PROXIES=127.0.0.1

cd "$REPO"
nohup "$WGER_RS_BIN" > "$PROXY_STATE/wger-rs.log" 2>&1 &
echo $! > "$PROXY_STATE/wger-rs.pid"
for _ in $(seq 1 30); do
  if curl -sf -o /dev/null "http://127.0.0.1:${HARNESS_PROXY_PORT}/healthz"; then
    break
  fi
  sleep 1
done
curl -sf -o /dev/null "http://127.0.0.1:${HARNESS_PROXY_PORT}/healthz" || {
  echo "wger-rs proxy failed to start" >&2
  cat "$PROXY_STATE/wger-rs.log" >&2 || true
  exit 1
}
echo "wger-rs proxy up on :${HARNESS_PROXY_PORT}"

# ---------------------------------------------------------------------------
# Throwaway PowerSync service pointed at the harness database.
# ---------------------------------------------------------------------------
# PowerSync gets wger-rs's real JWKS (fetched from /api/v2/powersync-keys) inline.
# Containers cannot reliably reach a host process (rootless docker / host
# firewalls), so jwks_uri is not used here; the production jwks_uri path is the
# same document served by the same endpoint.
JWKS_JSON=$(curl -fsS "http://127.0.0.1:${HARNESS_PROXY_PORT}/api/v2/powersync-keys")
printf '%s' "$JWKS_JSON" | python3 -c 'import json,sys; assert json.load(sys.stdin)["keys"]'
JWKS_INDENTED=$(printf '%s' "$JWKS_JSON" | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["keys"]))')
cat > "$PROXY_STATE/powersync.yaml" <<EOF
telemetry:
  disable_telemetry_sharing: true

replication:
  connections:
    - type: postgresql
      uri: postgres://wger:wger@pg:5432/wger
      sslmode: disable

storage:
  type: postgresql
  uri: postgres://wger:wger@pg:5432/wger
  sslmode: disable

port: 8080

sync_rules:
  path: /tmp/sync_rules.yaml

client_auth:
  jwks:
    keys: ${JWKS_INDENTED}
  audience: ["powersync"]
EOF
cat > "$PROXY_STATE/sync_rules.yaml" <<'EOF'
config:
  edition: 3

streams:
  languages:
    auto_subscribe: true
    queries:
      - SELECT * FROM core_language
EOF
chmod 0644 "$PROXY_STATE/powersync.yaml" "$PROXY_STATE/sync_rules.yaml"

docker create --name "$POWERSYNC_CONTAINER" \
  --network "$NETWORK" \
  -e PS_PORT=8080 \
  -e POWERSYNC_CONFIG_PATH=/tmp/powersync.yaml \
  -p "127.0.0.1:${HARNESS_POWERSYNC_PORT}:8080" \
  "$PS_IMAGE" start -r unified >/dev/null
docker cp "$PROXY_STATE/powersync.yaml" "$POWERSYNC_CONTAINER:/tmp/powersync.yaml"
docker cp "$PROXY_STATE/sync_rules.yaml" "$POWERSYNC_CONTAINER:/tmp/sync_rules.yaml"
docker start "$POWERSYNC_CONTAINER" >/dev/null

for _ in $(seq 1 90); do
  if curl -sf -o /dev/null "http://127.0.0.1:${HARNESS_POWERSYNC_PORT}/probes/liveness"; then
    break
  fi
  sleep 2
done
if ! curl -sf -o /dev/null "http://127.0.0.1:${HARNESS_POWERSYNC_PORT}/probes/liveness"; then
  echo "throwaway PowerSync did not become live" >&2
  docker logs --tail 160 "$POWERSYNC_CONTAINER" >&2 || true
  exit 1
fi
echo "throwaway PowerSync up on :${HARNESS_POWERSYNC_PORT}"

# ---------------------------------------------------------------------------
# Route and transport assertions through wger-rs.
# ---------------------------------------------------------------------------
LIVENESS_HEADERS=$(curl -sS -D - -o /dev/null -H 'Host: fitness.example.test' \
  "http://127.0.0.1:${HARNESS_PROXY_PORT}/ps/probes/liveness")
echo "$LIVENESS_HEADERS" | head -1 | grep -qE ' 200 ' || {
  echo "FAIL /ps/probes/liveness did not return 200" >&2
  echo "$LIVENESS_HEADERS" >&2
  exit 1
}
echo "$LIVENESS_HEADERS" | tr -d '\r' | grep -qi '^X-Accel-Buffering: no$' || {
  echo "FAIL /ps/probes/liveness is missing X-Accel-Buffering: no" >&2
  exit 1
}
echo "PASS /ps/probes/liveness is 200 with X-Accel-Buffering: no"

API_TOKEN=$(python3 -c \
  "import json; print(json.load(open('$STATE/media-host/wger-harness-ids.json'))['token'])")
TOKEN_JSON=$(curl -fsS -H "Authorization: Token ${API_TOKEN}" \
  "http://127.0.0.1:${HARNESS_PROXY_PORT}/api/v2/powersync-token")
PS_TOKEN=$(printf '%s' "$TOKEN_JSON" | python3 -c \
  'import json,sys; print(json.load(sys.stdin)["token"])')
[ -n "$PS_TOKEN" ] || { echo "FAIL no PowerSync token" >&2; exit 1; }
echo "PASS obtained a PowerSync token from wger-rs"

python3 "$DIR/proxy_check.py" \
  --base "http://127.0.0.1:${HARNESS_PROXY_PORT}" \
  --host fitness.example.test \
  --token "$PS_TOKEN"

echo "Proxy/PowerSync checks passed."
