#!/usr/bin/env bash
# Builds pr-wake and its PR watch engine and installs both into one directory.
# It changes no LaunchAgent; `pr-wake watch install` and `pr-wake review install` do that.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${PREFIX:-$HOME/.local}/bin"
build="$(mktemp -d)"
trap 'rm -rf "$build"' EXIT

# A CARGO_TARGET_DIR set for other work would move the engine away from the path placed below, so the target is fixed.
(cd "$root" && cargo build --locked --release --target-dir "$root/target")
(cd "$root" && go build -o "$build/pr-wake" ./cmd/pr-wake)

mkdir -p "$bin"
# Moved into place, so a running watch sees a new file at its path and restarts on it.
place() {
  install -m 0755 "$1" "$bin/.$2.tmp"
  mv -f "$bin/.$2.tmp" "$bin/$2"
}
place "$root/target/release/pr-wake-engine" pr-wake-engine
place "$build/pr-wake" pr-wake
echo "installed pr-wake and pr-wake-engine into $bin"
