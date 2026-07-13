#!/bin/sh
set -eu

: "${PI_SANDBOX_ROOT:=/var/lib/pi-sandbox}"

mkdir -p \
  "$PI_SANDBOX_ROOT/state" \
  "$PI_SANDBOX_ROOT/workspaces" \
  "$PI_SANDBOX_ROOT/roots" \
  "$PI_SANDBOX_ROOT/injected" \
  "$PI_SANDBOX_ROOT/results" \
  "$PI_SANDBOX_ROOT/tombstones"

if [ "${PI_SANDBOX_ISOLATION:-privileged}" = "user" ]; then
  chmod 0711 "$PI_SANDBOX_ROOT"
else
  chmod 0700 "$PI_SANDBOX_ROOT"
fi
chmod 0700 "$PI_SANDBOX_ROOT/state" "$PI_SANDBOX_ROOT/roots" "$PI_SANDBOX_ROOT/injected" "$PI_SANDBOX_ROOT/results" "$PI_SANDBOX_ROOT/tombstones"
chmod 0711 "$PI_SANDBOX_ROOT/workspaces" || chmod 0700 "$PI_SANDBOX_ROOT/workspaces"
chmod 0755 /tmp || true
chmod 0755 /var/tmp || true
chmod 0755 /dev/shm || true

exec "$@"
