#!/usr/bin/env bash
# Build zemacs-gui and deploy the freshly built .app straight into /Applications
# (local, unsigned install) so the change is live immediately — no .dmg drag.
set -euo pipefail
cd "$(dirname "$0")/.."

PRODUCT="zemacs-gui"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "localinstall: macOS-only (.app bundle deploy)" >&2
  exit 1
fi

echo "// building release bundle for $PRODUCT …"
pnpm run build

# Locate the freshly built .app (Tauri bundle or JUCE Standalone); newest wins.
BUILT="$(find . -type d -name "$PRODUCT.app" -not -path '*/node_modules/*' \
  \( -path '*/release/bundle/macos/*' -o -path '*/Release/Standalone/*' \) \
  -exec stat -f '%m %N' {} \; 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)"
if [ -z "$BUILT" ] || [ ! -d "$BUILT" ]; then
  echo "localinstall: $PRODUCT.app not found after build" >&2
  exit 1
fi

DEST="/Applications/$PRODUCT.app"
osascript -e "quit app \"$PRODUCT\"" >/dev/null 2>&1 || true
sleep 1
[ -e "$DEST" ] && command rm -rf "$DEST"
command cp -fRp "$BUILT" "$DEST"
echo "localinstall: installed $BUILT -> $DEST ($(du -sh "$BUILT" | awk '{print $1}'))"
