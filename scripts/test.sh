#!/usr/bin/env bash
# Test runner for zmax-gui: the headless JS bridge tests (always) + the Rust unit tests. The Rust
# bin embeds tauri::generate_context!(), which validates the externalBin sidecars exist at compile
# time — so we stage them first (cheap when already built/cached) and skip the Rust pass if they
# can't be produced (e.g. offline with no zmax build), rather than fail spuriously.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== JS bridge tests =="
node --test crates/zemacs-gui-core/webui/menu.test.cjs

echo "== staging sidecars (needed to compile the Rust bin) =="
node scripts/prepare-stryke-sidecar.mjs || true
ZMAX_NO_BUILD="${ZMAX_NO_BUILD:-}" node scripts/prepare-zmax-sidecar.mjs || true

triple="$(rustc -vV | sed -n 's/^host: //p')"
if [ -f "app/src-tauri/binaries/zmax-${triple}" ] && [ -f "app/src-tauri/binaries/stryke-${triple}" ]; then
    echo "== Rust unit tests =="
    cargo test --manifest-path app/src-tauri/Cargo.toml
else
    echo "== Rust unit tests SKIPPED: sidecars not staged for ${triple} (build crates/zemacs + stage stryke to enable) =="
fi
