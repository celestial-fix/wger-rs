# PowerSync config: deployed vs upstream (checked 2026-10-08)

- Deployed: /opt/stacks/app-wger/config-powersync/{sync_rules.yaml,powersync.yaml} on heavenly (copied here as deployed-*.yaml).
- Upstream: wger-project/docker `services/config-powersync/` at commit 77fad3d (2026-08-21), the last change to these files
  before the server 2.7 tag (2026-09-03); the docker repo has no tags and master is still identical.
- Result: `sync_rules.diff` and `powersync_yaml.diff` are EMPTY, so the deployed files are byte-identical to upstream.
- Postgres: wal_level=logical; publication `powersync` (owner wger) with an explicit 30-table list, not FOR ALL TABLES,
  publishing insert/update/delete/truncate, via root = false. Replication source role: `wger` (PS_DATABASE_URI user;
  superuser, replication, bypassrls, login). Bucket storage role: `powersync_storage` (login only, no replication or superuser),
  in the same `wger` DB. Active logical slot `powersync_1_6c94` (pgoutput).
