# syntax=docker/dockerfile:1
# Docker Hub 官方 ubuntu:24.04 multi-arch index，固定于 2026-08-26。
FROM ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_VERSION=1.98.0
ARG CARGO_DEB_VERSION=3.7.0
ARG CARGO_GENERATE_RPM_VERSION=0.21.0
ARG CARGO_AUDIT_VERSION=0.22.0
ARG CARGO_DENY_VERSION=0.19.0

ENV TZ=UTC \
    LC_ALL=C.UTF-8 \
    LANG=C.UTF-8 \
    CARGO_INCREMENTAL=0 \
    RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        bash \
        build-essential \
        ca-certificates \
        cmake \
        cpio \
        curl \
        dpkg-dev \
        file \
        git \
        groff \
        lintian \
        make \
        openssl \
        pkg-config \
        python3 \
        rpm \
        tar \
        util-linux \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
        --output /tmp/rustup-init.sh https://sh.rustup.rs \
    && sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal \
        --default-toolchain "$RUST_VERSION" \
    && rm -f /tmp/rustup-init.sh \
    && rustup component add --toolchain "$RUST_VERSION" clippy rustfmt \
    && cargo install cargo-deb --version "$CARGO_DEB_VERSION" --locked \
    && cargo install cargo-generate-rpm --version "$CARGO_GENERATE_RPM_VERSION" --locked \
    && cargo install cargo-audit --version "$CARGO_AUDIT_VERSION" --locked \
    && cargo install cargo-deny --version "$CARGO_DENY_VERSION" --locked \
    && rustup --version \
    && rustup run "$RUST_VERSION" rustc --version \
    && cargo-deb --version \
    && cargo-generate-rpm --version \
    && cargo-audit --version \
    && cargo-deny --version

RUN useradd --create-home --uid 10001 --shell /bin/bash release \
    && mkdir -p /workspace /artifacts \
    && chown release:release /workspace /artifacts

USER release
WORKDIR /workspace
