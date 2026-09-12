# syntax=docker/dockerfile:1

ARG RUST_VERSION=1.98.0
ARG ALPINE_VERSION=3.23.5

###############################################################################
# Stage 1: build a static musl binary
###############################################################################
FROM rust:${RUST_VERSION} AS builder

# Rarely changes: musl toolchain and Rust target. musl-tools provides musl-gcc, used as the linker
# (.cargo/config.toml) and by the `cc` crate for the vendored OpenSSL, libgit2 and mimalloc sources.
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl

WORKDIR /app

# Rarely changes: the vendored geoip2 reader is cloned and patched inside the image, so the build
# does not depend on a vendor/ directory being present on the host (see `make patch-geoip2-rs`).
COPY Makefile geoip2-rs.patch ./
RUN make patch-geoip2-rs

# Build configuration and manifests. `Cargo.loc[k]` copies Cargo.lock when it exists and tolerates
# its absence (the file is git-ignored in this repository).
COPY .cargo/config.toml .cargo/
COPY Cargo.toml Cargo.loc[k] build.rs ./

# Cargo validates every target declared in Cargo.toml, so the benchmark sources must be present
# even though `make build` compiles only the binary.
COPY benches/ benches/

# Changes most often: application sources.
COPY src/ src/

# Optional version string embedded in the binary by build.rs (the .git directory is not part of the
# build context). Declared after the COPY steps so that a new value only re-runs the build step.
ARG GIT_VERSION=""

# Compile. Cargo's registry, git checkouts and the target directory live in BuildKit cache mounts
# that persist across builds: a rebuild recompiles only the crates whose inputs changed, and the
# vendored C libraries are compiled once. Cache mounts are not part of the image and are invisible
# to other stages, so the finished binary is copied out of the mount into /out.
RUN --mount=type=cache,id=haproxy-spoa-firehol-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=haproxy-spoa-firehol-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=haproxy-spoa-firehol-target,target=/app/target,sharing=locked \
    <<'EOT'
set -eu
# Copied files keep their host mtime. Touching this crate's sources guarantees that Cargo rebuilds
# it whenever this step runs, even if a file is older than the previous build kept in the target
# cache. Dependencies are not touched and stay cached.
find src build.rs -type f -exec touch {} +
make build
bin=target/x86_64-unknown-linux-musl/release/haproxy-spoa-ip-reputation-firehol
if ! ldd "$bin" 2>&1 | grep -q 'statically linked'; then
    echo 'ERROR: binary is not statically linked' >&2
    exit 1
fi
install -D -m 0755 "$bin" /out/haproxy-spoa-ip-reputation-firehol
EOT

###############################################################################
# Stage 2: minimal runtime image
###############################################################################
FROM alpine:${ALPINE_VERSION} AS runtime

# ca-certificates: TLS trust store for outbound HTTPS. netcat-openbsd: `nc`, used by the Compose
# health check. The binary is static and needs nothing else.
RUN apk add --no-cache ca-certificates netcat-openbsd

WORKDIR /app

COPY --from=builder /out/haproxy-spoa-ip-reputation-firehol /usr/local/bin/haproxy-spoa-ip-reputation-firehol

# SPOE agent and Prometheus metrics.
EXPOSE 9000 8405

# Defaults use the agent's option names (src/cli.rs); override them at run time. The FireHOL clone
# and the generated database live under /app: mount a volume there to persist them across restarts.
ENV LOG_LEVEL=info \
    SPOA_LISTEN_ADRESS=0.0.0.0:9000 \
    SPOA_LISTEN_ADRESS_METRICS_PROMETHEUS=0.0.0.0:8405 \
    MMDB_PATH=/app/firehol.mmdb \
    FIREHOL_REPO_PATH=/app/firehol-blocklist-ipsets \
    DROP_BY_CATEGORY=abuse

CMD ["/usr/local/bin/haproxy-spoa-ip-reputation-firehol"]
