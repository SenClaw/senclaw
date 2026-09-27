#!/usr/bin/env bash
# Publish one packed Space App to the SenClaw hub.
#
# Speaks the hub's publish contract directly rather than shelling out to the
# `senclaw` binary: CI would otherwise have to build the whole daemon just to
# send a multipart POST.
#
#   POST <hub>/api/v1/publish        multipart/form-data
#   Authorization: Bearer snc_pat_…
#   fields: kind name version description file [keywords category permissions
#           repoUrl homepageUrl readme platform]
#
# Server-side invariants that shape this script:
#   * `name@version` is immutable — a published version can be yanked but never
#     replaced, so every shipped change needs a version bump.
#   * The scope is forced to the token owner's handle; we never send one.
#   * Uploads are capped at 20 MB (the hub hashes the whole body in memory).
#
# Adapted from the senclaw-app copy in ONE way: an artifact over the size cap is
# a SKIP, not a failure. mlx-lm ships an ~88 MB mlx.metallib and will always
# exceed the cap; its zip is still built and attached to the GitHub release, but
# it cannot live on the hub, and that must not turn the publish job red.
#
# Usage:
#   scripts/publish-app.sh <app-dir> <artifact.zip> [--platform <p>] [--dry-run]
#
# Environment:
#   SENCLAW_HUB_TOKEN     publish token from https://senclaw.bacnd.com/settings/tokens
#   SENCLAW_HUB_URL       hub base URL (default https://senclaw.bacnd.com)
#   ALLOW_VERSION_EXISTS  "1" to treat HTTP 409 as a skip rather than a failure
#   ALLOW_OVERSIZE        "1" (default) to treat an over-cap zip as a skip
set -euo pipefail

HUB="${SENCLAW_HUB_URL:-https://senclaw.bacnd.com}"
HUB="${HUB%/}"
MAX_BYTES=$((20 * 1024 * 1024))

APP=""; ARTIFACT=""; PLATFORM=""; DRY_RUN=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --platform) PLATFORM="$2"; shift 2 ;;
    --dry-run)  DRY_RUN=1; shift ;;
    -*) echo "unknown flag: $1" >&2; exit 2 ;;
    *)  if [[ -z "$APP" ]]; then APP="$1"; else ARTIFACT="$1"; fi; shift ;;
  esac
done
[[ -n "$APP" && -n "$ARTIFACT" ]] || {
  echo "usage: scripts/publish-app.sh <app-dir> <artifact.zip> [--platform <p>] [--dry-run]" >&2
  exit 2
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$ROOT/apps/$APP"
[[ -f "$ARTIFACT" ]] || { echo "no artifact at $ARTIFACT" >&2; exit 1; }

MANIFEST="$APP_DIR/senclaw-manifest.json"
HUBJSON="$APP_DIR/senclaw-hub.json"
[[ -f "$MANIFEST" ]] || { echo "missing $MANIFEST" >&2; exit 1; }
[[ -f "$HUBJSON" ]] || {
  echo "missing $HUBJSON — every published app needs one (see scripts/hub-init.py)" >&2
  exit 1
}

NAME=$(jq -er '.id' "$MANIFEST")
DESCRIPTION=$(jq -er '.description' "$MANIFEST") || {
  echo "$NAME: senclaw-manifest.json has no description — the hub rejects that" >&2
  exit 1
}
VERSION=$(jq -er '.version' "$HUBJSON")

# The hub is strict about semver; `1.0` and `v1.0.0` are the two mistakes people
# actually make, and both come back as an opaque 400 after the upload.
[[ "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] || {
  echo "$NAME: version '$VERSION' is not semver (X.Y.Z)" >&2
  exit 1
}

SIZE=$(wc -c < "$ARTIFACT" | tr -d ' ')
if (( SIZE > MAX_BYTES )); then
  echo "   SKIPPED: $NAME $(basename "$ARTIFACT") is $(( SIZE / 1048576 )) MB — over the hub's 20 MB cap, not published to the store"
  [[ "${ALLOW_OVERSIZE:-1}" == "1" ]] && exit 0
  exit 1
fi

[[ -n "$PLATFORM" ]] || PLATFORM=$(basename "$(dirname "$ARTIFACT")")

echo "── $NAME@$VERSION · $PLATFORM · $(( SIZE / 1024 )) KB · $HUB"

if (( DRY_RUN )); then
  echo "   dry-run — nothing uploaded"
  exit 0
fi

TOKEN="${SENCLAW_HUB_TOKEN:-}"
[[ -n "$TOKEN" ]] || {
  echo "$NAME: SENCLAW_HUB_TOKEN is unset — create a token with the 'publish' scope at $HUB/settings/tokens" >&2
  exit 1
}

args=(
  -sS -X POST "$HUB/api/v1/publish"
  -H "Authorization: Bearer $TOKEN"
  -F "kind=app"
  -F "name=$NAME"
  -F "version=$VERSION"
  -F "description=$DESCRIPTION"
  -F "platform=$PLATFORM"
  -F "file=@$ARTIFACT;type=application/zip"
)

add_opt() { # add_opt <form-field> <jq-path>  — skipped when absent in hub json
  local field="$1" path="$2" value
  value=$(jq -er "$path" "$HUBJSON" 2>/dev/null) || return 0
  [[ -n "$value" && "$value" != "null" ]] && args+=(-F "$field=$value")
  return 0
}
add_opt category    '.category'
add_opt keywords    '(.keywords // []) | join(",")'
add_opt permissions '.permissions | tojson'
add_opt repoUrl     '.repo_url'
add_opt homepageUrl '.homepage_url'

# The README becomes the package's page on the hub.
if [[ -f "$APP_DIR/README.md" ]]; then
  args+=(-F "readme=<$APP_DIR/README.md")
fi

BODY=$(mktemp)
trap 'rm -f "$BODY"' EXIT
STATUS=$(curl "${args[@]}" -o "$BODY" -w '%{http_code}' --max-time 300) || {
  echo "   FAILED: could not reach $HUB" >&2
  exit 1
}

if [[ "$STATUS" == 2* ]]; then
  echo "   published: $(jq -r '.url // .slug // "ok"' "$BODY" 2>/dev/null || cat "$BODY")"
  exit 0
fi

CODE=$(jq -r '.error // "unknown"' "$BODY" 2>/dev/null || echo unknown)
MSG=$(jq -r '.message // "(no message)"' "$BODY" 2>/dev/null || cat "$BODY")

# 409 means this exact name@version is already on the hub. Re-running a workflow
# on an unchanged version hits it constantly, and so does the second platform of
# a multi-platform release, so it is a skip by default.
if [[ "$STATUS" == "409" ]]; then
  echo "   SKIPPED: $NAME@$VERSION already exists on the hub — bump the version in senclaw-hub.json to ship a change"
  [[ "${ALLOW_VERSION_EXISTS:-1}" == "1" ]] && exit 0
  exit 1
fi

case "$STATUS:$CODE" in
  401:*)                  echo "   FAILED (401): token invalid or expired — regenerate it at $HUB/settings/tokens" >&2 ;;
  403:insufficient_scope) echo "   FAILED (403): token lacks the 'publish' scope" >&2 ;;
  403:no_handle)          echo "   FAILED (403): the token's account has no username yet — set one on the hub first" >&2 ;;
  403:*)                  echo "   FAILED (403): not a maintainer of '$NAME' in this scope" >&2 ;;
  413:*)                  echo "   FAILED (413): artifact above the hub's size limit" >&2 ;;
  *)                      echo "   FAILED (HTTP $STATUS · $CODE): $MSG" >&2 ;;
esac
exit 1
