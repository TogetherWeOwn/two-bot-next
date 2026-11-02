#!/usr/bin/env python3
"""Check docs/README.md discovery and byte budgets, without network.

Run with --json for {missing,stale,oversize}: sorted arrays of repo-relative
paths. Invalid/duplicate index rows appear in stale as docs/README.md:<line>.
An undecodable index is stale as docs/README.md so --json keeps its shape.
Uppercase .MD files are missing until renamed to lowercase .md targets.
Exit 0 means all arrays are empty; exit 1 means repair the named index rows or
split the oversized doc. --root selects a scratch checkout for mutation tests.
Purposes live only in the index's link labels, never in a second manifest.

The index merges with the union driver (.gitattributes), so parallel row
insertions combine instead of conflicting. --dedupe is the only write mode:
it removes the exact-duplicate lines union can leave behind, then reports
exactly like a check run. It never reorders lines and never picks between
rows that share a target with different labels; those stay stale here.
"""

import argparse
import json
import re
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[1]
# The authored commands.md landing stays indexed. This optional build output
# has separate read-only drift tests and still obeys the byte budget if present.
GENERATED_DOCS = frozenset({"docs/generated/commands.md"})
DOC_LIMIT = 64 * 1024
INDEX_LIMIT = 12_000  # Strictly below 12 KB, including UTF-8 and Markdown syntax.
LARGE_DOCS = {
    "docs/metrics.md": "Existing metric catalog; split by area in a separate content change.",
    "docs/observability-event-catalog.md": "Existing observability event catalog; split by area in a separate content change.",
    "docs/parity.md": "Historical parity ledger; use the start-here section map.",
    "docs/production-deploy.md": "Promotion-gate reference for every pre- and post-deploy gate; use section navigation.",
    "docs/runbook.md": "Existing operations guide; use section and alert navigation.",
    "docs/soak-checklist.md": "Generated coverage ledger; use the start-here section map.",
}
ROW = re.compile(r"- \[([^\]\n]+)\]\(([A-Za-z0-9_./-]+\.md)\)")


def check(root=ROOT, large_docs=LARGE_DOCS):
    docs = root / "docs"
    # Case-insensitive inventory: ROW only accepts lowercase .md targets, so an
    # uppercase .MD file lands in missing until renamed, never silently ignored.
    files = {
        p.relative_to(root).as_posix(): p
        for p in docs.rglob("*")
        if p.is_file() and p.suffix.lower() == ".md"
    }
    index = docs / "README.md"
    indexed = set()
    stale = set()
    if index.is_file():
        try:
            text = index.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            stale.add("docs/README.md")
            text = ""
        for number, line in enumerate(text.splitlines(), 1):
            if not line.startswith("- "):
                continue
            match = ROW.fullmatch(line)
            if not match:
                stale.add(f"docs/README.md:{number}")
                continue
            purpose, target = match.groups()
            path = PurePosixPath(target)
            name = f"docs/{target}"
            if (not purpose.strip() or path.is_absolute() or ".." in path.parts
                    or path.as_posix() != target or name in indexed):
                stale.add(f"docs/README.md:{number}")
                continue
            indexed.add(name)
            if name not in files and name not in GENERATED_DOCS:
                stale.add(name)
    missing = set(files) - indexed - GENERATED_DOCS
    if not index.is_file():
        missing.add("docs/README.md")
    oversize = {
        name for name, path in files.items()
        if (name == "docs/README.md" and path.stat().st_size >= INDEX_LIMIT)
        or (path.stat().st_size > DOC_LIMIT and name not in large_docs)
    }
    return {"missing": sorted(missing), "stale": sorted(stale), "oversize": sorted(oversize)}


def dedupe_text(text):
    """Canonical index text after a union merge: first occurrence wins.

    Exact-duplicate non-blank lines drop out, blank runs collapse to one
    blank line, and the result ends with exactly one newline (empty stays
    empty). Lines are never reordered or rewritten beyond that.
    """
    seen = set()
    kept = []
    for line in text.splitlines():
        if not line.strip():
            if kept and kept[-1] != "":
                kept.append("")
            continue
        if line in seen:
            continue
        seen.add(line)
        kept.append(line)
    while kept and kept[-1] == "":
        kept.pop()
    return "\n".join(kept) + "\n" if kept else ""


def dedupe(root=ROOT):
    """Rewrite docs/README.md under root deduped; return True if it changed.

    A missing (or non-file) index is a no-op, and UnicodeDecodeError
    propagates without any write, so the caller falls through to the normal
    fail-closed check report in both cases.
    """
    index = root / "docs" / "README.md"
    if not index.is_file():
        return False
    text = index.read_text(encoding="utf-8")
    cleaned = dedupe_text(text)
    if cleaned == text:
        return False
    index.write_text(cleaned, encoding="utf-8")
    return True


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit only the stable JSON report")
    parser.add_argument("--dedupe", action="store_true",
                        help="remove exact-duplicate index lines left by a union merge, then check")
    parser.add_argument("--root", type=Path, default=ROOT, help="repository root (default: script checkout)")
    args = parser.parse_args(argv)
    changed = False
    if args.dedupe:
        try:
            changed = dedupe(args.root)
        except (FileNotFoundError, UnicodeDecodeError):
            changed = False
    report = check(args.root)
    if changed and not args.json:
        print("deduped: docs/README.md")
    if args.json:
        print(json.dumps(report, sort_keys=True))
    elif any(report.values()):
        for kind, names in report.items():
            for name in names:
                print(f"{kind}: {name}")
        print("Repair index rows for missing/stale docs; split oversize docs or review an explicit LARGE_DOCS entry.")
    else:
        print("PASS: docs index complete; byte budgets satisfied")
    return int(any(report.values()))


if __name__ == "__main__":
    raise SystemExit(main())
