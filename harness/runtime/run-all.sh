#!/bin/bash
set -euo pipefail
DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$DIR" rev-parse --show-toplevel)
STATE="${HARNESS_STATE_DIR:-$REPO/harness/runtime/.state}"

HARNESS_NAME="${HARNESS_NAME:-wger-rs-m2}"
HARNESS_PG_PORT="${HARNESS_PG_PORT:-55432}"
HARNESS_DJANGO_PORT="${HARNESS_DJANGO_PORT:-8001}"
HARNESS_RUST_PORT="${HARNESS_RUST_PORT:-8012}"
WGER_RS_BIN="${WGER_RS_BIN:-$REPO/wger-rs}"

export HARNESS_NAME HARNESS_PG_PORT HARNESS_DJANGO_PORT HARNESS_RUST_PORT
export HARNESS_STATE_DIR="$STATE"
export HARNESS_MEDIA_HOST="$STATE/media-host"
export HARNESS_SEED_RUNNER="$DIR/seed_runner.py"
export HARNESS_SEED_PY="$REPO/harness/seed.py"
export HARNESS_JWT_ENV="$STATE/jwt.env"

COMPOSE=(docker compose --project-directory "$DIR" -f "$DIR/docker-compose.yml" --project-name "${HARNESS_NAME}-harness")

if [ "${1:-}" = "--down" ]; then
  exec "$DIR/down.sh"
fi

mkdir -p "$STATE/media-host"
if [ ! -f "$STATE/jwt.env" ]; then
  python3 "$DIR/gen-jwt.py" "$STATE/jwt.env"
  chmod 600 "$STATE/jwt.env"
fi

"$DIR/ram-guard.sh"
"$DIR/down.sh" || true
"$DIR/ram-guard.sh"

"${COMPOSE[@]}" up -d pg
for _ in $(seq 1 60); do
  st=$(docker inspect -f '{{.State.Health.Status}}' "${HARNESS_NAME}-pg" 2>/dev/null || echo starting)
  [ "$st" = healthy ] && break
  sleep 2
  "$DIR/ram-guard.sh" || exit 9
done
st=$(docker inspect -f '{{.State.Health.Status}}' "${HARNESS_NAME}-pg" 2>/dev/null || echo unknown)
[ "$st" = healthy ] || { echo "postgres not healthy (status=$st)"; docker logs --tail 80 "${HARNESS_NAME}-pg"; exit 1; }

"$DIR/ram-guard.sh"
"${COMPOSE[@]}" up -d django
for _ in $(seq 1 90); do
  if curl -sf -o /dev/null "http://127.0.0.1:${HARNESS_DJANGO_PORT}/api/v2/version/"; then break; fi
  sleep 3
  "$DIR/ram-guard.sh" || exit 9
done
curl -sf "http://127.0.0.1:${HARNESS_DJANGO_PORT}/api/v2/version/" || { echo "django not up"; docker logs --tail 80 "${HARNESS_NAME}-django"; exit 1; }
echo "django up: $(curl -sf "http://127.0.0.1:${HARNESS_DJANGO_PORT}/api/v2/version/")"

"${COMPOSE[@]}" exec -T django python3 /tmp/seed_runner.py

test -f "$STATE/media-host/wger-harness.env" || { echo "seed did not write wger-harness.env into $STATE/media-host"; exit 1; }
test -f "$STATE/media-host/wger-harness-ids.json" || { echo "seed did not write wger-harness-ids.json into $STATE/media-host"; exit 1; }

if [ -f "$STATE/rust.pid" ]; then
  kill "$(cat "$STATE/rust.pid")" 2>/dev/null || true
  rm -f "$STATE/rust.pid"
fi

set -a
. "$STATE/media-host/wger-harness.env"
set +a
export MEDIA_ROOT="$STATE/media-host"
export DJANGO_MEDIA_ROOT="$STATE/media-host"
export BIND="127.0.0.1:${HARNESS_RUST_PORT}"
export SITE_URL="http://127.0.0.1:${HARNESS_RUST_PORT}"
export DATABASE_URL="postgres://wger:wger@127.0.0.1:${HARNESS_PG_PORT}/wger"

test -x "$WGER_RS_BIN" || { echo "wger-rs binary not found/executable: $WGER_RS_BIN (set WGER_RS_BIN)"; exit 1; }
cd "$REPO"
nohup "$WGER_RS_BIN" > "$STATE/rust.log" 2>&1 &
echo $! > "$STATE/rust.pid"
for _ in $(seq 1 30); do
  if curl -sf -o /dev/null "http://127.0.0.1:${HARNESS_RUST_PORT}/healthz"; then break; fi
  sleep 1
done
curl -sf "http://127.0.0.1:${HARNESS_RUST_PORT}/healthz" || { echo "rust failed"; cat "$STATE/rust.log"; exit 1; }
echo "rust up"

export WGER_IDS="$STATE/media-host/wger-harness-ids.json"
export DJANGO_BASE="http://127.0.0.1:${HARNESS_DJANGO_PORT}"
export RUST_BASE="http://127.0.0.1:${HARNESS_RUST_PORT}"
export WGER_PG_CONTAINER="${HARNESS_NAME}-pg"
export WGER_MEDIA_ROOT="$STATE/media-host"

set +e
python3 "$REPO/harness/compare.py" | tee "$STATE/compare.out"
rc=${PIPESTATUS[0]}
set -e
RSS=$(ps -o rss= -p "$(cat "$STATE/rust.pid")" 2>/dev/null | tr -d ' ' || true)
echo "RUST_RSS_KB=${RSS:-0}" | tee -a "$STATE/compare.out"
echo "HEAD_SHA=$(git -C "$REPO" rev-parse HEAD)" | tee -a "$STATE/compare.out"
grep -E '^PASS [0-9]+|^FAIL' "$STATE/compare.out" || true
if [ "$rc" -ne 0 ]; then
  echo "FAIL: contract harness reported failures (compare.py exit $rc)" >&2
  exit "$rc"
fi
