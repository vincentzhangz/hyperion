FROM rust:1.97-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    libglib2.0-dev \
    libssl-dev \
    curl \
    ca-certificates \
    git \
    cmake \
    python3 \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH

WORKDIR /workspace
