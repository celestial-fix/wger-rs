# Multi-stage build. The runtime image is bookworm-slim plus libssl and curl.
FROM rust:1.89-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/wger-rs /usr/local/bin/wger-rs
USER 65534:65534
ENV BIND=0.0.0.0:8000
EXPOSE 8000
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s \
    CMD curl -fsS http://127.0.0.1:8000/healthz || exit 1
ENTRYPOINT ["wger-rs"]
