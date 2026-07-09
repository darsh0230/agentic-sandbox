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

chmod 0700 "$PI_SANDBOX_ROOT"
chmod 0700 "$PI_SANDBOX_ROOT/state" "$PI_SANDBOX_ROOT/roots" "$PI_SANDBOX_ROOT/injected" "$PI_SANDBOX_ROOT/results" "$PI_SANDBOX_ROOT/tombstones"
chmod 0711 "$PI_SANDBOX_ROOT/workspaces" || chmod 0700 "$PI_SANDBOX_ROOT/workspaces"

exec "$@"
