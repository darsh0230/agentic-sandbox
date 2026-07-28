# syntax=docker/dockerfile:1.7

FROM rust:bookworm AS build
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libseccomp-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY ui ./ui

RUN cargo build --locked --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        coreutils \
        libseccomp2 \
        passwd \
        tini \
        util-linux \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    for idx in $(seq 1 4); do \
        uid=$((10000 + idx)); \
        name=$(printf 'agent%04d' "$idx"); \
        groupadd --gid "$uid" "$name"; \
        useradd \
            --uid "$uid" \
            --gid "$uid" \
            --home-dir /nonexistent \
            --no-create-home \
            --shell /usr/sbin/nologin \
            "$name"; \
    done

COPY --from=build /app/target/release/agentic-sandbox /usr/local/bin/agentic-sandbox
COPY ops/entrypoint.sh /usr/local/bin/agentic-sandbox-entrypoint

RUN chmod 0755 /usr/local/bin/agentic-sandbox-entrypoint \
    && mkdir -p \
        /var/lib/agentic-sandbox/state \
        /var/lib/agentic-sandbox/workspaces \
        /var/lib/agentic-sandbox/roots \
        /var/lib/agentic-sandbox/injected \
        /var/lib/agentic-sandbox/results \
        /var/lib/agentic-sandbox/tombstones \
    && chmod 0700 /var/lib/agentic-sandbox \
    && chmod 0700 \
        /var/lib/agentic-sandbox/state \
        /var/lib/agentic-sandbox/roots \
        /var/lib/agentic-sandbox/injected \
        /var/lib/agentic-sandbox/results \
        /var/lib/agentic-sandbox/tombstones \
    && chmod 0711 /var/lib/agentic-sandbox/workspaces \
    && chmod 0755 /tmp /var/tmp

ENV AGENTIC_SANDBOX_ROOT=/var/lib/agentic-sandbox
ENV RUST_LOG=agentic_sandbox=info

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/agentic-sandbox-entrypoint"]
CMD ["/usr/local/bin/agentic-sandbox", "serve"]
