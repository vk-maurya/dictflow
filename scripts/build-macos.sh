#!/usr/bin/env bash
# Build DictFlow for macOS (Apple Silicon by default).
#
# Prerequisites (all free):
#   xcode-select --install          Xcode Command Line Tools
#   brew install node cmake         Node.js 20+ and CMake (for whisper.cpp)
#   curl --proto '=https' --tlsv1.2 https://sh.rustup.rs -sSf | sh
#   rustup target add aarch64-apple-darwin
#
# Optional â€” whisper.cpp sidecar (needed only for the Whisper engine):
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

echo "==> Checking prerequisites"
missing=0
for cmd in node npm npx cargo; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "ERROR: '$cmd' not found on PATH." >&2
    missing=1
  fi
done
if [ "$missing" -ne 0 ]; then
  cat >&2 <<'EOF'
Install the missing tools first:
  xcode-select --install
  brew install node
  curl --proto '=https' --tlsv1.2 https://sh.rustup.rs -sSf | sh
EOF
  exit 1
fi

if ! command -v rustup >/dev/null 2>&1; then
  echo "ERROR: rustup not found. Install Rust first:" >&2
  echo "  curl --proto '=https' --tlsv1.2 https://sh.rustup.rs -sSf | sh" >&2
  exit 1
fi
if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "ERROR: Rust target '$TARGET' is not installed." >&2
  echo "Run:  rustup target add $TARGET" >&2
  exit 1
fi

if ! cargo clippy --version >/dev/null 2>&1; then
  echo "ERROR: cargo clippy is not available. Install the component:" >&2
  echo "  rustup component add clippy" >&2
  exit 1
fi

# Only needed when compiling whisper.cpp from source; the brew whisper-cli
# sidecar does not require it.
command -v cmake >/dev/null 2>&1 || \
  echo "NOTE: cmake not found (only needed if you build whisper.cpp from source)."

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
  echo "Build complete (app bundle not found at expected path â€” check target directory)."
fi
