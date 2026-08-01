# syntax=docker/dockerfile:1

FROM rust:1.88.0-bookworm AS builder
WORKDIR /build

RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

COPY demo/grpc-target/Cargo.toml demo/grpc-target/Cargo.lock ./
COPY demo/grpc-target/build.rs ./build.rs
COPY demo/grpc-target/proto ./proto
COPY demo/grpc-target/src ./src

RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime
WORKDIR /app

RUN groupadd --system --gid 10002 pulse-demo \
    && useradd --system --uid 10002 --gid 10002 --no-create-home pulse-demo

COPY --from=builder /build/target/release/pulse-demo-target /usr/local/bin/pulse-demo-target

USER 10002:10002
EXPOSE 50051
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/pulse-demo-target"]
