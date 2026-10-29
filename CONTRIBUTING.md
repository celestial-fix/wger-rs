# Contributing

Contributions are welcome. Please open an issue or pull request with a concise
description of the change and its tests.

Open pull requests against `main` from a topic branch. Do not push directly to
`main`; the repository CI and contract workflows run on pull requests.

## Before submitting

- Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`.
- For API behavior changes, run the contract harness against its disposable
  Django/PostgreSQL environment and include the `PASS`/`FAIL` summary.
- Use only synthetic fixtures (`@example.com` addresses and fake credentials).
- Do not include production API captures, real user records, database dumps,
  secrets, JWTs, or private deployment configuration.
- Do not change the wger database schema; Django owns it.

By submitting a contribution, you agree that it may be distributed under the
AGPL-3.0-or-later license included in this repository.
