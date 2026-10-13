---
name: run-contract-harness
description: Build wger-rs and run the Django 2.7 vs Rust contract harness on a throwaway DB. Use before every push to a wger-rs PR.
---
# Run the contract harness

1. `df -h /`. If usage is over 85%, run `docker builder prune -f`.
2. Build: `cd repo && cargo-docker build --release`.
3. Copy the binary into the repo (stop the old harness process first; it holds the file):
   ```sh
   kill "$(cat ../harness-runtime/rust.pid)" 2>/dev/null; pkill -f '^./wger-rs$' || true
   docker run --rm -v "$HOME/.cargo-docker/target:/target" -v "$PWD:/out" rust:1.99-bookworm \
     bash -c 'cp /target/release/wger-rs /out/wger-rs'
   ```
4. Run: `cd ../harness-runtime && ./run-all.sh > run-all.log 2>&1`. This resets the throwaway
   `wger-rs-m2-pg` and `wger-rs-m2-django` containers, seeds fake data and starts Rust on :8012.
5. Read `compare.out`: `grep -E '^PASS [0-9]+|^FAIL' compare.out`. Expect `FAIL 0`.
6. For a FAIL, look at the diff under it:
   - If both sides return the same status but your test expectation is wrong, fix the test.
   - Otherwise fix `src/` to match Django. Never change Django or the schema.
7. Never point the harness at prod containers or prod DB. Never print `jwt.env` or the harness env.
