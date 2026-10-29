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

# bookworm-slim tags lag behind published Debian security updates; pull them in.
RUN apt-get update \
    && apt-get upgrade -y --no-install-recommends \
    && apt-get install -y --no-install-recommends \
    ca-certificates bash coreutils grep sed gawk findutils jq file procps psmisc python3 python3-venv \
    python3-pip git curl openssh-client libseccomp2 util-linux iproute2 netcat-openbsd tini \
    ripgrep xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && pip3 install --no-cache-dir --break-system-packages \
    "requests==2.34.2" "urllib3>=2.8.0" "certifi>=2026.10.18"

# Pinned agent install: fixed release tag plus per-architecture SHA256, never
# a moving channel manifest. Bump ANTE_VERSION and both digests together.
ARG ANTE_VERSION=v0.preview.87
ARG ANTE_SHA256_LINUX_AMD64=ad49386d2db0ee43c76869d320c2a51bee49918cb762ac9cc994f1eddd93355e
ARG ANTE_SHA256_LINUX_ARM64=f68f84836ce243707d3195701b01e3f0145b63c9a3a5a730251e5f0577f8ee38
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
      amd64) asset="ante-${ANTE_VERSION}-linux-x86_64-musl.tar.gz"; sha="$ANTE_SHA256_LINUX_AMD64" ;; \
      arm64) asset="ante-${ANTE_VERSION}-linux-arm64-musl.tar.gz"; sha="$ANTE_SHA256_LINUX_ARM64" ;; \
      *) echo "unsupported architecture: $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/ante.tar.gz "https://github.com/AntigmaLabs/ante/releases/download/${ANTE_VERSION}/${asset}"; \
    echo "${sha}  /tmp/ante.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/ante.tar.gz -C /usr/local/bin ante; \
    rm /tmp/ante.tar.gz; \
    ante --version

ARG NPM_VERSION=12.2.1
ARG PI_CODING_AGENT_VERSION=1.4.0
ARG OPENCODE_VERSION=1.19.5
# Override pi-coding-agent's pinned transitive deps with patched releases.
# vitest pins its own nested @vitest/mocker copy, so it is removed below after
# hoisting the patched one.
ARG UNDICI_VERSION=8.12.3
ARG VITEST_MOCKER_VERSION=4.1.12
# npm bundles pinned copies of these deps in its own tree, which npm install
# cannot patch; swap them via tarballs.
ARG NPM_BUNDLED_UNDICI_VERSION=6.29.1
ARG NPM_BUNDLED_IP_ADDRESS_VERSION=10.7.4
RUN set -eux; \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash -; \
    apt-get install -y --no-install-recommends nodejs; \
    node -v; \
    npm install -g "npm@${NPM_VERSION}"; \
    npm -v; \
    npm pack --ignore-scripts "undici@${NPM_BUNDLED_UNDICI_VERSION}" --pack-destination /tmp; \
    npm pack --ignore-scripts "ip-address@${NPM_BUNDLED_IP_ADDRESS_VERSION}" --pack-destination /tmp; \
    rm -rf /usr/lib/node_modules/npm/node_modules/undici /usr/lib/node_modules/npm/node_modules/ip-address; \
    mkdir -p /usr/lib/node_modules/npm/node_modules/undici /usr/lib/node_modules/npm/node_modules/ip-address; \
    tar -xzf "/tmp/undici-${NPM_BUNDLED_UNDICI_VERSION}.tgz" --strip-components=1 -C /usr/lib/node_modules/npm/node_modules/undici; \
    tar -xzf "/tmp/ip-address-${NPM_BUNDLED_IP_ADDRESS_VERSION}.tgz" --strip-components=1 -C /usr/lib/node_modules/npm/node_modules/ip-address; \
    rm "/tmp/undici-${NPM_BUNDLED_UNDICI_VERSION}.tgz" "/tmp/ip-address-${NPM_BUNDLED_IP_ADDRESS_VERSION}.tgz"; \
    npm -v; \
    npm install -g --ignore-scripts "@earendil-works/pi-coding-agent@${PI_CODING_AGENT_VERSION}"; \
    npm install -g "opencode-ai@${OPENCODE_VERSION}"; \
    cd /usr/lib/node_modules/@earendil-works/pi-coding-agent/; \
    npm install --no-save --ignore-scripts "undici@${UNDICI_VERSION}" "@vitest/mocker@${VITEST_MOCKER_VERSION}"; \
    rm -rf /usr/lib/node_modules/@earendil-works/pi-coding-agent/node_modules/vitest/node_modules/@vitest/mocker; \
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

STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/agentic-sandbox-entrypoint"]
CMD ["/usr/local/bin/agentic-sandbox", "serve"]
