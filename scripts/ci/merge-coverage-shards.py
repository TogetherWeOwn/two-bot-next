#!/usr/bin/env python3
"""Merge sharded LCOV reports into one union report (fail-closed).

Each coverage shard uploads its LCOV file and identity; the merge job checks
both before combining the reports by key (hit counts summed). The HARD gate in
check-patch-coverage.py reads the merged report. Any missing, empty or
malformed shard fails the merge: partial data must never pass as measured.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from script_intake import decode_json
sys.path.pop(0)


def artifact_context(expected):
    """Use the checkout, selector and executable matrix shared by both jobs."""
    source = subprocess.run(
        ["git", "rev-parse", "--verify", "HEAD"], capture_output=True, text=True,
        timeout=15, check=True,
    ).stdout.strip()
    if not re.fullmatch(r"[0-9a-f]{40}", source) or source != os.environ.get("GITHUB_SHA"):
        raise ValueError("coverage checkout does not match the workflow SHA")

    selector = decode_json(os.environ.get("COVERAGE_SELECTOR", ""), max_depth=4)
    if not isinstance(selector, dict) or any(not isinstance(value, str) for value in selector.values()):
        raise ValueError("invalid job-inputs outputs for coverage")

    matrix = decode_json(os.environ.get("COVERAGE_MATRIX", ""), max_depth=4)
    entries = matrix.get("include") if isinstance(matrix, dict) else None
    if not isinstance(entries, list) or not entries or len(entries) != expected:
        raise ValueError("coverage matrix does not match the expected shard count")
    shards = {}
    for entry in entries:
        if not isinstance(entry, dict) or entry.keys() != {"shard_index", "legs"}:
            raise ValueError("invalid coverage matrix entry")
        index, legs = entry["shard_index"], entry["legs"]
        if (type(index) is not int or index < 0 or index in shards
                or not isinstance(legs, str) or not legs.strip()):
            raise ValueError("invalid coverage shard index or legs in the matrix")
        shards[index] = legs

    # GitHub retains successful shards across attempts of the same run.
    # Canonicalize the inputs so JSON serialization order cannot break reuse.
    identity = json.dumps([source, selector, sorted(shards.items())], sort_keys=True)
    return hashlib.sha256(identity.encode()).hexdigest(), shards


def report_identity(report, identity, index):
    with report.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return f"{identity}:{index}:{digest}\n".encode()


def bind_report(index, expected):
    identity, shards = artifact_context(expected)
    if index not in shards or os.environ.get("SHARD_LEGS") != shards[index]:
        raise ValueError(f"executed coverage shard {index} does not match the matrix")
    report = Path(f"lcov-shard-{index}.info")
    if report.stat().st_size == 0:
        raise ValueError(f"shard report is empty: {report}")
    report.with_suffix(".identity").write_bytes(report_identity(report, identity, index))


def artifact_reports(directory, expected):
    identity, shards = artifact_context(expected)
    reports = {index: directory / f"lcov-shard-{index}.info" for index in sorted(shards)}
    names = {report.with_suffix(suffix).name for report in reports.values()
             for suffix in (".info", ".identity")}
    if {path.name for path in directory.iterdir()} != names:
        raise ValueError(f"downloaded reports and identity files do not match the {expected} planned indices")
    for index, report in reports.items():
        bound_identity = report_identity(report, identity, index)
        with report.with_suffix(".identity").open("rb") as stamp:
            if stamp.read(len(bound_identity) + 1) != bound_identity:
                raise ValueError(f"coverage shard {index} identity mismatch (source, selector, plan or report)")
    return list(reports.values())


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
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--report", action="append", default=[], help="local LCOV report (repeatable)")
    mode.add_argument("--bind-shard", type=int, help="bind this matrix shard before upload")
    mode.add_argument("--artifact-dir", type=Path, help="verify downloaded shard artifacts before merging")
    parser.add_argument("--expected-shards", type=int, required=True)
    parser.add_argument("--source-root", action="append", default=[],
                        help="absolute producing checkout root (repeatable); "
                             "this checkout and repo-relative SF paths are also accepted")
    parser.add_argument("--out")
    args = parser.parse_args(argv)
    if args.bind_shard is None and not args.out:
        parser.error("--out is required to merge coverage reports")
    try:
        if args.bind_shard is not None:
            bind_report(args.bind_shard, args.expected_shards)
            return 0
        reports = args.report
        if args.artifact_dir:
            reports = artifact_reports(args.artifact_dir, args.expected_shards)
        if len(reports) != args.expected_shards:
            raise ValueError(f"needs exactly {args.expected_shards} shard reports, got {len(reports)}")
        merged = merge_lcov(reports, source_roots=args.source_root)
    except (OSError, ValueError, RecursionError, subprocess.SubprocessError) as exc:
        print(f"::error::coverage report processing failed ({exc}); failing closed",
              flush=True)
        return 1
    try:
        _write_lcov(args.out, merged)
    except OSError as exc:
        print(f"::error::could not write merged coverage: {exc}", flush=True)
        return 1
    print(
        f"merged {len(reports)} coverage shards "
        f"({len(merged)} executable lines)",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
