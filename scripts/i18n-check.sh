#!/usr/bin/env bash
# Report the web UI's translation coverage.
#
# Every `t('…')` / `t("…")` literal in web/src is looked up in the merged
# Vietnamese dictionary (the generated vi.json plus the web-owned
# vi.web.json). A miss is **not** a failure: the English string is the key, so
# an untranslated sentence renders as itself rather than as a broken key. What
# would be a real failure is the reverse — a key in vi.web.json that no longer
# appears anywhere, which means a sentence was reworded and its translation
# left behind.
#
#   scripts/i18n-check.sh            # report
#   scripts/i18n-check.sh --strict   # also fail when coverage is incomplete
set -euo pipefail

cd "$(dirname "$0")/.."
STRICT="${1:-}"

python3 - "$STRICT" <<'PY'
import json, pathlib, re, sys

strict = sys.argv[1] == "--strict"
root = pathlib.Path("web/src")
shared = json.loads(pathlib.Path("web/src/i18n/vi.json").read_text())
web = json.loads(pathlib.Path("web/src/i18n/vi.web.json").read_text())
merged = {**shared, **web}

# `t('…')` and `tArgs('…', …)`, single or double quoted, no escapes inside.
call = re.compile(r"\bt(?:Args)?\(\s*(['\"])((?:(?!\1).)+)\1")
# A string that reaches `t()` through a variable cannot be seen as a literal.
# `// i18n-dynamic: A, B, C` declares those, so their translations are not
# reported as orphans and then deleted by someone tidying up.
dynamic = re.compile(r"//\s*i18n-dynamic:\s*(.+)")

used, files = set(), 0
for path in root.rglob("*.tsx"):
    if "i18n" in path.parts:
        continue
    files += 1
    body = path.read_text()
    for _, text in call.findall(body):
        used.add(text)
    for line in dynamic.findall(body):
        for key in line.split(","):
            if key.strip():
                used.add(key.strip())

missing = sorted(s for s in used if s not in merged)
orphans = sorted(k for k in web if k not in used)

print(f"scanned {files} files")
print(f"translated  {len(used) - len(missing)}/{len(used)} strings in use")
print(f"dictionary  {len(shared)} shared + {len(web)} web-only")

if missing:
    print(f"\nno Vietnamese yet ({len(missing)}) — these render in English:")
    for s in missing[:30]:
        print(f"  {s[:100]}")
    if len(missing) > 30:
        print(f"  … and {len(missing) - 30} more")

if orphans:
    print(f"\nweb-only keys nothing uses ({len(orphans)}) — reworded or deleted:")
    for s in orphans:
        print(f"  {s[:100]}")

# An orphan is always a real problem: a translation for a sentence that is no
# longer said. Missing coverage is only a problem when asked to be strict.
bad = bool(orphans) or (strict and bool(missing))
sys.exit(1 if bad else 0)
PY
