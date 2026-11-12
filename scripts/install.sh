#!/usr/bin/env bash
# Builds pr-wake and installs it with the pr-wake-engine link beside it.
# It changes no LaunchAgent; `pr-wake watch install` and `pr-wake review install` do that.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${PREFIX:-$HOME/.local}/bin"

# A CARGO_TARGET_DIR set for other work would move the binary away from the path placed below, so the target is fixed.
(cd "$root" && cargo build --locked --release --target-dir "$root/target")

mkdir -p "$bin"
# Moved into place, so a running agent sees a new file at its path and restarts on it.
install -m 0755 "$root/target/release/pr-wake" "$bin/.pr-wake.tmp"
mv -f "$bin/.pr-wake.tmp" "$bin/pr-wake"
# harnexus-crew starts the pr-wake-engine beside pr-wake for a LaunchAgent that still runs harnexus-task watch.
ln -sfn pr-wake "$bin/.pr-wake-engine.tmp"
mv -f "$bin/.pr-wake-engine.tmp" "$bin/pr-wake-engine"
echo "installed pr-wake and its pr-wake-engine link into $bin"
