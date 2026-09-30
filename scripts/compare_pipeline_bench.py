#!/usr/bin/env python3
"""Compare same-workload pipeline measurements, independently of required PR CI."""

import argparse
import json
import math
import sys
from pathlib import Path


METRICS = (
    ("peak_rss_mib",),
    ("handler_latency_us", "p50"),
    ("handler_latency_us", "p99"),
    ("db_round_trips_per_event",),
)


def metric(report, path):
    value = report["metrics"]
    for key in path:
        value = value[key]
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{'.'.join(path)} must be numeric")
    if not math.isfinite(value) or value < 0:
        raise ValueError(f"{'.'.join(path)} must be finite and nonnegative")
    return value


def validate(report):
    if report["schema_version"] != 1:
        raise ValueError("unsupported schema_version")
    if not isinstance(report["workload"], dict) or not report["workload"]:
        raise ValueError("missing workload")
    values = {path: metric(report, path) for path in METRICS}
    if values[("peak_rss_mib",)] <= 0:
        raise ValueError("RSS measurement unavailable")
    if values[("handler_latency_us", "p50")] <= 0:
        raise ValueError("latency measurement unavailable")
    if values[("handler_latency_us", "p99")] < values[("handler_latency_us", "p50")]:
        raise ValueError("p99 must be at least p50")
    return values


def compare(baseline, actual, tolerance=0.25, max_rss_mib=200):
    if not math.isfinite(tolerance) or tolerance < 0:
        raise ValueError("tolerance must be finite and nonnegative")
    if not math.isfinite(max_rss_mib) or not 0 < max_rss_mib <= 256:
        raise ValueError("RSS limit must be positive and at most the 256 MiB lite ceiling")
    before, after = validate(baseline), validate(actual)
    if baseline["workload"] != actual["workload"]:
        raise ValueError("workload differs from baseline; do not compare unlike profiles")
    failures = []
    for path in METRICS:
        limit = before[path] * (1 + tolerance)
        if after[path] > limit:
            failures.append(f"{'.'.join(path)}: {after[path]:.3f} > {limit:.3f} ({tolerance:.0%} tolerance)")
    if after[("peak_rss_mib",)] >= max_rss_mib:
        failures.append(f"peak_rss_mib: {after[('peak_rss_mib',)]:.3f} >= lite budget {max_rss_mib:.3f}")
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("actual", type=Path)
    parser.add_argument("--baseline", type=Path, default=Path("docs/pipeline-benchmark-baseline.json"))
    parser.add_argument("--tolerance", type=float, default=0.25)
    parser.add_argument("--max-rss-mib", type=float, default=200)
    args = parser.parse_args()
    try:
        failures = compare(json.loads(args.baseline.read_text()), json.loads(args.actual.read_text()), args.tolerance, args.max_rss_mib)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"INVALID benchmark: {error}", file=sys.stderr)
        return 2
    if failures:
        print("NEEDS WORK\n" + "\n".join(failures))
        return 1
    print("PASS: same workload, metrics within tolerance and lite RSS budget")
    return 0


if __name__ == "__main__":
    sys.exit(main())
