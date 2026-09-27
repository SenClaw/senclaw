#!/usr/bin/env bash
# Build {{title_name}} and assemble an installable Space-App zip.
#
#   release/                 <- staged, flat install layout
#     {{crate_name}}         (release binary; manifest runtime.start = ./{{crate_name}})
#     senclaw-manifest.json
#     web/                   (static UI, served next to the binary)
#   {{id}}-app.zip           <- the artifact you install in SenClaw
#
# The layout is flat on purpose: the daemon unpacks the zip into the app
# directory and runs `runtime.start` from there, so anything nested one level
# deeper is simply not found.
#
# Usage: ./scripts/pack.sh [--skip-build]
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")/.." && pwd)"
REL="$APP_DIR/release"
ZIP="$APP_DIR/{{id}}-app.zip"
BIN="$APP_DIR/target/release/{{crate_name}}"

if [[ "${1:-}" != "--skip-build" ]]; then
  echo "==> building release binary"
  ( cd "$APP_DIR" && cargo build --release )
fi

[[ -f "$BIN" ]] || { echo "thiếu $BIN — chạy lại không có --skip-build"; exit 1; }

echo "==> staging release/"
rm -rf "$REL" "$ZIP"
mkdir -p "$REL"
cp "$BIN" "$REL/{{crate_name}}"
chmod +x "$REL/{{crate_name}}"
cp "$APP_DIR/senclaw-manifest.json" "$REL/senclaw-manifest.json"
cp -R "$APP_DIR/web" "$REL/web"

echo "==> zipping -> $ZIP"
( cd "$REL" && zip -rq "$ZIP" . -x '*.DS_Store' )

echo "xong:"
echo "  staged: $REL"
echo "  zip:    $ZIP ($(du -h "$ZIP" | cut -f1))"
