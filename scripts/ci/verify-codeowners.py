#!/usr/bin/env python3
"""Verify CODEOWNERS syntax and a non-empty repository-wide ownership rule.

This offline gate verifies coverage, not GitHub account/team write access or
branch protection. CODEOWNERS remains advisory; no owner/review policy changes.
"""

import argparse
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[2]
OWNER = re.compile(r"(?:@[A-Za-z0-9][A-Za-z0-9-]*(?:/[A-Za-z0-9][A-Za-z0-9_-]*)?|[^\s@]+@[^\s@]+\.[^\s@]+)\Z")


def verify(source):
    errors = []
    catchall = False
    for line_number, line in enumerate(source.splitlines(), 1):
        # GitHub permits trailing comments, but not unsupported gitignore
        # negation, character ranges or escaped leading comment characters.
        rule = line.split("#", 1)[0].strip()
        if not rule:
            continue
        fields = rule.split()
        pattern, owners = fields[0], fields[1:]
        if any(char in pattern for char in "![]\\"):
            errors.append(f"line {line_number}: unsupported CODEOWNERS pattern {pattern!r}")
        if not owners:
            errors.append(f"line {line_number}: ownership-erasing rule {pattern!r}")
        for owner in owners:
            if not OWNER.fullmatch(owner):
                errors.append(f"line {line_number}: invalid owner {owner!r}")
        if pattern == "*" and owners and all(OWNER.fullmatch(owner) for owner in owners):
            catchall = True
    if not catchall:
        errors.append("missing non-empty '*' repository-wide ownership rule")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    path = args.root / ".github/CODEOWNERS"
    try:
        errors = verify(path.read_text())
    except (OSError, UnicodeError) as exc:
        print(f"CODEOWNERS gate: {exc}", file=sys.stderr)
        return 1
    for error in errors:
        print(f".github/CODEOWNERS: {error}", file=sys.stderr)
    if not errors:
        print("CODEOWNERS gate: repository-wide ownership verified (offline)")
    return bool(errors)


if __name__ == "__main__":
    sys.exit(main())
