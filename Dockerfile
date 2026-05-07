# 我们使用这个参数来灵活切换基础镜像
ARG BASE_IMAGE=ubuntu:24.04
FROM ${BASE_IMAGE}

# 1. 换源脚本：支持 22.04 (sources.list) 和 24.04 (ubuntu.sources)
RUN if [ -f /etc/apt/sources.list.d/ubuntu.sources ]; then \
    sed -i 's/archive.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list.d/ubuntu.sources && \
    sed -i 's/security.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list.d/ubuntu.sources && \
    sed -i 's/ports.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list.d/ubuntu.sources; \
    else \
    sed -i 's/archive.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list && \
    sed -i 's/security.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list && \
    sed -i 's/ports.ubuntu.com/mirrors.tuna.tsinghua.edu.cn/g' /etc/apt/sources.list; \
    fi

ENV DEBIAN_FRONTEND=noninteractive

ENV DEBIAN_FRONTEND=noninteractive

# 安装基础工具（两个环境都需要）
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    build-essential \
    curl \
    gdb \
    binutils \
    && rm -rf /var/lib/apt/lists/*

# 如果是 24.04 环境（编译器环境），我们需要 Rust
# 我们通过一个简单的判断来安装
RUN if [ -f /etc/os-release ] && grep -q "24.04" /etc/os-release; then \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.85.0; \
    fi
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /app
