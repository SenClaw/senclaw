#!/usr/bin/env bash
# Build one Space App and assemble its installable zip.
#
# Every app is packed the same way, from the row that describes it in apps.json.
# Only two engine apps live in this repo (candle, mlx-lm); the other ~45 are in
# the senclaw-app repo, whose scripts/pack-app.sh this is adapted from. Two
# differences from that copy:
#
#   * web/ is a static dir (index.html) served by the app's own ServeDir from
#     exe_dir/web, so it is staged as-is — there is no npm build to run.
#   * an app with "metallib": true (mlx-lm) gets mlx.metallib staged beside its
#     binary. MLX resolves its Metal shader library relative to the executable,
#     so the engine cannot start without it. The library is a build artifact,
#     found under target/, not a source file — this mirrors `make app-build`.
#
#   release/<id>/            <- staged, flat install layout
#     <bin>[.exe]            (release binary; manifest runtime.start points at it)
#     senclaw-manifest.json
#     senclaw-hub.json
#     web/…                  (whatever apps.json lists under `stage`)
#     mlx.metallib           (metallib apps only)
#   dist/<platform>/<zip>    <- the artifact you install in SenClaw / publish
#
# Usage: scripts/pack-app.sh <app-dir> [--skip-build]
#
# Windows note: the daemon launches a server app with `cmd /C <runtime.start>`,
# and cmd cannot run `./name` — so on Windows the binary is staged as
# `<bin>.exe` and the manifest inside the zip has `runtime.start` rewritten to
# match. The manifest in the source tree is never touched.
set -euo pipefail

APP="${1:-}"
[[ -n "$APP" ]] || { echo "usage: scripts/pack-app.sh <app-dir> [--skip-build]" >&2; exit 2; }
SKIP_BUILD="${2:-}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$ROOT/apps/$APP"
[[ -d "$APP_DIR" ]] || { echo "no such app: apps/$APP" >&2; exit 1; }

# ── platform ────────────────────────────────────────────────────────────────
# Names match `senclaw`'s own host_platform(): darwin|linux|windows + arm64|x64.
# The hub stores one artifact per (version, platform), so this string is what a
# user's daemon matches against when it downloads.
case "$(uname -s)" in
  Darwin)                          OS=darwin  ; EXE=""     ;;
  Linux)                           OS=linux   ; EXE=""     ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT) OS=windows ; EXE=".exe" ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) ARCH=arm64 ;;
  x86_64|amd64)  ARCH=x64   ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
PLATFORM="$OS-$ARCH"

# ── app metadata ────────────────────────────────────────────────────────────
# `tr -d '\r'` because jq.exe writes CRLF on Windows, and `read -r` keeps the CR.
meta()  { jq -er --arg d "$APP" "$1" "$ROOT/apps.json" | tr -d '\r'; }
metar() { jq -r  --arg d "$APP" "$1" "$ROOT/apps.json" | tr -d '\r'; }  # no -e: booleans
row='(.apps[] | select(.dir == $d))'

meta "$row | .dir" >/dev/null || { echo "apps/$APP is not listed in apps.json" >&2; exit 1; }
LANG_=$(meta "$row | .lang")
[[ "$LANG_" == "rust" ]] || { echo "apps/$APP is a $LANG_ app — not packed by this script" >&2; exit 1; }

ID=$(meta "$row | .id")
CRATE=$(meta "$row | .crate")
BIN=$(meta "$row | .bin")
ZIPNAME=$(meta "$row | .zip")

STAGE="$ROOT/release/$ID"
OUTDIR="$ROOT/dist/$PLATFORM"
ZIP="$OUTDIR/$ZIPNAME"

echo "==> packing $ID ($CRATE) for $PLATFORM"

# ── build ───────────────────────────────────────────────────────────────────
if [[ "$SKIP_BUILD" != "--skip-build" ]]; then
  while read -r d; do
    [[ -n "$d" ]] || continue
    echo "==> npm build: $d"
    ( cd "$APP_DIR/$d" && npm install --silent && npm run build )
  done < <(meta "$row | (.npm // [])[]" || true)

  echo "==> cargo build -p $CRATE --release"
  ( cd "$ROOT" && cargo build -p "$CRATE" --release )
fi

BINPATH="$ROOT/target/release/$CRATE$EXE"
[[ -f "$BINPATH" ]] || { echo "missing $BINPATH — run without --skip-build" >&2; exit 1; }

# ── stage ───────────────────────────────────────────────────────────────────
rm -rf "$STAGE" "$ZIP"
mkdir -p "$STAGE" "$OUTDIR"

cp "$BINPATH" "$STAGE/$BIN$EXE"
chmod +x "$STAGE/$BIN$EXE"

# The manifest is the one file that differs per platform: cmd.exe cannot run
# `./name`, so Windows zips carry `runtime.start = "name.exe"`.
if [[ -n "$EXE" ]]; then
  jq --arg s "$BIN$EXE" '.runtime.start = $s' \
    "$APP_DIR/senclaw-manifest.json" > "$STAGE/senclaw-manifest.json"
  echo "    runtime.start -> $BIN$EXE (Windows)"
else
  cp "$APP_DIR/senclaw-manifest.json" "$STAGE/senclaw-manifest.json"
fi

while read -r pair; do
  [[ -n "$pair" ]] || continue
  src="${pair%%$'\t'*}"; dest="${pair#*$'\t'}"
  if [[ ! -e "$APP_DIR/$src" ]]; then
    echo "    skip $src (absent)"
    continue
  fi
  cp -R "$APP_DIR/$src" "$STAGE/$dest"
done < <(meta "$row | (.stage // [])[] | @tsv" || true)

while read -r f; do
  [[ -n "$f" ]] || continue
  [[ -f "$APP_DIR/$f" ]] && cp "$APP_DIR/$f" "$STAGE/$f" || echo "    skip $f (absent)"
done < <(meta "$row | (.files // [])[]" || true)

# ── mlx.metallib (MLX apps only) ─────────────────────────────────────────────
# Found under target/ rather than staged from the app dir, because it is emitted
# by the mlx-sys build script, not committed. Prefer the copy the app's build.rs
# drops next to the binary; fall back to the raw one under build/ (same search
# `make app-build` uses for the daemon's sidecar).
if [[ "$(metar "$row | (.metallib // false)")" == "true" ]]; then
  METALLIB="$ROOT/target/release/mlx.metallib"
  [[ -f "$METALLIB" ]] || METALLIB=$(find "$ROOT/target/release/build" -name mlx.metallib 2>/dev/null | head -1)
  [[ -n "$METALLIB" && -f "$METALLIB" ]] || {
    echo "missing mlx.metallib under target/release — build $CRATE before packing" >&2
    exit 1
  }
  cp "$METALLIB" "$STAGE/mlx.metallib"
  echo "    staged mlx.metallib ($(( $(wc -c < "$METALLIB") / 1048576 )) MB)"
fi

# senclaw-hub.json rides along so `senclaw hub publish` and the CI publisher
# read the same version whether they start from the zip or the source tree.
[[ -f "$APP_DIR/senclaw-hub.json" && ! -f "$STAGE/senclaw-hub.json" ]] \
  && cp "$APP_DIR/senclaw-hub.json" "$STAGE/senclaw-hub.json"

find "$STAGE" -name '.DS_Store' -delete 2>/dev/null || true

# ── zip ─────────────────────────────────────────────────────────────────────
# `zip` is absent from GitHub's Windows runners; 7-Zip is present on all three.
if command -v zip >/dev/null 2>&1; then
  ( cd "$STAGE" && zip -rqX "$ZIP" . )
elif command -v 7z >/dev/null 2>&1; then
  ( cd "$STAGE" && 7z a -tzip -bso0 -bsp0 "$(cygpath -w "$ZIP" 2>/dev/null || echo "$ZIP")" . >/dev/null )
else
  echo "need either 'zip' or '7z' on PATH to build the artifact" >&2
  exit 1
fi

# `senclaw hub publish <app> --pack` looks for the artifact beside the app by
# default, so leave a copy there for hand-publishing from a dev machine. CI
# publishes out of dist/<platform>/ and never reads this one.
cp "$ZIP" "$APP_DIR/$ZIPNAME"

SIZE=$(wc -c < "$ZIP" | tr -d ' ')
echo "done: $ZIP ($(( SIZE / 1024 )) KB)"

# The hub holds the whole upload in memory to hash it, so 20 MB is a hard server
# limit. mlx-lm ships an ~88 MB mlx.metallib and will always exceed it — its zip
# is still built (and attached to the GitHub release), but the publisher skips
# the hub upload rather than failing. See scripts/publish-app.sh.
if (( SIZE > 20 * 1024 * 1024 )); then
  echo "note: $ZIPNAME is $(( SIZE / 1048576 )) MB — over the hub's 20 MB cap; it will not be published to the store" >&2
fi
