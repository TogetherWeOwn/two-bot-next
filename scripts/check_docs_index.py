#!/usr/bin/env python3
"""Check the README-linked docs index and byte budgets, without network.

Run with --json for {missing,stale,oversize}: sorted arrays of repo-relative
paths. Invalid/duplicate rows appear in stale as <index path>:<line>.
An undecodable index is stale as its path so --json keeps its shape.
Uppercase .MD files are missing until renamed to lowercase .md targets.
Exit 0 means all arrays are empty; exit 1 means repair the named index rows or
split the oversized doc. --root selects a scratch checkout for mutation tests.
Purposes live only in the index's link labels, never in a second manifest.

Only strict README links to docs-root index-*.md sections are assembled, at
that link's position. Section links are not recursive. All row destinations
are relative to docs/ (README and sections are siblings).
The indexes merge with the union driver (.gitattributes). --dedupe is the
only write mode: it removes exact-duplicate lines in each linked index, then
reports exactly like a check run. It never reorders lines or picks between
rows sharing a target with different labels, or across files.
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
    "docs/runbook.md": "Existing operations guide; use section and alert navigation.",
    "docs/soak-checklist.md": "Generated coverage ledger; use the start-here section map.",
}
ROW = re.compile(r"- \[([^\]\n]+)\]\(([A-Za-z0-9_./-]+\.md)\)")
FRAGMENT = re.compile(r"index-[^/]+\.md")
ENTRYPOINT = "docs/README.md"


def index_rows(text, source, stale):
    """Yield validated (line, target) pairs; retain the strict line-based rule."""
    for number, line in enumerate(text.splitlines(), 1):
        if not line.startswith("- "):
            continue
        match = ROW.fullmatch(line)
        if match:
            purpose, target = match.groups()
            path = PurePosixPath(target)
            if (purpose.strip() and not path.is_absolute() and ".." not in path.parts
                    and path.as_posix() == target):
                yield number, target
                continue
        stale.add(f"{source}:{number}")


def check(root=ROOT, large_docs=LARGE_DOCS):
    docs = root / "docs"
    # Case-insensitive inventory: ROW only accepts lowercase .md targets, so an
    # uppercase .MD file lands in missing until renamed, never silently ignored.
    files = {
        p.relative_to(root).as_posix(): p
        for p in docs.rglob("*")
        if p.is_file() and p.suffix.lower() == ".md"
    }
    indexed = set()
    indexes = {ENTRYPOINT}
    stale = set()

    def visit(source):
        if source not in files:
            return
        try:
            text = files[source].read_text(encoding="utf-8")
        except UnicodeDecodeError:
            stale.add(source)
            return
        for number, target in index_rows(text, source, stale):
            name = f"docs/{target}"
            if name in indexed:
                stale.add(f"{source}:{number}")
                continue
            indexed.add(name)
            if name not in files and name not in GENERATED_DOCS:
                stale.add(name)
            if source == ENTRYPOINT and FRAGMENT.fullmatch(target):
                indexes.add(name)
                visit(name)

    visit(ENTRYPOINT)
    missing = set(files) - indexed - GENERATED_DOCS
    # A section is discoverable only through a direct README link, never a
    # row in another section. Unlinked sections and their rows stay orphans.
    missing.update(name for name in files
                   if FRAGMENT.fullmatch(name.removeprefix("docs/")) and name not in indexes)
    if ENTRYPOINT not in files:
        missing.add(ENTRYPOINT)
    oversize = {
        name for name, path in files.items()
        if (name in indexes and path.stat().st_size >= INDEX_LIMIT)
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
    """Dedupe README and its linked sections independently; return any change.

    Missing/non-file indexes are no-ops. An undecodable index is left untouched
    for the normal fail-closed report, without preventing other section repairs.
    Orphans are never read or rewritten.
    """
    sources = [ENTRYPOINT]
    changed = False
    for source in sources:
        index = root / source
        if not index.is_file():
            continue
        try:
            text = index.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        if source == ENTRYPOINT:
            sources.extend(dict.fromkeys(
                f"docs/{target}" for _, target in index_rows(text, source, set())
                if FRAGMENT.fullmatch(target)
            ))
        cleaned = dedupe_text(text)
        if cleaned != text:
            index.write_text(cleaned, encoding="utf-8")
            changed = True
    return changed


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
        print("deduped: linked docs indexes")
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
