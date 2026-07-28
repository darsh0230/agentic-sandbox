#!/bin/sh
set -eu

: "${AGENTIC_SANDBOX_ROOT:=/var/lib/agentic-sandbox}"

mkdir -p \
  "$AGENTIC_SANDBOX_ROOT/state" \
  "$AGENTIC_SANDBOX_ROOT/workspaces" \
  "$AGENTIC_SANDBOX_ROOT/roots" \
  "$AGENTIC_SANDBOX_ROOT/injected" \
  "$AGENTIC_SANDBOX_ROOT/results" \
  "$AGENTIC_SANDBOX_ROOT/tombstones"

if [ "${AGENTIC_SANDBOX_ISOLATION:-privileged}" = "user" ]; then
  chmod 0711 "$AGENTIC_SANDBOX_ROOT"
else
  chmod 0700 "$AGENTIC_SANDBOX_ROOT"
fi
chmod 0700 "$AGENTIC_SANDBOX_ROOT/state" "$AGENTIC_SANDBOX_ROOT/roots" "$AGENTIC_SANDBOX_ROOT/injected" "$AGENTIC_SANDBOX_ROOT/results" "$AGENTIC_SANDBOX_ROOT/tombstones"
chmod 0711 "$AGENTIC_SANDBOX_ROOT/workspaces" || chmod 0700 "$AGENTIC_SANDBOX_ROOT/workspaces"
chmod 0755 /tmp || true
chmod 0755 /var/tmp || true
chmod 0755 /dev/shm || true

exec "$@"
