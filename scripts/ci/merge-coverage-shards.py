#!/usr/bin/env python3
"""Merge sharded LCOV reports into one union report (fail-closed).

Each coverage shard uploads its own LCOV file; the merge job combines them
with key union (hit counts summed) and the unchanged HARD gate in
check-patch-coverage.py reads the merged report. Any missing, empty or
malformed shard fails the merge: partial data must never pass as measured.
"""

import argparse
import sys
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[2]


def _source_path(filename, source_roots):
    """Relativize producer paths without guessing a suffix or resolving symlinks."""
    path = PurePosixPath(filename)
    if not filename or ".." in path.parts or "\x00" in filename:
        raise ValueError(f"invalid LCOV source path: {filename!r}")
    if path.is_absolute():
        for root in source_roots:
            if path.is_relative_to(root):
                path = path.relative_to(root)
                break
        else:
            raise ValueError(
                f"LCOV source outside configured source roots: {filename!r}; "
                "pass --source-root with the producing checkout root"
            )
    if not path.parts:
        raise ValueError(f"LCOV source has no filename: {filename!r}")
    return path.as_posix()


def merge_lcov(paths, source_roots=()):
    """Union {(repo-relative file, line): count} across shard LCOV reports.

    Absolute SF paths must be under this checkout or an explicit producer root.
    Raises ValueError on a missing, empty, malformed or unmappable report.
    """
    roots = [PurePosixPath(ROOT)]
    for raw_root in source_roots:
        root = PurePosixPath(raw_root)
        if not root.is_absolute() or ".." in root.parts or root == PurePosixPath("/"):
            raise ValueError(f"invalid LCOV source root: {str(raw_root)!r}")
        roots.append(root)
    merged = {}
    if not paths:
        raise ValueError("no shard reports provided")
    for raw_path in paths:
        path = Path(raw_path)
        try:
            text = path.read_text()
        except OSError as exc:
            raise ValueError(f"shard report unreadable: {raw_path}: {exc}")
        if not text.strip():
            raise ValueError(f"shard report is empty: {raw_path}")
        current = None
        records = 0
        for raw in text.splitlines():
            if raw.startswith("SF:"):
                if current is not None:
                    raise ValueError(f"malformed LCOV (nested SF): {raw_path}")
                current = raw[3:].strip() or None
                if current is None:
                    raise ValueError(f"malformed LCOV (empty SF): {raw_path}")
                current = _source_path(current, roots)
            elif raw.startswith("DA:"):
                parts = raw[3:].split(",")
                try:
                    line, count = int(parts[0]), int(parts[1])
                except (ValueError, IndexError):
                    raise ValueError(
                        f"malformed LCOV (bad DA record): {raw_path}"
                    )
                if current is None or line < 1 or count < 0:
                    raise ValueError(
                        f"malformed LCOV (DA outside record): {raw_path}"
                    )
                key = (current, line)
                merged[key] = merged.get(key, 0) + count
                records += 1
            elif raw.strip() == "end_of_record":
                if current is None:
                    raise ValueError(
                        f"malformed LCOV (stray end_of_record): {raw_path}"
                    )
                current = None
        if current is not None:
            raise ValueError(
                f"malformed LCOV (unterminated record): {raw_path}"
            )
        if not records:
            raise ValueError(f"shard report has no DA records: {raw_path}")
    if not merged:
        raise ValueError("merged coverage is empty")
    return merged


def _write_lcov(path, hits):
    by_file = {}
    for (filename, line), count in hits.items():
        by_file.setdefault(filename, {})[line] = count
    lines = []
    for filename in sorted(by_file):
        lines.append("TN:")
        lines.append(f"SF:{filename}")
        for line in sorted(by_file[filename]):
            lines.append(f"DA:{line},{by_file[filename][line]}")
        lines.append("end_of_record")
    Path(path).write_text("\n".join(lines) + "\n")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", action="append", default=[],
                        help="shard LCOV report (repeatable)")
    parser.add_argument("--expected-shards", type=int, required=True)
    parser.add_argument("--source-root", action="append", default=[],
                        help="absolute producing checkout root (repeatable); "
                             "this checkout and repo-relative SF paths are also accepted")
    parser.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    if len(args.report) != args.expected_shards:
        print(
            f"::error::coverage merge needs exactly {args.expected_shards} "
            f"shard reports, got {len(args.report)}; failing closed",
            flush=True,
        )
        return 1
    try:
        merged = merge_lcov(args.report, source_roots=args.source_root)
    except ValueError as exc:
        print(f"::error::coverage merge failed ({exc}); failing closed",
              flush=True)
        return 1
    try:
        _write_lcov(args.out, merged)
    except OSError as exc:
        print(f"::error::could not write merged coverage: {exc}", flush=True)
        return 1
    print(
        f"merged {len(args.report)} coverage shards "
        f"({len(merged)} executable lines)",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
