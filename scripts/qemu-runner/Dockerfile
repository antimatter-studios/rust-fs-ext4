ARG BASE_IMAGE=debian:12-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
FROM ${BASE_IMAGE}
ARG ENVIRONMENT_ID
ARG SOURCE_REPOSITORY
LABEL org.opencontainers.image.source=${SOURCE_REPOSITORY}
LABEL io.antimatter.ext4.runner-key=${ENVIRONMENT_ID}
ENV DEBIAN_FRONTEND=noninteractive RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:${PATH} QEMU_RUNNER_ENVIRONMENT=1
COPY packages.txt setup.sh rust-toolchain.toml core-toolchain.sh environment.json /opt/ext4-runner-recipe/
RUN RUNNER_RECIPE_KEY=${ENVIRONMENT_ID} bash /opt/ext4-runner-recipe/setup.sh
CMD ["bash"]
