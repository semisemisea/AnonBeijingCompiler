ARG BASE_IMAGE=ubuntu:22.04
FROM ${BASE_IMAGE}

ARG DEBIAN_FRONTEND=noninteractive
ARG INSTALL_RUST=false

RUN apt-get update && apt-get install -y --no-install-recommends \
    binutils \
    build-essential \
    ca-certificates \
    curl \
    file \
    gdb \
    time \
    && rm -rf /var/lib/apt/lists/*

RUN if [ "${INSTALL_RUST}" = "true" ]; then \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain 1.85.0; \
    fi

ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /app

CMD ["/bin/bash"]
