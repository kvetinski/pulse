# syntax=docker/dockerfile:1

FROM rust:1.88-bookworm AS builder
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        protobuf-compiler \
        libprotobuf-dev \
        build-essential \
        cmake \
        pkg-config \
        libcurl4-openssl-dev \
        libssl-dev \
        libsasl2-dev \
        zlib1g-dev \
        libzstd-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim AS runtime
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 pulse \
    && useradd --system --uid 10001 --gid 10001 --create-home --home-dir /home/pulse pulse

COPY --from=builder /app/target/release/pulse /usr/local/bin/pulse
COPY scenarios.yaml /app/scenarios.yaml
COPY k8s/overlays/kind/scenarios.kind.yaml /app/scenarios.kind.yaml
COPY k8s/overlays/staging/scenarios.staging.yaml /app/scenarios.staging.yaml
COPY k8s/overlays/prod/scenarios.prod.yaml /app/scenarios.prod.yaml
COPY descriptors /app/descriptors

RUN chown -R 10001:10001 /app /home/pulse

USER 10001:10001

ENV RUST_LOG=info
STOPSIGNAL SIGTERM
CMD ["pulse"]
