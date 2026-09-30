#!/usr/bin/env python3
"""Prepare and validate fork changelog entries using only the Python standard library."""

import argparse
import datetime
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
# New releases use SemVer; historical four-component tags remain in the document.
SEMVER = re.compile(
    r"v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)"
    r"(?:-(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
HEADING = re.compile(r"^## \[([^\]]+)\](?: - (\d{4}-\d{2}-\d{2}))?$", re.MULTILINE)
CATEGORIES = {"Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"}


def has_notes(body):
    bullets = re.findall(r"^- (.+)$", body, re.MULTILINE)
    return any(
        text.strip() and not re.match(r"(?i)^(todo|tbd|coming soon)\b", text.strip())
        for text in bullets
    )


def sections(text):
    headings = list(HEADING.finditer(text))
    if not headings or headings[0][1] != "Unreleased":
        raise ValueError("The changelog must start with ## [Unreleased].")
    if len(re.findall(r"^## ", text, re.MULTILINE)) != len(headings):
        raise ValueError("Use ## [vX.Y.Z] - YYYY-MM-DD for release headings.")
    result = {}
    for index, heading in enumerate(headings):
        name, date = heading[1], heading[2]
        if name in result:
            raise ValueError(f"Duplicate changelog entry: {name}")
        if name == "Unreleased":
            if date:
                raise ValueError("Unreleased must not have a release date.")
        else:
            if not re.fullmatch(r"v\d+\.\d+\.\d+(?:\.\d+)?(?:[-+][0-9A-Za-z.-]+)?", name):
                raise ValueError(f"Invalid historical release heading: {name}")
            if not date:
                raise ValueError(f"Missing release date for {name}.")
            datetime.date.fromisoformat(date)
        end = headings[index + 1].start() if index + 1 < len(headings) else len(text)
        body = text[heading.end():end].strip()
        for category in re.findall(r"^### (.+)$", body, re.MULTILINE):
            if category not in CATEGORIES:
                raise ValueError(f"Unknown changelog category: {category}")
        if name != "Unreleased" and not has_notes(body):
            raise ValueError(f"{name} needs at least one non-placeholder change bullet.")
        result[name] = (heading.start(), end, body)
    return result


def release_notes(root, tag=None):
    version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    expected = "v" + version
    tag = tag or expected
    if not SEMVER.fullmatch(tag):
        raise ValueError(f"New release tags must use SemVer, for example v1.2.3: {tag}")
    if tag != expected:
        raise ValueError(f"Release tag {tag} does not match Cargo.toml version {expected}.")
    entries = sections((root / "docs/CHANGELOG.md").read_text())
    releases = [name for name in entries if name != "Unreleased"]
    if not releases or releases[0] != tag:
        raise ValueError(f"The latest dated changelog entry must be {tag}. Prepare it before tagging.")
    return entries[tag][2] + "\n"


def prepare(root, date):
    date = datetime.date.fromisoformat(date).isoformat()
    version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    tag = "v" + version
    if not SEMVER.fullmatch(tag):
        raise ValueError(f"Workspace version is not SemVer: {version}")
    path = root / "docs/CHANGELOG.md"
    text = path.read_text()
    entries = sections(text)
    if tag in entries:
        raise ValueError(f"{tag} already has an entry. Bump the workspace version first.")
    start, end, body = entries["Unreleased"]
    if not has_notes(body):
        raise ValueError("Add change bullets under Unreleased before preparing a release.")
    replacement = f"## [Unreleased]\n\n## [{tag}] - {date}\n\n{body}\n\n"
    updated = text[:start] + replacement + text[end:]
    sections(updated)
    path.write_text(updated)
    return tag


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("check", help="Check the current workspace version or a release tag")
    check.add_argument("--tag")
    notes = commands.add_parser("notes", help="Write the validated release entry to stdout")
    notes.add_argument("--tag", required=True)
    finalize = commands.add_parser("prepare", help="Move Unreleased notes to the workspace version")
    finalize.add_argument("--date", required=True, help="Release date in YYYY-MM-DD format")
    args = parser.parse_args()
    try:
        if args.command == "prepare":
            print(f"Prepared {prepare(ROOT, args.date)} in docs/CHANGELOG.md; review and commit before tagging.")
        else:
            body = release_notes(ROOT, args.tag)
            if args.command == "notes":
                sys.stdout.write(body)
            else:
                print("Changelog, workspace version, and release tag agree.")
    except (ValueError, KeyError, OSError) as error:
        parser.exit(1, f"Changelog error: {error}\n")


if __name__ == "__main__":
    main()
