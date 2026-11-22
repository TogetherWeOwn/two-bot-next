#!/usr/bin/env python3
"""Enforce the SQLx pool lint on added Rust lines in Cargo's JSON output."""

import argparse
from collections import Counter
import json
from pathlib import Path
import runpy
import sys


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = "crates/store/tests/sqlx_pool_policy_fixture.rs"
FIXTURE_SOURCE = ROOT / "scripts/ci/fixtures/sqlx_pool_policy.rs"
OWNER = "crates/store/src/pool.rs"
LINT = "clippy::disallowed_methods"
LAZY_METHOD = "use of a disallowed method `sqlx_core::pool::PoolOptions::connect_lazy_with`"


def source_spans(diagnostic):
    for primary in diagnostic["spans"]:
        if not primary["is_primary"]:
            continue
        span = primary
        while span is not None:
            path = Path(span["file_name"])
            if not path.is_absolute() or path.is_relative_to(ROOT):
                yield str(path.relative_to(ROOT) if path.is_absolute() else path), span
            expansion = span.get("expansion")
            span = expansion["span"] if expansion else None


def owner_constructor(diagnostic, path, span):
    # The store's lazy connector runs the same validated options builder as
    # its eager connector. Admit only its one raw SQLx invocation.
    return (
        path == OWNER
        and diagnostic["message"] == LAZY_METHOD
        and not span.get("expansion")
        and [line["text"].strip() for line in span["text"]] == [".connect_lazy_with(options))"]
    )


def check(lines, added, expected_fixture, output):
    fixture = Counter()
    owner_lines = set()
    violations = set()
    grandfathered = set()
    completed = False
    for line in lines:
        record = json.loads(line)
        if record["reason"] == "build-finished":
            completed = record["success"] is True
        if record["reason"] != "compiler-message":
            continue
        diagnostic = record["message"]
        rendered = diagnostic["rendered"]
        if (diagnostic["code"] or {}).get("code") != LINT:
            if rendered:
                output.write(rendered)
            continue
        spans = list(source_spans(diagnostic))
        if not spans:
            raise ValueError("SQLx lint diagnostic has no workspace location")
        path, primary = spans[0]
        location = (path, primary["line_start"])
        if path == FIXTURE:
            fixture[primary["line_start"]] += 1
        elif owner_constructor(diagnostic, path, primary):
            owner_lines.add(location)
        elif any(
            (path, number) in added
            for path, span in spans
            for number in range(span["line_start"], span["line_end"] + 1)
        ):
            if location not in violations:
                output.write(rendered)
            violations.add(location)
        else:
            grandfathered.add(location)

    if not completed:
        raise ValueError("Cargo did not report a successful build")
    if len(owner_lines) != 1:
        raise ValueError("expected exactly one canonical lazy SQLx constructor")
    if fixture != expected_fixture:
        raise ValueError(f"SQLx lint fixture expected {dict(expected_fixture)}, got {dict(fixture)}")
    print(
        f"SQLx pool lint: {sum(fixture.values())} expected fixture diagnostics, "
        f"{len(grandfathered)} grandfathered lines, {len(violations)} new violations",
        file=output,
    )
    return bool(violations)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--head-ref", required=True)
    args = parser.parse_args()
    add_lines = runpy.run_path(str(ROOT / "scripts/ci/check-patch-coverage.py"))["added_lines"]
    added, error = add_lines(args.base_ref, args.head_ref)
    if error:
        sys.exit(f"SQLx pool lint could not read changed lines: {error}")
    expected = Counter(
        number for number, line in enumerate(FIXTURE_SOURCE.read_text().splitlines(), 1)
        if line.endswith("// disallowed")
    )
    if not expected:
        sys.exit("SQLx pool lint fixture is empty")
    try:
        return check(sys.stdin, added, expected, sys.stderr)
    except (KeyError, TypeError, ValueError) as error:
        sys.exit(f"SQLx pool lint could not verify the build: {error}")


if __name__ == "__main__":
    sys.exit(main())
