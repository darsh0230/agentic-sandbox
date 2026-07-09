# syntax=docker/dockerfile:1.7

FROM rust:bookworm AS build
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libseccomp-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY ui ./ui

RUN cargo build --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates bash coreutils grep sed gawk findutils procps psmisc python3 python3-venv \
       python3-pip git curl openssh-client libseccomp2 util-linux iproute2 netcat-openbsd tini \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/pi-sandbox-master /usr/local/bin/pi-sandbox-master
COPY ops/entrypoint.sh /usr/local/bin/pi-sandbox-entrypoint

RUN chmod 0755 /usr/local/bin/pi-sandbox-entrypoint \
    && mkdir -p /var/lib/pi-sandbox/state /var/lib/pi-sandbox/workspaces /var/lib/pi-sandbox/roots \
       /var/lib/pi-sandbox/injected /var/lib/pi-sandbox/results /var/lib/pi-sandbox/tombstones \
    && chmod 0700 /var/lib/pi-sandbox

ENV RUST_LOG=info
ENV PI_SANDBOX_ROOT=/var/lib/pi-sandbox

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/pi-sandbox-entrypoint"]
CMD ["/usr/local/bin/pi-sandbox-master", "serve", "--config", "/etc/pi-sandbox/config.toml"]
