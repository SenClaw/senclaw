#!/usr/bin/env bash
# Re-sign the built SenClaw Desktop.app after the daemon/metallib were bundled
# into Contents/Resources (which breaks the seal flutter build created).
#
# Uses the stable "SenClaw Dev" identity when present so the app's code-signing
# identity — and therefore macOS TCC grants like Screen Recording — survive
# every rebuild/reinstall. Falls back to ad-hoc (old behaviour) with a warning.
#
# Usage: scripts/macos_sign_app.sh "<path to SenClaw Desktop.app>"
set -euo pipefail

APP="${1:?usage: macos_sign_app.sh <path-to-.app>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENTITLEMENTS="$ROOT/desktop_app/macos/Runner/Release.entitlements"
CN="SenClaw Dev"

[[ -d "$APP" ]] || { echo "[sign] no app bundle at $APP"; exit 1; }

IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
  | awk -F'"' -v cn="$CN" '$2 == cn {print $2; exit}')"

if [[ -z "$IDENTITY" ]]; then
  echo "[sign] WARNING: no '$CN' identity found — signing ad-hoc."
  echo "[sign]   TCC grants (Screen Recording…) will break on every reinstall."
  echo "[sign]   Run scripts/macos_make_signing_cert.sh once to fix this for good."
  IDENTITY="-"
else
  echo "[sign] signing with stable identity '$IDENTITY' (TCC grants persist)"
fi

# Sign the bundled daemon first: it lives in Resources, so its hash is part of
# the app's resource seal — it must be final before the outer signature.
if [[ -f "$APP/Contents/Resources/senclaw" ]]; then
  codesign --force -s "$IDENTITY" "$APP/Contents/Resources/senclaw"
fi

# Deep-sign nested frameworks/plugins, then the app itself with the Release
# entitlements (file_picker >=10 needs files.user-selected in the signature).
codesign --force --deep -s "$IDENTITY" "$APP"
codesign --force -s "$IDENTITY" --entitlements "$ENTITLEMENTS" "$APP"

codesign --verify --deep "$APP" && echo "[sign] signature verified"
