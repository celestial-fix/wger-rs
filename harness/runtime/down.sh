#!/bin/bash
set -euo pipefail
DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$DIR" rev-parse --show-toplevel)
STATE="${HARNESS_STATE_DIR:-$REPO/harness/runtime/.state}"

HARNESS_NAME="${HARNESS_NAME:-wger-rs-m2}"
HARNESS_PG_PORT="${HARNESS_PG_PORT:-55432}"
HARNESS_DJANGO_PORT="${HARNESS_DJANGO_PORT:-8001}"
HARNESS_RUST_PORT="${HARNESS_RUST_PORT:-8012}"

PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

export HARNESS_NAME HARNESS_PG_PORT HARNESS_DJANGO_PORT HARNESS_RUST_PORT
export HARNESS_STATE_DIR="$STATE"
export HARNESS_MEDIA_HOST="$STATE/media-host"
export HARNESS_SEED_RUNNER="$DIR/seed_runner.py"
export HARNESS_SEED_PY="$REPO/harness/seed.py"
export HARNESS_JWT_ENV="$STATE/jwt.env"

docker compose --project-directory "$DIR" -f "$DIR/docker-compose.yml" --project-name "${HARNESS_NAME}-harness" down -v --remove-orphans || true

if [ -f "$STATE/rust.pid" ]; then
  kill "$(cat "$STATE/rust.pid")" 2>/dev/null || true
  rm -f "$STATE/rust.pid"
fi

if [ "$PURGE" = 1 ]; then
  # seed media is owned by the django image uid 1000; delete it as that user,
  # then drop the throwaway state dir on the host. Harness containers only.
  docker run --rm -v "$STATE/media-host:/m" wger/server:2.7 sh -c "rm -rf /m/* /m/.[!.]*" || true
  rm -rf "$STATE"
fi

# safety: never touch prod names
for c in wger-web wger-db wger-nginx wger-powersync wger-celery-worker wger-cache; do
  if ! docker ps -a --format '{{.Names}}' | grep -qx "$c"; then
    echo "WARN: prod container $c missing (unexpected)" >&2
  fi
done
echo "Harness torn down."
free -h | head -2
