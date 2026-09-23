#!/usr/bin/env bash
# Fail if the DictFlow DMG does not contain the ad-hoc signed app.
# Tauri must sign the .app before it packs the DMG. A later codesign --deep
# on the loose .app does not update the image users install.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MIN_OS="${MIN_OS:-12.0}"

DMG="${1:-}"
if [ -z "$DMG" ]; then
  DMG="$(find "$ROOT/src-tauri/target" -name 'DictFlow_*.dmg' -print | head -1 || true)"
fi
if [ -z "$DMG" ] || [ ! -f "$DMG" ]; then
  echo "verify-macos-bundle: no DictFlow DMG found" >&2
  exit 1
fi

MOUNT=""
cleanup() {
  if [ -n "$MOUNT" ]; then
    hdiutil detach "$MOUNT" >/dev/null 2>&1 || hdiutil detach -force "$MOUNT" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

ATTACH="$(hdiutil attach -readonly -nobrowse "$DMG")"
# Volume names can contain spaces ("DictFlow 2" when DictFlow is already mounted).
MOUNT="$(printf '%s\n' "$ATTACH" | sed -n 's/.*\(\/Volumes\/.*\)/\1/p' | head -1)"
if [ -z "$MOUNT" ] || [ ! -d "$MOUNT" ]; then
  echo "verify-macos-bundle: could not mount $DMG" >&2
  echo "$ATTACH" >&2
  exit 1
fi

APP="$MOUNT/DictFlow.app"
BIN="$APP/Contents/MacOS/dictflow"
if [ ! -x "$BIN" ]; then
  echo "verify-macos-bundle: missing $BIN" >&2
  exit 1
fi

echo "==> codesign verify $APP"
codesign --verify --strict "$APP"

IDENT="$(codesign -dv "$APP" 2>&1 | awk -F= '/^Identifier=/ { print $2; exit }')"
if [ "$IDENT" != "com.dictflow.app" ]; then
  echo "verify-macos-bundle: identifier is '$IDENT', expected com.dictflow.app" >&2
  exit 1
fi

echo "==> linked libraries"
NON_SYSTEM="$(otool -L "$BIN" | awk 'NR>1 { gsub(/^[[:space:]]+/, ""); print $1 }' | awk '!/^\/System\// && !/^\/usr\/lib\//')"
if [ -n "$NON_SYSTEM" ]; then
  echo "verify-macos-bundle: non-system libraries:" >&2
  echo "$NON_SYSTEM" >&2
  exit 1
fi

echo "==> minimum macOS"
if ! vtool -show-build "$BIN" | awk -v want="$MIN_OS" '$1=="minos" && $2==want { found=1 } END { exit !found }'; then
  echo "verify-macos-bundle: Mach-O minos is not $MIN_OS" >&2
  vtool -show-build "$BIN" >&2 || true
  exit 1
fi

echo "verify-macos-bundle: $DMG ok (identifier $IDENT, minos $MIN_OS)"
