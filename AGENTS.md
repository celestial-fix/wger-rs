# AGENTS.md: wger-rs

## Purpose
wger-rs is an API-only Rust replacement for the self-hosted wger server (fitness.heavenly.cl).
It serves the same REST, allauth-headless JWT, PowerSync token/keys and `upload-powersync-data`
contract as **Django wger/server 2.7**, on the same Postgres schema, using far less RAM.

## Hard rules
1. **No schema changes.** No migrations, no new tables, columns or indexes in any shared DB.
   Django 2.7 owns the schema. Index experiments happen only on throwaway or preview copies.
2. **Same contract as Django 2.7.** Status codes, JSON bodies, error bodies, pagination and count
   behaviour must match byte-for-byte modulo ids and timestamps. The contract harness is the
   judge. If Django does something odd, copy it.
3. **Secrets via env only.** Never put passwords, JWT keys or tokens on argv, in git, in logs or
   in PR text. Use env files with mode 600. Fixtures use fake data only (`@example.com`).
4. **No prod swap without Pablo's OK in his own chat.** Never point prod (`app-wger`: wger-web,
   wger-db, wger-powersync, wger-celery-worker, wger-nginx) at wger-rs, and never write to the prod
   DB or media, unless Pablo himself approves it in his own chat. A relay through other people or
   agents does not count.
5. Never push to `main`. Work goes on `cursor/*` branches as draft PRs.

## Tests
- Host has no gcc/libssl. Build with `cargo-docker build --release` (`$HOME/bin`, wraps
  `rust:1.99-bookworm`). The binary lands in `$HOME/.cargo-docker/target/release/wger-rs`.
- Unit tests: `cargo-docker test --lib`.
- Check `df -h /` first and run `docker builder prune -f` if usage is over 85%.

## Contract harness
See `.agents/skills/run-contract-harness/SKILL.md`. In short: `harness-runtime/run-all.sh` starts a
throwaway Postgres + Django 2.7 + wger-rs, seeds fake data, runs `harness/compare.py`
(including `writes_checks.py`) and writes `compare.out` (`PASS n FAIL m`). Every case must pass.

## Preview
See `.agents/skills/preview-deploy/SKILL.md`. `/opt/stacks/app-wger-rs-preview` runs wger-rs on a
pg_dump copy of the live DB, bound to the Tailscale IP only (100.112.122.36:8095). Writes made
there never reach live wger.
