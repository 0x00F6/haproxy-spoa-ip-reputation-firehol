# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.98.0
ARG ALPINE_VERSION=3.23.5

FROM rust:$RUST_VERSION AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY vendor* ./vendor
COPY build.rs ./build.rs
COPY Makefile ./Makefile
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    make build

RUN --mount=type=cache,target=/app/target\
    ldd /app/target/x86_64-unknown-linux-musl/release/haproxy-spoa-ip-reputation-firehol \
    | grep -q "statically linked" \
    || (echo "ERROR: binary is not statically linked" >&2 && exit 1)

RUN --mount=type=cache,target=/app/target \
    mkdir out \
    && cp /app/target/x86_64-unknown-linux-musl/release/haproxy-spoa-ip-reputation-firehol /app/out/haproxy-spoa-ip-reputation-firehol

FROM alpine:$ALPINE_VERSION

RUN apk add --no-cache ca-certificates netcat-openbsd wget

WORKDIR /app

COPY --from=builder  \
    /app/out/haproxy-spoa-ip-reputation-firehol \
    /usr/local/bin/haproxy-spoa-ip-reputation-firehol

COPY firehol.mmdb* /app/firehol.mmdb

EXPOSE 9000

ENV RUST_LOG=info
ENV SPOA_LISTEN_ADRESS=0.0.0.0:9000
ENV MMDB_PATH=/app/firehol.mmdb
ENV DROP_CATEGORY=abuse

CMD ["/usr/local/bin/haproxy-spoa-ip-reputation-firehol"]