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
export HARNESS_JWT_ENV="$STATE/jwt.env"
DJANGO_CONTAINER="${HARNESS_NAME}-django"

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

# No host bind mounts: create the container, copy the seed scripts in (0644 so
# the image user uid 1000 can read them), then start it.
"${COMPOSE[@]}" create django
"${COMPOSE[@]}" run --rm --no-deps --user 0 --entrypoint sh django \
  -c 'chown 1000:1000 /tmp/wger-media'
install -m 0644 "$DIR/seed_runner.py" "$STATE/seed_runner.py"
install -m 0644 "$REPO/harness/seed.py" "$STATE/seed_orig.py"
docker cp "$STATE/seed_runner.py" "${DJANGO_CONTAINER}:/tmp/seed_runner.py"
docker cp "$STATE/seed_orig.py" "${DJANGO_CONTAINER}:/tmp/seed_orig.py"
"${COMPOSE[@]}" start django
"$DIR/ram-guard.sh"
echo "Throwaway up. Django :${HARNESS_DJANGO_PORT}  PG container ${HARNESS_NAME}-pg"
