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
    ca-certificates bash coreutils grep sed gawk findutils jq file procps psmisc python3 python3-venv \
    python3-pip git curl openssh-client libseccomp2 util-linux iproute2 netcat-openbsd tini \
    ripgrep xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && pip3 install requests --break-system-packages

RUN set -eux; \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash -; \
    apt-get install -y --no-install-recommends nodejs; \
    node -v; \
    npm install -g --ignore-scripts @earendil-works/pi-coding-agent; \
    npm install -g opencode-ai; \
    cd /usr/lib/node_modules/opencode-ai/; \
    node postinstall.mjs; \
    npm cache clean --force; \
    rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    for idx in $(seq 1 64); do \
    uid=$((10000 + idx)); \
    name=$(printf 'agent%04d' "$idx"); \
    groupadd --gid "$uid" "$name"; \
    useradd --uid "$uid" --gid "$uid" --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin "$name"; \
    done

COPY --from=build /app/target/release/agentic-sandbox /usr/local/bin/agentic-sandbox
COPY ops/entrypoint.sh /usr/local/bin/agentic-sandbox-entrypoint

RUN chmod 0755 /usr/local/bin/agentic-sandbox-entrypoint \
    && mkdir -p /var/lib/agentic-sandbox/state /var/lib/agentic-sandbox/workspaces /var/lib/agentic-sandbox/roots \
    /var/lib/agentic-sandbox/injected /var/lib/agentic-sandbox/results /var/lib/agentic-sandbox/tombstones \
    && chmod 0700 /var/lib/agentic-sandbox \
    && chmod 0700 /var/lib/agentic-sandbox/state /var/lib/agentic-sandbox/roots /var/lib/agentic-sandbox/injected /var/lib/agentic-sandbox/results /var/lib/agentic-sandbox/tombstones \
    && chmod 0711 /var/lib/agentic-sandbox/workspaces \
    && chmod 0755 /tmp /var/tmp

RUN chmod 0755 /dev/shm || true

ENV RUST_LOG=info
ENV AGENTIC_SANDBOX_ROOT=/var/lib/agentic-sandbox

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/agentic-sandbox-entrypoint"]
CMD ["/usr/local/bin/agentic-sandbox", "serve"]
