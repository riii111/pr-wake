#!/usr/bin/env bash
# Builds pr-wake and its PR watch engine and installs both into one directory.
# It changes no LaunchAgent; `pr-wake watch install` and `pr-wake review install` do that.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${PREFIX:-$HOME/.local}/bin"
build="$(mktemp -d)"
trap 'rm -rf "$build"' EXIT

(cd "$root/engine" && bun ci && bun run build)
(cd "$root" && go build -o "$build/pr-wake" ./cmd/pr-wake)

mkdir -p "$bin"
# Moved into place, so a running watch sees a new file at its path and restarts on it.
place() {
  install -m 0755 "$1" "$bin/.$2.tmp"
  mv -f "$bin/.$2.tmp" "$bin/$2"
}
place "$root/engine/dist/pr-wake-engine" pr-wake-engine
place "$build/pr-wake" pr-wake
echo "installed pr-wake and pr-wake-engine into $bin"
