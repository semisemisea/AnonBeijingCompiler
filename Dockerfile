ARG BASE_IMAGE=ubuntu:22.04
FROM ${BASE_IMAGE}

RUN if [ -f /etc/apt/sources.list.d/ubuntu.sources ]; then \
    sed -i 's/archive.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list.d/ubuntu.sources && \
    sed -i 's/security.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list.d/ubuntu.sources && \
    sed -i 's/ports.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list.d/ubuntu.sources; \
    else \
    sed -i 's/archive.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list && \
    sed -i 's/security.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list && \
    sed -i 's/ports.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list; \
    fi

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
