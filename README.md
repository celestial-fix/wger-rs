# wger-rs

[![CI](https://github.com/heavenly-tech/wger-rs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/heavenly-tech/wger-rs/actions/workflows/ci.yml)
[![Contract](https://github.com/heavenly-tech/wger-rs/actions/workflows/contract.yml/badge.svg?branch=main)](https://github.com/heavenly-tech/wger-rs/actions/workflows/contract.yml)

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
proxy and mounts the static and media volumes read-only. It runs the container
as `user: "65534:65534"`, the same numeric user as the `scratch` runtime image.
See [Read-only mounts](#read-only-mounts) for the writable-media variant.

## Static and media files

`wger-rs` serves the files a front-end nginx used to serve, so a shared reverse
proxy such as `nginx-proxy` can route straight to this service:

| Mount | Environment variable | Default | Cache-Control |
| --- | --- | --- | --- |
| `/static/*` hashed file name | `STATIC_ROOT` | `/static` | `public, max-age=31536000, immutable` |
| `/static/*` other file name | `STATIC_ROOT` | `/static` | `public, max-age=3600` |
| `/media/*` | `MEDIA_ROOT` | `/media` | `public, max-age=86400` |

- Only `GET` and `HEAD` are served, without authentication and outside the JSON
  API middleware.
- Django's `ManifestStaticFilesStorage` keeps both `app.css` and
  `app.80a6279921f8.css`. `/static` uses the long immutable lifetime only when
  the file name has a `.<12 hex>.` segment before the extension, including the
  `.map`, `.gz`, and `.br` variants; every other static path gets one hour, so
  a redeployed `app.css` is picked up without a cache purge.
- `Cache-Control` is attached only to `2xx`, `206`, and `304` responses. A
  `404` (or any other status) is sent with `Cache-Control: no-store` so clients
  and proxies do not remember a missing file.
- `/media` sets `Content-Type` from an allowlist of extensions (`png`,
  `jpg`/`jpeg`, `gif`, `webp`, `svg`, `mp4`, `webm`, `pdf`). Anything else is
  served as `application/octet-stream`, whatever its extension, so an uploaded
  file cannot be interpreted as a script or stylesheet.
- Both mounts send `X-Content-Type-Options: nosniff`, and SVG files on `/media`
  also get `Content-Security-Policy: sandbox`.
- `/static` also answers `Vary: Accept-Encoding` and uses precompressed
  `.gz`/`.br` siblings when the client accepts them.
- The headers are scoped to these two mounts; they are not added to the API's
  own responses.
- Directory indices are disabled, and paths outside the configured roots are
  never served.
- A missing or unmounted root only logs a startup warning; requests return 404.
- The runtime image is built `FROM scratch`, with CA certificates copied from
  the builder and the numeric user `65534:65534`. Because the image has no
  `wget`, the Docker health check runs `wger-rs healthcheck`, which probes
  `http://127.0.0.1:<port from BIND>/healthz`.

### Read-only mounts

The static and media mounts are read-only by default, and the container runs as
`65534:65534`, so the service needs no write access to either volume.

Media-file deletion (`DELETE` of image records) writes to `MEDIA_ROOT`. If you
use it, or if uploads are written into the shared media volume by this service,
mount `/media` read-write and run the container as the uid that owns that
volume:

```yaml
    user: "1000:1000"      # uid owning the media volume (wger image uses 1000)
    volumes:
      - wger-static:/static:ro
      - wger-media:/media:rw
```

The `/static` mount can stay read-only in every case.

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

GitHub Actions runs formatting checks, Rust tests, a container build, and the
disposable contract harness. Dependabot updates Rust and Actions dependencies.
CodeQL and dependency review activate when the repository is public; private
repositories need an eligible GitHub Advanced Security plan for those scans.
Tagged `vX.Y.Z` releases publish an amd64 image to GHCR and create a GitHub
Release after validation passes.

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
