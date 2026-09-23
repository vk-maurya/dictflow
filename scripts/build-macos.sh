#!/usr/bin/env bash
# Build DictFlow for macOS (Apple Silicon by default).
#
# Prerequisites (all free):
#   xcode-select --install          Xcode Command Line Tools
#   brew install node cmake         Node.js 20+ and CMake (for whisper.cpp)
#   curl --proto '=https' --tlsv1.2 https://sh.rustup.rs -sSf | sh
#   rustup target add aarch64-apple-darwin
#
# Optional — whisper.cpp sidecar (needed only for the Whisper engine):
#   brew install whisper-cpp
#   mkdir -p ~/Library/Application\ Support/com.dictflow.app/dictflow/bin
#   cp /opt/homebrew/bin/whisper-cli \
#      ~/Library/Application\ Support/com.dictflow.app/dictflow/bin/
#
# Usage:
#   bash scripts/build-macos.sh              # Apple Silicon (default)
#   ARCH=x86_64 bash scripts/build-macos.sh  # Intel Mac
#   ARCH=universal bash scripts/build-macos.sh # Universal binary (both)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ARCH="${ARCH:-aarch64}"

case "$ARCH" in
  aarch64)   TARGET="aarch64-apple-darwin" ;;
  x86_64)    TARGET="x86_64-apple-darwin" ;;
  universal) TARGET="universal-apple-darwin" ;;
  *)         echo "Unknown ARCH=$ARCH (use aarch64, x86_64, or universal)"; exit 1 ;;
esac

echo "==> Building DictFlow for $TARGET"

echo "==> Installing Node dependencies"
npm ci

echo "==> Building frontend"
npm run build

echo "==> Running Clippy"
(cd src-tauri && cargo clippy --locked -- -D warnings)

echo "==> Running tests"
(cd src-tauri && cargo test --locked)

echo "==> Building Tauri app for $TARGET"
npx tauri build --target "$TARGET"

APP=$(find "src-tauri/target/${TARGET}/release/bundle/macos" -name "*.app" 2>/dev/null | head -1)
if [ -z "$APP" ]; then
  # Universal builds land in a different path
  APP=$(find "src-tauri/target/release/bundle/macos" -name "*.app" 2>/dev/null | head -1)
fi

if [ -n "$APP" ]; then
  echo "==> Verifying the app inside the DMG"
  bash "$ROOT/scripts/verify-macos-bundle.sh"

  echo ""
  echo "Build complete: $APP"
  echo ""
  echo "A local build has no quarantine and opens directly."
  echo "A downloaded DMG still needs Gatekeeper cleared once:"
  echo "  xattr -dr com.apple.quarantine /Applications/DictFlow.app"
  echo "Or System Settings -> Privacy & Security -> Open Anyway."
else
  echo "Build complete (app bundle not found at expected path — check target directory)."
fi
