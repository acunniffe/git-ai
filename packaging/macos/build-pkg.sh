#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: build-pkg.sh --binary <path> --arch <x64|arm64> --version <version> --output <path>
USAGE
}

BINARY=""
ARCH=""
VERSION=""
OUTPUT=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --binary) BINARY="${2:-}"; shift 2 ;;
    --arch) ARCH="${2:-}"; shift 2 ;;
    --version) VERSION="${2:-}"; shift 2 ;;
    --output) OUTPUT="${2:-}"; shift 2 ;;
    *) usage; exit 2 ;;
  esac
done

[ -n "$BINARY" ] && [ -n "$ARCH" ] && [ -n "$VERSION" ] && [ -n "$OUTPUT" ] || { usage; exit 2; }
[ -f "$BINARY" ] || { echo "binary not found: $BINARY" >&2; exit 1; }

case "$ARCH" in
  x64|arm64) ;;
  *) echo "unsupported arch: $ARCH" >&2; exit 2 ;;
esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK_DIR="$ROOT/target/package/pkg-$ARCH"
SCRIPTS="$WORK_DIR/scripts"
COMPONENT_PKG="$WORK_DIR/git-ai-component.pkg"
OUTPUT_ABS="$(python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$OUTPUT")"

rm -rf "$WORK_DIR"
mkdir -p "$SCRIPTS" "$(dirname "$OUTPUT_ABS")"
install -m 0755 "$BINARY" "$SCRIPTS/git-ai"
install -m 0755 "$ROOT/packaging/macos/scripts/preinstall" "$SCRIPTS/preinstall"
install -m 0755 "$ROOT/packaging/macos/scripts/postinstall" "$SCRIPTS/postinstall"
xattr -cr "$SCRIPTS" 2>/dev/null || true

pkgbuild \
  --nopayload \
  --scripts "$SCRIPTS" \
  --identifier "com.git-ai.git-ai" \
  --version "$VERSION" \
  --install-location "/" \
  "$COMPONENT_PKG"

if [ -n "${APPLE_DEVELOPER_ID_INSTALLER_IDENTITY:-}" ]; then
  productsign \
    --sign "$APPLE_DEVELOPER_ID_INSTALLER_IDENTITY" \
    "$COMPONENT_PKG" \
    "$OUTPUT_ABS"
else
  cp "$COMPONENT_PKG" "$OUTPUT_ABS"
fi

echo "Built PKG: $OUTPUT_ABS"
