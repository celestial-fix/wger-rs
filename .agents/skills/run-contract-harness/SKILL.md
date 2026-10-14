---
name: run-contract-harness
description: Build wger-rs and run the Django 2.7 vs Rust contract harness on a throwaway DB. Use before every push to a wger-rs PR.
---
# Run the contract harness

1. `df -h /`. If usage is over 85%, run `docker builder prune -f`.
2. Build: `cd repo && cargo-docker build --release`.
3. Copy the binary to `$REPO/wger-rs` (stop the old harness process first; it holds the file):
   ```sh
   kill "$(cat harness/runtime/.state/rust.pid)" 2>/dev/null; pkill -f '^./wger-rs$' || true
   docker run --rm -v "$HOME/.cargo-docker/target:/target" -v "$PWD:/out" rust:1.99-bookworm \
     bash -c 'cp /target/release/wger-rs /out/wger-rs'
   ```
   Or point the runner at the built binary with `WGER_RS_BIN=/path/to/wger-rs`.
4. Run: `repo/harness/runtime/run-all.sh`. This resets the throwaway `wger-rs-m2-pg` and
   `wger-rs-m2-django` containers, seeds fake data and starts Rust on :8012. It writes its
   output (JWT keys, media, `rust.log`, `compare.out`) into `harness/runtime/.state/`.
5. Read `harness/runtime/.state/compare.out`: `grep -E '^PASS [0-9]+|^FAIL' harness/runtime/.state/compare.out`.
   Expect `FAIL 0`; the runner exits non-zero when `FAIL > 0`. Teardown with
   `harness/runtime/run-all.sh --down` (or `harness/runtime/down.sh`).
6. For a FAIL, look at the diff under it:
   - If both sides return the same status but your test expectation is wrong, fix the test.
   - Otherwise fix `src/` to match Django. Never change Django or the schema.
7. Never point the harness at prod containers or prod DB. Never print `jwt.env` or the harness env.
