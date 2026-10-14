# Multi-stage build. The builder is Rust on Alpine and emits a static musl
# binary (rustls, no glibc or system OpenSSL). The runtime is Alpine.
FROM rust:1.89-alpine AS build
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM alpine:3.22
RUN apk add --no-cache ca-certificates \
    && addgroup -g 65532 -S wger \
    && adduser -u 65532 -S -G wger -H -h /nonexistent -s /sbin/nologin wger
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/wger-rs /usr/local/bin/wger-rs
USER wger
ENV BIND=0.0.0.0:8000
EXPOSE 8000
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s \
    CMD wget -q -O /dev/null http://127.0.0.1:8000/healthz || exit 1
ENTRYPOINT ["wger-rs"]
