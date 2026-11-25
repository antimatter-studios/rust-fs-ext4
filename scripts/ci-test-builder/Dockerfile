FROM debian:12-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
ARG RECIPE_KEY
ARG SOURCE_REPOSITORY
LABEL org.opencontainers.image.source=${SOURCE_REPOSITORY}
LABEL io.antimatter.ext4.builder-key=${RECIPE_KEY}
ENV DEBIAN_FRONTEND=noninteractive CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:${PATH}
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl gcc-aarch64-linux-gnu git libc6-dev-arm64-cross \
    pkg-config python3 python3-yaml ripgrep rsync util-linux \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /opt/ext4-toolchain
COPY rust-toolchain.toml core-toolchain.sh ./
RUN FS_CORE_CALLER="$PWD" bash core-toolchain.sh \
    && rustup target add aarch64-unknown-linux-gnu
