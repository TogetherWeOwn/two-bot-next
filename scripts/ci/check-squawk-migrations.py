#!/usr/bin/env python3
"""Advisory lint for risky Postgres migration statements (squawk subset).

Scans changed migration files for statements that take disruptive locks or
break running application code. Deterministic: standard library only, no
database, no network, no third-party packages. See docs/migrations.md.

Rules use squawk-compatible IDs so the findings map 1:1 onto a future
squawk binary or shared reusable workflow:
- adding-required-field: ADD COLUMN with NOT NULL and no DEFAULT.
- creating-index: CREATE INDEX without CONCURRENTLY.
- adding-check-constraint: ADD CONSTRAINT CHECK without NOT VALID.
- changing-column-type: ALTER COLUMN ... TYPE rewrites the table.
- dropping-object: DROP TABLE or DROP COLUMN breaks running code.

Advisory by default: findings print as ::warning annotations and exit 0.
--strict prints ::error and exits 1 (the promotion path to a required
check). Tooling failures (unresolvable baseline, unreadable file) always
exit 1 so blindness can never read as clean.
"""

import argparse
import fnmatch
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_GLOB = "crates/*/migrations/*.sql"

RULES = (
    "adding-required-field",
    "creating-index",
    "adding-check-constraint",
    "changing-column-type",
    "dropping-object",
)

ADVICE = {
    "adding-required-field": (
        "ADD COLUMN with NOT NULL and no DEFAULT rewrites the table under an "
        "exclusive lock; add the column nullable with a DEFAULT (or backfill), "
        "then SET NOT NULL in a later migration"
    ),
    "creating-index": (
        "CREATE INDEX without CONCURRENTLY blocks writes; "
        "use CREATE INDEX CONCURRENTLY"
    ),
    "adding-check-constraint": (
        "CHECK constraint without NOT VALID scans and locks the table; "
        "add it as NOT VALID, then VALIDATE CONSTRAINT separately"
    ),
    "changing-column-type": (
        "ALTER COLUMN ... TYPE rewrites the table; prefer an additive "
        "migration (new column, backfill, swap)"
    ),
    "dropping-object": (
        "DROP TABLE / DROP COLUMN breaks running application code; "
        "coordinate removal across deploys instead of dropping in one step"
    ),
}

ADD_COLUMN = re.compile(r"\bADD\s+(?:COLUMN\s+)?(?:IF\s+NOT\s+EXISTS\s+)?", re.IGNORECASE)
NOT_NULL = re.compile(r"\bNOT\s+NULL\b", re.IGNORECASE)
DEFAULT = re.compile(r"\bDEFAULT\b", re.IGNORECASE)
CREATE_INDEX = re.compile(r"\bCREATE\s+(?:UNIQUE\s+)?INDEX\b", re.IGNORECASE)
CONCURRENTLY = re.compile(r"\bCONCURRENTLY\b", re.IGNORECASE)
ADD_CHECK = re.compile(r"\bADD\s+(?:CONSTRAINT\s+\S+\s+)?CHECK\b", re.IGNORECASE)
NOT_VALID = re.compile(r"\bNOT\s+VALID\b", re.IGNORECASE)
ALTER_TYPE = re.compile(r"\bALTER\s+TABLE\b.*\bTYPE\s+[A-Za-z]", re.IGNORECASE | re.DOTALL)
DROP_OBJECT = re.compile(r"\bDROP\s+(?:TABLE|COLUMN)\b", re.IGNORECASE)
DOLLAR_OPEN = re.compile(r"\$[A-Za-z_][A-Za-z0-9_]*\$|\$\$")


class SquawkError(ValueError):
    pass


def blanked(text):
    """Return text with string, quoted-identifier, dollar-quoted and comment
    spans replaced by spaces (newlines preserved), so keyword matching and
    statement splitting never fire inside literals or comments."""
    chars = list(text)
    n = len(text)

    def blank(start, end):
        for i in range(start, end):
            if chars[i] != "\n":
                chars[i] = " "

    i = 0
    while i < n:
        two = text[i : i + 2]
        if two == "--":
            end = text.find("\n", i)
            blank(i, n if end < 0 else end)
            i = n if end < 0 else end
        elif two == "/*":
            end = text.find("*/", i + 2)
            end = n if end < 0 else end + 2
            blank(i, end)
            i = end
        elif text[i] == "'":
            begin = i
            i += 1
            while i < n:
                if text[i] == "'":
                    if text[i + 1 : i + 2] == "'":
                        i += 2
                        continue
                    i += 1
                    break
                i += 1
            blank(begin, i)
        elif text[i] == '"':
            begin = i
            i += 1
            while i < n:
                if text[i] == '"':
                    if text[i + 1 : i + 2] == '"':
                        i += 2
                        continue
                    i += 1
                    break
                i += 1
            blank(begin, i)
        elif text[i] == "$":
            match = DOLLAR_OPEN.match(text, i)
            if match:
                tag = match.group()
                close = text.find(tag, match.end())
                end = n if close < 0 else close + len(tag)
                blank(i, end)
                i = end
            else:
                i += 1
        else:
            i += 1
    return "".join(chars)


def statements(text):
    """Yield (line, sql) for each ;-terminated statement in cleaned text."""
    cleaned = blanked(text)
    start = 0
    for match in re.finditer(";", cleaned):
        chunk = cleaned[start : match.start()]
        if chunk.strip():
            offset = start
            while offset < match.start() and cleaned[offset] in " \t\n\r":
                offset += 1
            yield cleaned.count("\n", 0, offset) + 1, chunk.strip()
        start = match.end()
    tail = cleaned[start:].strip()
    if tail:
        yield cleaned.count("\n", 0, start) + 1, tail


def lint_statement(sql):
    """Return the list of rule IDs violated by one cleaned statement."""
    found = []
    if ADD_COLUMN.search(sql) and NOT_NULL.search(sql) and not DEFAULT.search(sql):
        found.append("adding-required-field")
    if CREATE_INDEX.search(sql) and not CONCURRENTLY.search(sql):
        found.append("creating-index")
    if ADD_CHECK.search(sql) and not NOT_VALID.search(sql):
        found.append("adding-check-constraint")
    if ALTER_TYPE.search(sql):
        found.append("changing-column-type")
    if DROP_OBJECT.search(sql):
        found.append("dropping-object")
    return found


def lint_text(text):
    """Return [(line, rule)] findings for one migration file's contents."""
    findings = []
    for line, sql in statements(text):
        for rule in lint_statement(sql):
            findings.append((line, rule))
    return findings


def matches_any(path, patterns):
    return any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns)


def discover_all(root, globs, excludes):
    found = []
    for pattern in globs:
        for file in sorted(root.glob(pattern)):
            if file.is_file():
                relative = file.relative_to(root).as_posix()
                if not matches_any(relative, excludes):
                    found.append(relative)
    return found


def git(root, *args):
    result = subprocess.run(
        ["git", "-C", str(root), *args], capture_output=True
    )
    if result.returncode:
        detail = result.stderr.decode(errors="replace").strip()
        raise SquawkError(f"cannot read Git baseline: {detail}")
    return result.stdout.decode().split("\0")


def changed_paths(root, base_ref):
    resolved = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--verify", "--end-of-options", f"{base_ref}^{{commit}}"],
        capture_output=True,
    )
    if resolved.returncode:
        raise SquawkError(f"unresolvable Git baseline {base_ref!r}")
    records = git(root, "diff", "--name-only", "-z", f"{base_ref}...HEAD", "--")
    return [record for record in records if record]


def select_files(root, globs, excludes, base_ref):
    if base_ref is None:
        return discover_all(root, globs, excludes)
    selected = []
    for path in changed_paths(root, base_ref):
        parts = PurePosixPath(path).parts
        if len(parts) < 4 or parts[0] == ".." or ".." in parts:
            continue
        if matches_any(path, globs) and not matches_any(path, excludes):
            selected.append(path)
    return sorted(selected)


def check(root, globs, excludes, base_ref):
    findings = []
    for relative in select_files(root, globs, excludes, base_ref):
        try:
            text = (root / relative).read_text()
        except OSError as error:
            raise SquawkError(f"{relative}: {error}") from error
        for line, rule in lint_text(text):
            findings.append((relative, line, rule))
    return findings


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument(
        "--migration-glob",
        action="append",
        default=[DEFAULT_GLOB],
        help="repeatable glob for migration files (default: %(default)s)",
    )
    parser.add_argument(
        "--exclude",
        action="append",
        default=[],
        help="repeatable fnmatch exclusion applied after the globs",
    )
    parser.add_argument("--base-ref", default=None, help="Git baseline; lint files changed since it (default: lint all files)")
    parser.add_argument("--strict", action="store_true", help="exit 1 on findings (required-check mode)")
    args = parser.parse_args(argv)
    try:
        findings = check(args.root, args.migration_glob, args.exclude, args.base_ref)
    except (SquawkError, OSError) as error:
        print(f"squawk gate: {error}", file=sys.stderr)
        return 1
    level = "error" if args.strict else "warning"
    for relative, line, rule in findings:
        print(f"::{level} file={relative},line={line}::[squawk:{rule}] {ADVICE[rule]}")
    if findings:
        files = sorted({relative for relative, _, _ in findings})
        print(
            f"squawk gate: {len(findings)} finding(s) in {len(files)} file(s)"
            + ("" if args.strict else " (advisory)")
        )
    else:
        print("squawk gate: no risky migration statements")
    return 1 if args.strict and findings else 0


if __name__ == "__main__":
    sys.exit(main())
