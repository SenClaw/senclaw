#!/usr/bin/env python3
"""Fleet invariants for the apps that ship from this repo.

Runs first in CI so a typo — a duplicate port, a manifest whose id drifted from
apps.json, a non-semver version, an unknown platform — costs seconds rather than
an hour of build time, and never reaches the hub. This is the two-app cousin of
senclaw-app's scripts/check-apps.py; it checks the same silent-failure mistakes
without the 49-app fleet machinery.

    scripts/check-apps.py          # exit 1 on any problem
"""
import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
KNOWN_PLATFORMS = {"linux-x64", "darwin-arm64", "windows-x64"}
SEMVER = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$"
)
REQUIRED = ("id", "dir", "port", "lang", "crate", "bin", "zip")


def main() -> int:
    errors = []

    with open(os.path.join(ROOT, "apps.json")) as fh:
        apps = json.load(fh)["apps"]

    seen = {"id": {}, "port": {}, "zip": {}, "dir": {}}
    for app in apps:
        who = app.get("id") or app.get("dir") or "<unnamed>"

        for field in REQUIRED:
            if not app.get(field) and app.get(field) != 0:
                errors.append(f"{who}: missing apps.json field '{field}'")

        # Uniqueness across the fleet: two apps on one port both bind it, and the
        # loser silently never starts.
        for key in ("id", "port", "zip", "dir"):
            val = app.get(key)
            if val is None:
                continue
            if val in seen[key]:
                errors.append(f"duplicate {key} '{val}': {seen[key][val]} and {who}")
            seen[key][val] = who

        for p in app.get("platforms", []):
            if p not in KNOWN_PLATFORMS:
                errors.append(f"{who}: unknown platform '{p}' (want {sorted(KNOWN_PLATFORMS)})")

        if "metallib" in app and not isinstance(app["metallib"], bool):
            errors.append(f"{who}: 'metallib' must be true/false, got {app['metallib']!r}")

        if app.get("lang") != "rust":
            continue  # the rest is Rust-app packaging metadata

        app_dir = os.path.join(ROOT, "apps", app.get("dir", ""))
        if not os.path.isdir(app_dir):
            errors.append(f"{who}: apps/{app.get('dir')} does not exist")
            continue

        # Manifest ⇄ apps.json: the daemon runs the manifest, the hub lists it,
        # the packer reads apps.json — a drifted id ships an app under the wrong
        # name, an empty description is a hard hub reject after the upload.
        man_path = os.path.join(app_dir, "senclaw-manifest.json")
        if not os.path.isfile(man_path):
            errors.append(f"{who}: missing senclaw-manifest.json")
        else:
            manifest = json.load(open(man_path))
            if manifest.get("id") != app.get("id"):
                errors.append(
                    f"{who}: manifest id '{manifest.get('id')}' != apps.json id '{app.get('id')}'"
                )
            if not (manifest.get("description") or "").strip():
                errors.append(f"{who}: senclaw-manifest.json has no description (the hub rejects that)")

        hub_path = os.path.join(app_dir, "senclaw-hub.json")
        if not os.path.isfile(hub_path):
            errors.append(f"{who}: missing senclaw-hub.json (run scripts/hub-init.py)")
        else:
            version = json.load(open(hub_path)).get("version", "")
            if not SEMVER.match(str(version)):
                errors.append(f"{who}: version '{version}' is not semver (X.Y.Z)")

    if errors:
        print(f"check-apps: {len(errors)} problem(s):", file=sys.stderr)
        for e in errors:
            print(f"  ✗ {e}", file=sys.stderr)
        return 1

    print(f"check-apps: {len(apps)} app(s) OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
