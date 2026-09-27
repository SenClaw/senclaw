#!/usr/bin/env bash
# Publish every zip under dist/<platform>/ to the SenClaw hub.
#
# One version of an app is published once per platform, in a fixed order. The
# hub answers 409 for the second artifact of a version it already holds, which
# publish-app.sh treats as a skip (ALLOW_VERSION_EXISTS), so a re-run of an
# unchanged version is a no-op. An over-cap zip (mlx-lm) is likewise a skip, not
# a failure (ALLOW_OVERSIZE). Each platform's outcome is printed in the job
# summary so a partial publish is visible here rather than from a user.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# The token is a repo secret the operator mints by hand (scope: publish,
# https://senclaw.bacnd.com/settings/tokens). Until it exists every green build
# would die right here — so its absence is a loud SKIP, not a failure: the
# builds and the release-attach job stay usable, and the summary says exactly
# what did not happen. A hub outage or a bad token still fails properly below.
if [[ -z "${SENCLAW_HUB_TOKEN:-}" ]]; then
  echo "::warning::SENCLAW_HUB_TOKEN secret is not set — hub publish SKIPPED. Mint a publish-scope token at https://senclaw.bacnd.com/settings/tokens and add it in Settings → Secrets."
  [[ -n "${GITHUB_STEP_SUMMARY:-}" ]] && {
    echo "## Hub publish skipped"
    echo "No \`SENCLAW_HUB_TOKEN\` secret — nothing was pushed to the hub."
  } >> "$GITHUB_STEP_SUMMARY"
  exit 0
fi
export SENCLAW_HUB_URL="${SENCLAW_HUB_URL:-https://senclaw.bacnd.com}"

# Platforms are published in this order; the first is the one guaranteed to land
# if the hub still accepts only a single artifact per version.
ORDER="${PUBLISH_PLATFORM_ORDER:-darwin-arm64 linux-x64 windows-x64}"
DRY_RUN="${PUBLISH_DRY_RUN:-0}"

# A zip on disk is traced back to its app via apps.json. Looked up per file
# rather than cached in an associative array, because macOS bash 3.2 has none.
app_of() { jq -r --arg z "$1" '.apps[] | select(.zip == $z) | .dir' apps.json; }

rows=(); failed=0

for platform in $ORDER; do
  [[ -d "dist/$platform" ]] || { echo "no dist/$platform — skipping"; continue; }
  for zip in dist/"$platform"/*.zip; do
    [[ -e "$zip" ]] || continue
    base=$(basename "$zip")
    app=$(app_of "$base")
    if [[ -z "$app" ]]; then
      echo "::warning::$base is not listed in apps.json — not publishing it"
      continue
    fi

    args=("$app" "$zip" --platform "$platform")
    [[ "$DRY_RUN" == "1" ]] && args+=(--dry-run)

    echo "::group::publish $app ($platform)"
    out=$(ALLOW_VERSION_EXISTS=1 ALLOW_OVERSIZE=1 bash scripts/publish-app.sh "${args[@]}" 2>&1) && rc=0 || rc=$?
    echo "$out"
    echo "::endgroup::"

    if [[ $rc -ne 0 ]]; then
      result="**failed**"
      failed=$((failed + 1))
      echo "::error title=publish failed::$app on $platform"
    elif grep -q "over the hub's 20 MB cap" <<<"$out"; then
      result="skipped (over 20 MB cap)"
    elif grep -q "SKIPPED" <<<"$out"; then
      result="skipped (version already on the hub)"
    elif grep -q "dry-run" <<<"$out"; then
      result="dry-run"
    else
      result="published"
    fi
    rows+=("| \`$app\` | $platform | $result |")
  done
done

{
  echo "### Hub publish — $SENCLAW_HUB_URL"
  echo
  echo "| app | platform | result |"
  echo "|---|---|---|"
  printf '%s\n' ${rows[@]+"${rows[@]}"}
  echo
  echo "_A \`skipped\` row means the hub already holds that \`name@version\`, or the "
  echo "zip is over the 20 MB cap. Published versions are immutable — bump "
  echo "\`version\` in the app's \`senclaw-hub.json\` (\`scripts/hub-init.py --bump patch\`) "
  echo "to ship a change._"
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"

exit $(( failed > 0 ))
