---
name: preview-deploy
description: Redeploy the tailnet-only wger-rs preview (/opt/stacks/app-wger-rs-preview) with a new binary, or refresh its DB copy. Preview only, never prod.
---
# Preview deploy

Layout: `/opt/stacks/app-wger-rs-preview/`
- `compose.yml`: `db` (postgres:17-alpine, internal network, no ports, 512m), `web`
  (wger-rs binary mounted read-only, `100.112.122.36:8095:8000`, 128m), and `powersync` if present.
- `.env` (mode 600): `PG_PASSWORD` and preview-only JWT keys. Never print it.
- `media/`: a copy of prod media. `wger-rs` is the running binary; `wger-rs.prev` is the rollback.

## Redeploy a new binary
1. Run the harness first (run-contract-harness skill). It must show `FAIL 0`.
2. ```sh
   P=/opt/stacks/app-wger-rs-preview
   cp $P/wger-rs $P/wger-rs.prev
   cp repo/wger-rs $P/wger-rs.new && mv $P/wger-rs.new $P/wger-rs
   cd $P && docker compose up -d --force-recreate web
   curl -s http://100.112.122.36:8095/healthz   # {"status":"ok"}
   ```
3. Roll back with `mv wger-rs.prev wger-rs && docker compose up -d --force-recreate web`.

## Refresh the DB copy (needs Pablo's OK for a read-only dump)
1. Check `df -h /` (the dump is about 0.5 GB and the restored DB a few GB).
2. `docker exec wger-db pg_dump -U wger -d wger -Fc > $P/dump/wger.dump` (read-only on prod).
3. `docker compose down -v db`, `docker compose up -d db`, then
   `docker exec wger-rs-preview-db pg_restore -U wger -d wger --no-owner /dump/wger.dump`.
4. Delete the dump afterwards.

## Rules
- Bind only to the Tailscale IP. No public vhost, no nginx-proxy labels.
- Never mount prod volumes read-write. Never touch app-wger containers.
- Writes made in the preview never reach live wger. Say so when sharing the URL.
