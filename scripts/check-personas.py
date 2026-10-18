#!/usr/bin/env python3
"""Persona invariants for the Space App repos.

The daemon's PersonaRegistry (src/agent/persona_registry.rs `parse_file`) silently
skips a persona file that has no `---` frontmatter block or an empty `description`.
The app still installs, `list_agents` still lists the persona, and a dispatch to it
then stalls for 60 s and reports "Never started: its agent or persona could not be
resolved". Nothing at build or install time says why, so this check says it first.

Apps live in their own repos (`app-<dir>`, checked out next to this one), so a
cargo test here would only see them when the siblings happen to exist. This is a
stdlib-only script instead: run it from any checkout, point it at one app repo
from inside that repo's CI, or let it scan the siblings.

    scripts/check-personas.py                 # every ../app-* next to this repo
    scripts/check-personas.py ../app-ba       # one or more app repo directories
    scripts/check-personas.py --apps-dir DIR  # every DIR/app-*

Exit 1 on any FAIL. What it enforces, per app repo:

  * every personas/*.md starts with a `---` ... `---` block (first line, as the
    installer's own `front_matter_name` reads it) holding a non-empty `name` and
    `description`;
  * (warning only) neither value is a YAML block-scalar indicator (`>-`, `|`, ...):
    the registry splits on the first colon of each line and registers the literal
    `>-`, so the persona loads but with a useless description;
  * every persona the manifest declares has its file, and its manifest `name` equals
    the frontmatter `name` (the registry keys on the frontmatter name; the installer
    only uses the manifest name for the `<app>__<name>.md` file);
  * no frontmatter `name` is declared by two apps: on a clash the registry keeps one
    under its plain name and renames the other to its file stem, in directory order.

The parse below mirrors `parse_file`: `key: value` split on the first colon, both
trimmed, blank and `#` lines skipped.
"""
import argparse
import glob
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
BLOCK_SCALAR_STARTS = (">", "|")


def parse_frontmatter(text):
    """(fields, error) for a persona file, read the way the registry reads it."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return None, "no `---` frontmatter block starting on the first line (the registry skips a persona without one)"
    end = next((i for i in range(1, len(lines)) if lines[i].strip() == "---"), None)
    if end is None:
        return None, "frontmatter is opened with `---` but never closed"
    fields = {}
    for line in lines[1:end]:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, sep, value = line.partition(":")
        if sep:
            fields[key.strip()] = value.strip()
    return fields, None


def check_persona_file(path):
    """(name, problems, warnings) for one personas/*.md file."""
    try:
        with open(path, encoding="utf-8") as fh:
            fields, error = parse_frontmatter(fh.read())
    except (OSError, UnicodeDecodeError) as exc:
        return None, [f"unreadable: {exc}"], []
    if error:
        return None, [error], []
    problems, warnings = [], []
    for key in ("name", "description"):
        value = fields.get(key, "")
        if not value:
            problems.append(f"frontmatter has no non-empty `{key}`")
        elif value.startswith(BLOCK_SCALAR_STARTS):
            warnings.append(
                f"`{key}: {value}` is a YAML block scalar; the registry reads that literally, "
                f"so write the {key} on one line"
            )
    return fields.get("name") or None, problems, warnings


def check_repo(repo):
    """(errors, warnings, {frontmatter name: file}) for one app repo directory."""
    errors, warnings, names, checked = [], [], {}, {}
    app = os.path.basename(os.path.normpath(repo))

    def fail(rel, message):
        errors.append(f"{app}: {rel}: {message}")

    def check_file(rel):
        """Check one persona file once; its frontmatter name, or None if it has none."""
        rel = os.path.normpath(rel)
        if rel not in checked:
            name, problems, notes = check_persona_file(os.path.join(repo, rel))
            for problem in problems:
                fail(rel, problem)
            warnings.extend(f"{app}: {rel}: {note}" for note in notes)
            if name:
                names[name] = rel
            checked[rel] = name
        return checked[rel]

    on_disk = sorted(glob.glob(os.path.join(repo, "personas", "*.md")))
    for path in on_disk:
        check_file(os.path.relpath(path, repo))

    manifest_path = os.path.join(repo, "senclaw-manifest.json")
    if not os.path.isfile(manifest_path):
        if on_disk:
            fail("senclaw-manifest.json", "missing, so the personas above are never installed")
        return errors, warnings, names
    try:
        with open(manifest_path, encoding="utf-8") as fh:
            manifest = json.load(fh)
    except (OSError, ValueError) as exc:
        fail("senclaw-manifest.json", f"unreadable: {exc}")
        return errors, warnings, names

    declared = set()
    for entry in manifest.get("personas") or []:
        pname = entry.get("name")
        if not pname:
            fail("senclaw-manifest.json", f"persona entry without a name: {entry!r}")
            continue
        rel = os.path.normpath(entry.get("path") or f"personas/{pname}.md")
        declared.add(rel)
        if not os.path.isfile(os.path.join(repo, rel)):
            fail("senclaw-manifest.json", f"persona '{pname}' declares {rel}, which does not exist")
            continue
        name = check_file(rel)
        if name and name != pname:
            fail(rel, f"frontmatter name '{name}' != manifest persona name '{pname}'")

    for path in on_disk:
        rel = os.path.relpath(path, repo)
        if rel not in declared:
            fail(rel, "not declared in senclaw-manifest.json `personas`, so it is never installed")
    return errors, warnings, names


def find_repos(args):
    if args.repos:
        return args.repos
    apps_dir = args.apps_dir or os.path.dirname(os.path.dirname(HERE))
    return sorted(
        d for d in glob.glob(os.path.join(apps_dir, "app-*"))
        if os.path.isfile(os.path.join(d, "senclaw-manifest.json"))
    )


def main():
    parser = argparse.ArgumentParser(description="Check Space App persona files.")
    parser.add_argument("repos", nargs="*", help="app repo directories (default: ../app-*)")
    parser.add_argument("--apps-dir", help="scan DIR/app-* instead of this repo's siblings")
    args = parser.parse_args()

    repos = find_repos(args)
    if not repos:
        print("check-personas: no app repos found", file=sys.stderr)
        return 1

    errors, warnings, owners, files = [], [], {}, 0
    for repo in repos:
        repo_errors, repo_warnings, names = check_repo(repo)
        errors.extend(repo_errors)
        warnings.extend(repo_warnings)
        files += len(glob.glob(os.path.join(repo, "personas", "*.md")))
        for name in names:
            owners.setdefault(name, []).append(os.path.basename(os.path.normpath(repo)))
    for name, apps in sorted(owners.items()):
        if len(apps) > 1:
            errors.append(f"persona name '{name}' is declared by {', '.join(apps)}; names must be unique across apps")

    for warning in warnings:
        print(f"WARN {warning}")
    for error in errors:
        print(f"FAIL {error}")
    print(
        f"check-personas: {len(repos)} app repo(s), {files} persona file(s), "
        f"{len(errors)} failure(s), {len(warnings)} warning(s)"
    )
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
