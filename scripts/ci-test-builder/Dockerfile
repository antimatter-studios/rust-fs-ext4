FROM debian:12-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
ARG RECIPE_KEY
ARG SOURCE_REPOSITORY
LABEL org.opencontainers.image.source=${SOURCE_REPOSITORY}
LABEL io.antimatter.ext4.builder-key=${RECIPE_KEY}
ENV DEBIAN_FRONTEND=noninteractive CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:${PATH}
# An amd64 builder (CI) also cross-compiles for an aarch64 guest. An arm64
# builder (a Mac's Docker, for its aarch64 guest) compiles natively, and
# Debian ships no aarch64 cross compiler for arm64.
ARG TARGETARCH
RUN case "$TARGETARCH" in \
        amd64) cross="gcc-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
        arm64) cross="" ;; \
        *) echo "unsupported builder architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl git \
    pkg-config python3 python3-yaml ripgrep rsync util-linux $cross \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /opt/ext4-toolchain
COPY rust-toolchain.toml core-toolchain.sh ./
RUN FS_CORE_CALLER="$PWD" bash core-toolchain.sh \
    && rustup target add aarch64-unknown-linux-gnu
