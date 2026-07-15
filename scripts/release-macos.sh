#!/usr/bin/env bash
# release-macos.sh — build the macOS .app, package a .pkg installer + .zip, and
# publish both to the `v<version>` GitHub release. Modeled on zwire's release-macos.sh.
#
#   dist/<Product>-<version>.pkg        pkgbuild component, non-relocatable,
#                                       identifier <bundle id>, installs to /Applications
#   dist/<Product>-<version>-macos.zip  the bundle itself (<Product>.app at the archive root)
#
# The .app is ad-hoc signed (no Developer ID / notarization) — first launch needs
# right-click -> Open, matching the local install. This script only packages + publishes.
#
# Version is read from package.json; the git tag `v<version>` must already exist
# (bump + commit + tag first). Refuses to overwrite an existing release.
#
#   scripts/release-macos.sh [--dry-run]   (--dry-run: build + package, do NOT publish)
#
# Requires: macOS, cargo, pnpm, gh (authenticated), pkgbuild/ditto.
set -euo pipefail
cd "$(dirname "$0")/.."

DRY=0
[ "${1:-}" = "--dry-run" ] && DRY=1

CONF=""
for c in app/src-tauri/tauri.conf.json src-tauri/tauri.conf.json; do
  [ -f "$c" ] && { CONF="$c"; break; }
done
[ -n "$CONF" ] || CONF="$(find . -name tauri.conf.json -path '*src-tauri*' -not -path '*/target/*' -not -path '*/node_modules/*' 2>/dev/null | head -1)"
[ -n "$CONF" ] || { echo "release-macos: tauri.conf.json not found" >&2; exit 1; }
PROD="$(node -p "require('./$CONF').productName")"
ID="$(node -p "require('./$CONF').identifier")"
VERSION="$(node -p "require('./package.json').version")"
TAG="v$VERSION"

[ "$(uname -s)" = Darwin ] || { echo "release-macos: macOS-only (produces .app/.pkg)" >&2; exit 1; }
command -v pkgbuild >/dev/null || { echo "release-macos: pkgbuild not found (Xcode command line tools)" >&2; exit 1; }

if [ "$DRY" = 0 ]; then
  command -v gh >/dev/null || { echo "release-macos: gh not found — install the GitHub CLI and 'gh auth login'" >&2; exit 1; }
  REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
  git rev-parse -q --verify "refs/tags/$TAG" >/dev/null \
    || { echo "release-macos: tag $TAG does not exist — bump package.json, commit, then: git tag $TAG && git push origin $TAG" >&2; exit 1; }
  if gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
    echo "release-macos: release $TAG already exists — refusing to overwrite" >&2; exit 1
  fi
  git ls-remote --exit-code --tags origin "$TAG" >/dev/null 2>&1 || git push origin "$TAG"
fi

OUT="dist"; rm -rf "$OUT"; mkdir -p "$OUT/stage"
STAGE_APP="$OUT/stage/$PROD.app"
# Reuse localinstall.sh's tested build+bundle, redirected to a writable staging dir
# via $LOCALINSTALL_DEST so no sudo / no touching /Applications. stage/ then holds
# exactly one thing — <Product>.app — which is what pkgbuild --root expects.
LOCALINSTALL_DEST="$PWD/$STAGE_APP" bash scripts/localinstall.sh
[ -d "$STAGE_APP" ] || { echo "release-macos: staged app missing: $STAGE_APP" >&2; exit 1; }

PKG="$OUT/${PROD}-${VERSION}.pkg"
PLIST="$OUT/component.plist"
# pkgbuild defaults an app bundle to relocatable=true; force non-relocatable so it
# always installs to /Applications.
pkgbuild --analyze --root "$OUT/stage" "$PLIST" >/dev/null
/usr/libexec/PlistBuddy -c "Set :0:BundleIsRelocatable false" "$PLIST"
pkgbuild --root "$OUT/stage" --component-plist "$PLIST" \
  --identifier "$ID" --version "$VERSION" \
  --install-location /Applications "$PKG" >/dev/null
echo "release-macos: pkg  -> $PKG ($(du -h "$PKG" | awk '{print $1}'))"

ZIP="$OUT/${PROD}-${VERSION}-macos.zip"
ditto -c -k --sequesterRsrc --keepParent "$STAGE_APP" "$ZIP"
echo "release-macos: zip  -> $ZIP ($(du -h "$ZIP" | awk '{print $1}'))"

if [ "$DRY" = 1 ]; then
  echo "release-macos: DRY RUN — artifacts built, release NOT created"
  exit 0
fi

NOTES="$OUT/notes.md"
PREV="$(git tag --list 'v*' --sort=-version:refname | grep -vx "$TAG" | head -1 || true)"
{
  echo "### $PROD $VERSION (macOS)"
  echo
  if [ -n "$PREV" ]; then
    echo "Changes since $PREV:"; echo
    git log --no-merges --pretty='- %s' "$PREV..$TAG"
  else
    git log --no-merges --pretty='- %s' -20 "$TAG"
  fi
} > "$NOTES"

gh release create "$TAG" -R "$REPO" \
  --title "$PROD $VERSION (macOS)" \
  --notes-file "$NOTES" \
  "$PKG" "$ZIP"
echo "release-macos: published https://github.com/$REPO/releases/tag/$TAG"
