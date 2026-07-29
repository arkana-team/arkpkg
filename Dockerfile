FROM rust:1.85-slim-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    build-essential \
    tar \
    liblz4-tool \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add rustfmt clippy

WORKDIR /workspace/arkpkg

RUN mkdir -p /workspace/fake_root/etc/arkpkg/packages /workspace/fake_root/var/log

ENV ARKPKG_ROOT=/workspace/fake_root

CMD ["bash"]
