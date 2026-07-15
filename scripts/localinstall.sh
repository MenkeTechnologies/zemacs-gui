#!/usr/bin/env bash
# Build zemacs-gui and FORCE-install the freshly built artifact locally:
#   * a .app bundle  -> /Applications
#   * a plain binary -> ~/.cargo/bin
# Force-syncs nested submodules to their pinned commits first so the build always
# has its inputs, then overwrites any previously installed copy. Unsigned, live now.
set -euo pipefail
cd "$(dirname "$0")/.."

PRODUCT="zemacs-gui"

# bundle_dmg.sh mounts a scratch rw.*.dmg, populates it, then converts it to the final read-only DMG.
# An interrupted build leaves that scratch image ATTACHED — the next `hdiutil attach` then fails and
# the whole build aborts at the DMG step. Detach any of our images still mounted and delete the scratch
# files so each run starts from a clean slate. macOS-only (no hdiutil elsewhere); safe to skip.
cleanup_stale_dmg() {
  [ "$(uname -s)" = "Darwin" ] || return 0
  if command -v hdiutil >/dev/null 2>&1; then
    hdiutil info 2>/dev/null | awk -v p="$PRODUCT" '
      /^image-path/       { m = ($0 ~ p) }
      /^\/dev\/disk[0-9]+[[:space:]]/ && m { print $1 }
    ' | while read -r dev; do
      echo "// detaching stale DMG mount $dev"
      hdiutil detach "$dev" -force >/dev/null 2>&1 || true
    done
  fi
  command rm -f app/src-tauri/target/release/bundle/macos/rw.*.dmg 2>/dev/null || true
}
cleanup_stale_dmg

echo "// force-syncing nested submodules to latest main …"
git submodule sync --recursive >/dev/null 2>&1 || true
git submodule update --init --recursive --force >/dev/null 2>&1 || true
# Advance every nested submodule to its latest origin/main (pinned commits can be
# stale and miss build inputs); tolerate submodules that lack a main branch.
git submodule foreach --recursive \
  'git fetch -q origin main 2>/dev/null && git reset -q --hard origin/main 2>/dev/null || true' \
  >/dev/null 2>&1 || true

echo "// building release bundle for $PRODUCT …"
pnpm run build

# Prefer a .app bundle (Tauri / JUCE Standalone) -> /Applications; newest wins.
APP="$(find . -type d -name "$PRODUCT.app" -not -path '*/node_modules/*' \
  \( -path '*/release/bundle/macos/*' -o -path '*/Release/Standalone/*' \) \
  -exec stat -f '%m %N' {} \; 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)"

if [ -n "$APP" ] && [ -d "$APP" ]; then
  if [ "$(uname -s)" != "Darwin" ]; then
    echo "localinstall: .app deploy is macOS-only" >&2; exit 1
  fi
  DEST="${LOCALINSTALL_DEST:-/Applications/$PRODUCT.app}"
  osascript -e "quit app \"$PRODUCT\"" >/dev/null 2>&1 || true
  sleep 1
  [ -e "$DEST" ] && command rm -rf "$DEST"
  command cp -fRp "$APP" "$DEST"
  echo "localinstall: installed $APP -> $DEST ($(du -sh "$APP" | awk '{print $1}'))"
  exit 0
fi

# Fall back to a release binary -> ~/.cargo/bin.
BIN="$(find . -type f -perm +111 -name "$PRODUCT" -path '*/release/*' \
  -not -path '*/deps/*' -not -path '*/build/*' 2>/dev/null | head -1)"
if [ -n "$BIN" ] && [ -f "$BIN" ]; then
  mkdir -p "$HOME/.cargo/bin"
  command install -m 755 "$BIN" "$HOME/.cargo/bin/$PRODUCT"
  echo "localinstall: installed $BIN -> $HOME/.cargo/bin/$PRODUCT"
  exit 0
fi

echo "localinstall: no $PRODUCT.app or $PRODUCT binary found after build" >&2
exit 1
