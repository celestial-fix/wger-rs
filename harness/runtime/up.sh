#!/bin/bash
set -euo pipefail
DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$DIR" rev-parse --show-toplevel)
STATE="${HARNESS_STATE_DIR:-$REPO/harness/runtime/.state}"

HARNESS_NAME="${HARNESS_NAME:-wger-rs-m2}"
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

if [ "${RESTORE_DUMP:-0}" = 1 ]; then
  DUMP="${RESTORE_DUMP_PATH:-/opt/backup/wger-trim/wger-20261008-trim.dump}"
  echo "Restoring $DUMP (this may take a while; watch RAM)"
  docker exec -i "${HARNESS_NAME}-pg" pg_restore -U wger -d wger --no-owner --role=wger --clean --if-exists < "$DUMP" || true
  "$DIR/ram-guard.sh"
fi

"${COMPOSE[@]}" up -d django
"$DIR/ram-guard.sh"
echo "Throwaway up. Django :${HARNESS_DJANGO_PORT}  PG container ${HARNESS_NAME}-pg"
