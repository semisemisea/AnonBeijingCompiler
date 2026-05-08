FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        python3 \
        clang \
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
