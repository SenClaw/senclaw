#!/usr/bin/env bash
# Build {{title_name}} and assemble an installable Space-App zip.
#
# A Go app has **no install step**: the daemon runs `runtime.install` for the
# node and python runners only, so the binary has to exist before the app is
# ever launched. That is what this script is for.
#
#   release/                 <- staged, flat install layout
#     {{id}}                 (compiled binary; manifest runtime.start = ./{{id}})
#     senclaw-manifest.json
#     web/                   (static UI, served next to the binary)
#   {{id}}-app.zip           <- the artifact you install in SenClaw
#
# Usage: ./scripts/pack.sh [--skip-build]
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")/.." && pwd)"
REL="$APP_DIR/release"
ZIP="$APP_DIR/{{id}}-app.zip"
BIN="$APP_DIR/{{id}}"

if [[ "${1:-}" != "--skip-build" ]]; then
  echo "==> building binary"
  ( cd "$APP_DIR" && go build -o "{{id}}" . )
fi

[[ -f "$BIN" ]] || { echo "thiếu $BIN — chạy lại không có --skip-build"; exit 1; }

echo "==> staging release/"
rm -rf "$REL" "$ZIP"
mkdir -p "$REL"
cp "$BIN" "$REL/{{id}}"
chmod +x "$REL/{{id}}"
cp "$APP_DIR/senclaw-manifest.json" "$REL/senclaw-manifest.json"
cp -R "$APP_DIR/web" "$REL/web"

echo "==> zipping -> $ZIP"
( cd "$REL" && zip -rq "$ZIP" . -x '*.DS_Store' )

echo "xong:"
echo "  staged: $REL"
echo "  zip:    $ZIP ($(du -h "$ZIP" | cut -f1))"
