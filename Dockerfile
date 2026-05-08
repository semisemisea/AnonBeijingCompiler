FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN sed -i "s@http://.*archive.ubuntu.com@http://mirrors.pku.edu.cn@g" /etc/apt/sources.list.d/ubuntu.sources && \
  sed -i "s@http://.*security.ubuntu.com@http://mirrors.pku.edu.cn@g" /etc/apt/sources.list.d/ubuntu.sources && \
  sed -i "s@http://.*ports.ubuntu.com@http://mirrors.pku.edu.cn@g" /etc/apt/sources.list.d/ubuntu.sources

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        python3 \
        clang \
        gdb-multiarch \
        lld \
        llvm \
        gcc-aarch64-linux-gnu \
        qemu-user-static \
        libc6-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work

COPY ./tests/test.py /usr/local/bin/soyo-test
RUN chmod +x /usr/local/bin/soyo-test

ENTRYPOINT ["/usr/local/bin/soyo-test"]
