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
- `GET`/`HEAD` serving of the Django static build and uploaded media, so the
  front-end reverse proxy does not need file-system access

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
proxy and mounts the static and media volumes read-only. Media-file deletion
(`DELETE` of image records) writes to `MEDIA_ROOT`, so mount that volume
read-write if you use it, and make it writable by the container user. The
production compose file uses `user: "1000:1000"`, which works with the
`scratch` runtime image.

## Static and media files

`wger-rs` serves the files a front-end nginx used to serve, so a shared reverse
proxy such as `nginx-proxy` can route straight to this service:

| Mount | Environment variable | Default | Cache-Control |
| --- | --- | --- | --- |
| `/static/*` | `STATIC_ROOT` | `/static` | `public, max-age=31536000, immutable` |
| `/media/*` | `MEDIA_ROOT` | `/media` | `public, max-age=86400` |

- Only `GET` and `HEAD` are served, without authentication and outside the JSON
  API middleware.
- `/static` also answers `Vary: Accept-Encoding` and uses precompressed
  `.gz`/`.br` siblings when the client accepts them.
- The cache headers match the nginx `location` blocks they replace, including
  on a 404 for a missing file. They are not added to the API's own responses.
- Directory indices are disabled, and paths outside the configured roots are
  never served.
- A missing or unmounted root only logs a startup warning; requests return 404.
- The runtime image is built `FROM scratch`, with CA certificates copied from
  the builder and the numeric user `65534:65534`. Because the image has no
  `wget`, the Docker health check runs `wger-rs healthcheck`, which probes
  `http://127.0.0.1:<port from BIND>/healthz`.

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
