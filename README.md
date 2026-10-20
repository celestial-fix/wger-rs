# wger-rs

`wger-rs` is an independently implemented Rust API server that aims to be
compatible with the REST and PowerSync contracts used by wger 2.7. It reads
the existing wger PostgreSQL schema; Django remains the schema owner. This
project does not include or modify the wger Django application.

This is an API service, not a replacement for wger's browser UI. It is intended
to run beside an existing wger deployment or against a compatible database.

## Current capabilities

- REST API reads and writes for supported exercise, routine, nutrition,
  measurement, and workout resources
- allauth-headless JWT login/refresh compatibility
- PowerSync token/JWKS endpoints and upload handling
- Shared read access to the existing media volume

The contract harness compares behavior with `wger/server:2.7`. Compatibility is
endpoint-by-endpoint; see the source routes and harness for current coverage.
The service does not currently provide the Django-rendered web UI or general
multipart media upload endpoints.

## Run

Requirements: Docker Compose, PostgreSQL with a compatible wger 2.7 schema, and
the deployment's JWT key pair. No schema migrations are run by this service.

1. Copy `.env.example` to `.env` and set the database URL, site URL, and JWT
   keys. Keep `.env` private and do not commit it.
2. Review `deploy/compose.yml`, especially the database and media settings.
3. Start the service:

   ```sh
   docker compose -f deploy/compose.yml up -d --build
   ```

The database URL must point to an existing wger database. The private key
should be supplied as a single-line JWK JSON value in the format accepted by
the application; the public key must be the matching JWK. Use the same key pair
as the wger deployment if existing clients need token compatibility.
The example compose file binds the API to `127.0.0.1:8000` for a local reverse
proxy. Its media volume is read-write and must be writable by UID 1000 when
media-file deletion is used.

## Development and tests

Run unit tests with:

```sh
cargo test --lib
```

The contract harness uses Docker, a disposable PostgreSQL database, synthetic
test records, and the Django 2.7 server image. It never needs a production
database or production data:

```sh
harness/runtime/run-all.sh
harness/runtime/run-all.sh --down
```

Never point the harness at a live database. Do not add real user data, API
responses, credentials, keys, tokens, or production configuration to tests.

## Relationship and licensing

This is a compatibility implementation and is not affiliated with or endorsed
by the wger project. The wger application is a separate AGPL-3.0-or-later
project; its source code, data, trademarks, and other assets are not included
here. This repository is licensed under AGPL-3.0-or-later; dependencies keep
their respective licenses. See [`LICENSE`](LICENSE), [`COPYRIGHT`](COPYRIGHT),
and [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

If you operate a modified version as a network service, AGPL section 13 requires
users interacting with it remotely to have an opportunity to obtain the
Corresponding Source. Provide a prominent source link or equivalent notice in
the service's user-facing interface or API documentation.
