#!/bin/bash
set -euo pipefail
DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$DIR" rev-parse --show-toplevel)
STATE="$REPO/harness/runtime/.state"

HARNESS_NAME="${HARNESS_NAME:-wger-rs-contract}"
HARNESS_PG_PORT="${HARNESS_PG_PORT:-55432}"
HARNESS_DJANGO_PORT="${HARNESS_DJANGO_PORT:-8001}"
HARNESS_RUST_PORT="${HARNESS_RUST_PORT:-8012}"

export HARNESS_NAME HARNESS_PG_PORT HARNESS_DJANGO_PORT HARNESS_RUST_PORT
export HARNESS_STATE_DIR="$STATE"
export HARNESS_MEDIA_HOST="$STATE/media-host"
export HARNESS_SEED_RUNNER="$DIR/seed_runner.py"
export HARNESS_SEED_PY="$REPO/harness/seed.py"
export HARNESS_JWT_ENV="$STATE/jwt.env"

COMPOSE=(docker compose --project-directory "$DIR" -f "$DIR/docker-compose.yml" --project-name "${HARNESS_NAME}-harness")

mkdir -p "$STATE/media-host"
# throwaway test media only: django 2.7 image runs as uid 1000 and must write
# seed files into this bind mount; the harness DB/data is disposable.
chmod 0777 "$STATE/media-host"
if [ ! -f "$STATE/jwt.env" ]; then
  python3 "$DIR/gen-jwt.py" "$STATE/jwt.env"
  chmod 600 "$STATE/jwt.env"
fi

"$DIR/ram-guard.sh"
"${COMPOSE[@]}" up -d pg
for _ in $(seq 1 60); do
  st=$(docker inspect -f '{{.State.Health.Status}}' "${HARNESS_NAME}-pg" 2>/dev/null || echo starting)
  [ "$st" = healthy ] && break
  sleep 2
  "$DIR/ram-guard.sh" || exit 9
done
"$DIR/ram-guard.sh"

"${COMPOSE[@]}" up -d django
"$DIR/ram-guard.sh"
echo "Throwaway up. Django :${HARNESS_DJANGO_PORT}  PG container ${HARNESS_NAME}-pg"
