#!/usr/bin/env python3
"""Create or refresh `senclaw-hub.json` for every app that lacks one.

The runtime manifest (`senclaw-manifest.json`) tells the daemon how to run an
app; this file tells the hub how to list it. They are deliberately separate —
same facts in both is how the two drift — so this script only *derives* the
mechanically derivable parts (the narrow permission declaration) and leaves the
editorial fields for a human.

    scripts/hub-init.py               # fill in the gaps, touch nothing else
    scripts/hub-init.py --check       # exit 1 if any app is missing one (CI)
    scripts/hub-init.py --bump patch  # bump every app's version to ship a change

`permissions` is a security declaration shown to a user *before* they install,
so it is seeded at the narrowest thing that is certainly true — the app's own
binary and loopback — and flagged for review rather than guessed wider.
"""
import argparse
import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO_URL = "https://github.com/NortonBen/SenClaw"
INITIAL_VERSION = "1.0.0"


def bump(version: str, part: str) -> str:
    major, minor, patch = (int(x) for x in version.split("+")[0].split("-")[0].split("."))
    return {
        "major": f"{major + 1}.0.0",
        "minor": f"{major}.{minor + 1}.0",
        "patch": f"{major}.{minor}.{patch + 1}",
    }[part]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="report gaps, write nothing")
    ap.add_argument("--bump", choices=["major", "minor", "patch"])
    ap.add_argument("apps", nargs="*", help="app dirs (default: all Rust apps)")
    args = ap.parse_args()

    inventory = json.load(open(os.path.join(ROOT, "apps.json")))["apps"]
    rows = [a for a in inventory if a["lang"] == "rust"]
    if args.apps:
        rows = [a for a in rows if a["dir"] in args.apps]

    missing, created, bumped = [], [], []

    for app in rows:
        app_dir = os.path.join(ROOT, "apps", app["dir"])
        hub_path = os.path.join(app_dir, "senclaw-hub.json")
        manifest = json.load(open(os.path.join(app_dir, "senclaw-manifest.json")))

        if os.path.isfile(hub_path):
            if args.bump:
                doc = json.load(open(hub_path))
                doc["version"] = bump(doc["version"], args.bump)
                if not args.check:
                    with open(hub_path, "w") as fh:
                        json.dump(doc, fh, indent=2, ensure_ascii=False)
                        fh.write("\n")
                bumped.append(f"{app['id']} -> {doc['version']}")
            continue

        missing.append(app["id"])
        if args.check or args.bump:
            continue

        doc = {
            "version": INITIAL_VERSION,
            "category": "app",
            "keywords": [app["id"]],
            # Narrow on purpose: the app's own binary plus loopback, which is
            # where the daemon bridge lives. An app that reaches further must say
            # so here — an under-declared permission is a lie to the user, and an
            # over-declared one trains them to ignore the screen.
            "permissions": {
                "network": ["127.0.0.1"],
                "exec": [manifest.get("runtime", {}).get("start", f"./{app['bin']}")],
            },
            "updater": "none",
            "repo_url": REPO_URL,
        }
        with open(hub_path, "w") as fh:
            json.dump(doc, fh, indent=2, ensure_ascii=False)
            fh.write("\n")
        created.append(app["id"])

    if created:
        print(f"created senclaw-hub.json for {len(created)} apps: {', '.join(created)}")
    if bumped:
        print(f"bumped {len(bumped)}: {', '.join(bumped)}")
    if args.check and missing:
        print(f"missing senclaw-hub.json: {', '.join(missing)}", file=sys.stderr)
        return 1
    if not created and not bumped and not missing:
        print("every app already has a senclaw-hub.json")
    return 0


if __name__ == "__main__":
    sys.exit(main())
