#!/bin/bash
set -euo pipefail
DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$DIR" rev-parse --show-toplevel)
STATE="$REPO/harness/runtime/.state"

HARNESS_NAME="${HARNESS_NAME:-wger-rs-contract}"
HARNESS_PG_PORT="${HARNESS_PG_PORT:-55432}"
HARNESS_DJANGO_PORT="${HARNESS_DJANGO_PORT:-8001}"
HARNESS_RUST_PORT="${HARNESS_RUST_PORT:-8012}"

PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

export HARNESS_NAME HARNESS_PG_PORT HARNESS_DJANGO_PORT HARNESS_RUST_PORT
export HARNESS_JWT_ENV="$STATE/jwt.env"

docker compose --project-directory "$DIR" -f "$DIR/docker-compose.yml" --project-name "${HARNESS_NAME}-harness" down -v --remove-orphans || true

if [ -f "$STATE/rust.pid" ]; then
  kill "$(cat "$STATE/rust.pid")" 2>/dev/null || true
  rm -f "$STATE/rust.pid"
fi

if [ "$PURGE" = 1 ]; then
  # host media copy was produced by `docker cp` and is owned by the invoking
  # user, so a plain recursive delete is enough. `down -v` already removed the
  # named media volume. Harness containers only.
  rm -rf "$STATE" || true
fi

echo "Harness torn down."
free -h | head -2
