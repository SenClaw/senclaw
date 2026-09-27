#!/usr/bin/env bash
# Decide which apps get built, on which platforms.
#
# Emits three workflow outputs:
#   any      false when nothing needs building, so the build job can skip
#   matrix   the build job's `include` list — one entry per (app, platform)
#   publish  whether the publish job should run
#
# Unlike the senclaw-app fleet (49 apps batched to share a cargo cache), only
# two apps live here and they share almost no expensive compilation — candle
# builds candle-rs, mlx-lm builds the MLX C++ runtime — so each (app, platform)
# is its own job. Each app declares its own `platforms` in apps.json: candle is
# pure Rust and ships everywhere, mlx-lm is Apple-Silicon only.
set -euo pipefail

: "${GITHUB_OUTPUT:?must run inside GitHub Actions}"

runner_for() {
  case "$1" in
    linux-x64)    echo ubuntu-22.04 ;;
    # macos-latest, NOT macos-14 (the senclaw-app fleet's choice): mlx-lm
    # compiles the MLX C++ runtime, whose cmake dies on the macos-14 image's
    # older Xcode — deterministically, ~6 min in, cargo exit 101 — while the
    # same tree builds clean on macos-latest (desktop.yml builds mlx-sys there
    # for the senclaw-media sidecar). The other repo's 49 apps build no MLX,
    # which is why macos-14 never hurt it.
    darwin-arm64) echo macos-latest ;;
    windows-x64)  echo windows-2022 ;;
    *) echo "::error::unknown platform '$1'" >&2; exit 1 ;;
  esac
}

# Platforms requested by a workflow_dispatch (default: whatever each app declares).
case "${INPUT_PLATFORMS:-all}" in
  ""|all) requested=(linux-x64 darwin-arm64 windows-x64) ;;
  *)      requested=("${INPUT_PLATFORMS}") ;;
esac
want_platform() { local p; for p in "${requested[@]}"; do [[ "$p" == "$1" ]] && return 0; done; return 1; }

# All buildable Rust apps (honour apps.json's `build: false` opt-out).
all=(); while read -r d; do all+=("$d"); done \
  < <(jq -r '.apps[] | select(.lang == "rust" and .build != false) | .dir' apps.json | sort)

changed() { ! git diff --quiet "$BEFORE" "$GITHUB_SHA" -- "apps/$1/"; }

selected=(); reason=""

if [[ -n "${INPUT_APPS:-}" ]]; then
  # An explicit dispatch list wins over everything, including "it did not change".
  for a in $INPUT_APPS; do
    if printf '%s\n' "${all[@]}" | grep -qx -- "$a"; then
      selected+=("$a")
    else
      echo "::error::'$a' is not a buildable app in apps.json"; exit 1
    fi
  done
  reason="explicit list from workflow_dispatch"

elif [[ "$GITHUB_REF" == refs/tags/* ]]; then
  # A release tag must be self-contained: build every app so all installers
  # attach to the release, regardless of what changed.
  selected=("${all[@]}")
  reason="tag push — building the whole fleet"

elif [[ -z "${BEFORE:-}" ]] \
  || ! git rev-parse -q --verify "${BEFORE}^{commit}" >/dev/null 2>&1 \
  || ! git diff --quiet "$BEFORE" "$GITHUB_SHA" -- \
        apps.json Cargo.toml Cargo.lock \
        scripts/ci-plan.sh scripts/pack-app.sh scripts/publish-app.sh \
        scripts/ci-publish.sh scripts/check-apps.py scripts/hub-init.py \
        .github/workflows/space-apps.yml app-space-sdk/ apps/local-model-core/; then
  # No usable diff base (first push, force push, PR), or a shared file moved.
  # app-space-sdk and local-model-core are compiled into BOTH engines, so a
  # change to either — like a lockfile or packer edit — can break any app.
  selected=("${all[@]}")
  reason="no diff base, or a shared file changed"

else
  for a in "${all[@]}"; do changed "$a" && selected+=("$a"); done
  reason="changed since $BEFORE"
fi

echo "Plan: ${#selected[@]} app(s) — $reason"
for a in ${selected[@]+"${selected[@]}"}; do echo "  · $a"; done

# ── (app × its platforms) ────────────────────────────────────────────────────
matrix='[]'
for a in ${selected[@]+"${selected[@]}"}; do
  arow=$(jq -c --arg d "$a" '.apps[] | select(.dir == $d)' apps.json)
  id=$(jq -r '.id'  <<<"$arow")
  zip=$(jq -r '.zip' <<<"$arow")
  platforms=$(jq -r '(.platforms // ["linux-x64","darwin-arm64","windows-x64"])[]' <<<"$arow")
  for p in $platforms; do
    want_platform "$p" || continue
    matrix=$(jq -c \
      --arg app "$a" --arg id "$id" --arg zip "$zip" \
      --arg platform "$p" --arg runner "$(runner_for "$p")" \
      '. + [{app: $app, id: $id, zip: $zip, platform: $platform, runner: $runner}]' \
      <<<"$matrix")
  done
done

count=$(jq 'length' <<<"$matrix")

# A push to main publishes, not only a tag or an explicit dispatch. Safe on
# every push because `name@version` comes from each app's senclaw-hub.json and
# the hub answers 409 for a version it already holds, which publish-app.sh
# treats as a skip — so a push that bumped nothing publishes nothing. A
# pull_request run has GITHUB_REF=refs/pull/N/merge and never matches.
publish=false
if [[ "${INPUT_PUBLISH:-false}" == "true" \
   || "$GITHUB_REF" == refs/tags/* \
   || "$GITHUB_REF" == refs/heads/main ]]; then
  publish=true
fi

{
  if [[ "$count" -eq 0 ]]; then echo "any=false"; else echo "any=true"; fi
  echo "matrix=$matrix"
  echo "publish=$publish"
} >> "$GITHUB_OUTPUT"

echo "Jobs: $count · publish=$publish"
