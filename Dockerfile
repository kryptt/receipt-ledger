# syntax=docker/dockerfile:1
FROM rust:1-alpine AS builder

# TARGETPLATFORM is set automatically by BuildKit (e.g. linux/amd64,
# linux/arm64). We declare it as an ARG so the cache-mount `id`s below can
# interpolate it — keeping per-arch cargo target directories separate so a
# parallel amd64+arm64 release build does not trample each other's artifacts.
ARG TARGETPLATFORM

# musl-dev for the Rust toolchain; cmake/clang/perl/make/g++/linux-headers are
# required to build aws-lc-sys, which is pulled transitively by jmap-client's
# `reqwest/rustls` feature even though our own TLS path uses ring. (reqwest
# 0.13's `rustls` feature unconditionally enables the aws-lc-rs backend; feature
# unification means we cannot opt out of it while depending on jmap-client.)
RUN apk add --no-cache musl-dev cmake make perl clang g++ linux-headers

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY tests/ tests/

# Resolve the native musl target triple so the same Dockerfile builds on amd64
# and arm64 hosts (e.g. under QEMU emulation in CI). rustc prints lines like
# "host: x86_64-unknown-linux-musl"; grab that.
RUN rustc -vV | awk '/^host:/ {print $2}' > /tmp/target-triple && \
    cat /tmp/target-triple

# Three persistent BuildKit cache mounts each stage:
#
#   cargo-registry  — index + downloaded crate tarballs (platform-agnostic;
#                     id unscoped, sharing=shared so both arches read together)
#   cargo-git       — git-dependency checkouts (same shape)
#   receipt-ledger-target-${TARGETPLATFORM}
#                   — compiled artifacts; per-arch because the .rlib bits in
#                     target/<triple>/release differ per arch. sharing=locked
#                     because cargo holds a build-lock on the target dir.

FROM builder AS test
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=shared \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=shared \
    --mount=type=cache,target=/build/target,id=receipt-ledger-target-${TARGETPLATFORM},sharing=locked \
    TARGET="$(cat /tmp/target-triple)" && \
    cargo test --target "$TARGET"

FROM builder AS release
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=shared \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=shared \
    --mount=type=cache,target=/build/target,id=receipt-ledger-target-${TARGETPLATFORM},sharing=locked \
    TARGET="$(cat /tmp/target-triple)" && \
    cargo build --release --target "$TARGET" && \
    cp "target/$TARGET/release/receipt-ledger" /receipt-ledger
RUN mkdir -p /empty-tmp

FROM scratch

COPY --from=release /empty-tmp /tmp
COPY --from=release /receipt-ledger /receipt-ledger

# One-shot job, not a server — no EXPOSE. Runs once and exits with a status
# code the Kubernetes CronJob uses to fire CronJobFailing on real errors.
ENTRYPOINT ["/receipt-ledger"]
