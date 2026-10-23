# Multi-stage build. The builder is Rust on Alpine and emits a static musl
# binary (rustls, no glibc or system OpenSSL). The runtime is scratch: the
# binary needs nothing but CA certificates, and `wger-rs healthcheck` replaces
# wget for the Docker health check.
FROM rust:1.89-alpine AS build
RUN apk add --no-cache musl-dev gcc make ca-certificates
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/wger-rs /wger-rs
# Numeric non-root user, so the image works without /etc/passwd in scratch.
USER 65534:65534
ENV BIND=0.0.0.0:8000
EXPOSE 8000
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s \
    CMD ["/wger-rs", "healthcheck"]
ENTRYPOINT ["/wger-rs"]
