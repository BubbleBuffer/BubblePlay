# Relay image: a static musl binary on scratch, running as uid 10001.
#
# Build from the repository root, because the build needs crates/ (bp-device-key), relay-rs/
# and worker/public/ (the static assets):
#   docker build -f relay-rs/Dockerfile -t bp-relay .
# The ignore rules for this build live in relay-rs/Dockerfile.dockerignore (BuildKit reads a
# Dockerfile-specific ignore file that sits next to the Dockerfile).

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev ca-certificates \
 && mkdir -p /out/data \
 && chown 10001:10001 /out/data
WORKDIR /src
COPY crates ./crates
COPY relay-rs ./relay-rs
ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN cargo build --release --locked \
      --target x86_64-unknown-linux-musl \
      --manifest-path relay-rs/Cargo.toml \
      -p bp-relay-server --bin bp-relay

FROM scratch
COPY --from=build /src/relay-rs/target/x86_64-unknown-linux-musl/release/bp-relay /bp-relay
COPY worker/public /assets
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
COPY --from=build --chown=10001:10001 /out/data /data
USER 10001:10001
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/bp-relay", "healthcheck", "--config", "/data/relay.json"]
ENTRYPOINT ["/bp-relay", "serve", "--config", "/data/relay.json"]
