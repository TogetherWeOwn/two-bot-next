#!/usr/bin/env python3
"""Partition the nightly libtest inventory without changing its feature graph.

Keep the original skip filters during discovery, then pass exact positive names:
using substring skips to subtract other partitions can also skip a prefix-sharing
neighbour. Equal names in different test targets always have the same owner.
Doctest names contain spaces and rustdoc splits test arguments on whitespace, so
run the original doctest selection once, separately from exact libtest filters.
"""

import argparse
import hashlib
import re
import subprocess
import sys
import time


FILTER_BATCH_BYTES = 128 * 1024


def inventory(output):
    names = set()
    for line in output.splitlines():
        if line.endswith(": test") or line.endswith(": benchmark"):
            name = line.rsplit(": ", 1)[0]
            if not name or name.startswith("-") or any(c.isspace() for c in name):
                raise ValueError("invalid libtest inventory name")
            names.add(name)
        elif line and not re.fullmatch(r"\d+ tests?, \d+ benchmarks?", line):
            raise ValueError("unrecognized libtest inventory output")
    if not names:
        raise ValueError("nightly inventory selected no tests")
    return sorted(names)


def owner(name, count):
    return int.from_bytes(hashlib.sha256(name.encode()).digest()[:8], "big") % count


def batches(names):
    batch = []
    size = 0
    for name in names:
        width = len(name.encode()) + 1
        if width > FILTER_BATCH_BYTES:
            raise ValueError("libtest name exceeds argument budget")
        if size + width > FILTER_BATCH_BYTES:
            yield batch
            batch = []
            size = 0
        batch.append(name)
        size += width
    if batch:
        yield batch


def run_partition(command, index, count):
    separator = command.index("--")
    cargo = command[:separator]
    harness = command[separator + 1:]
    # Compile the original default targets once, including ordinary examples.
    # Forcing --examples into test mode hides invalid ordinary main signatures.
    if index == 0:
        subprocess.run(cargo + ["--no-run"], check=True)
    # --tests retains manifest test=true examples without executing normal ones.
    # The caller retains workspace/all-feature/locked/no-fail-fast and serial
    # ignored-test flags. Only target selection and exact filters are added.
    tests = cargo + ["--lib", "--bins", "--tests", "--"] + harness
    start = time.monotonic()
    probe = subprocess.run(tests + ["--list", "--format", "terse"],
                           check=True, stdout=subprocess.PIPE, text=True)
    names = inventory(probe.stdout)
    selected = [name for name in names if owner(name, count) == index]
    if not selected:
        raise ValueError("nightly partition selected no tests")
    print(f"Nightly partition {index}/{count}: {len(selected)}/{len(names)} unique names; "
          f"inventory {time.monotonic() - start:.2f}s", flush=True)
    failed = False
    for number, batch in enumerate(batches(selected), 1):
        start = time.monotonic()
        result = subprocess.run(tests + ["--exact"] + batch, check=False)
        print(f"Nightly partition {index} batch {number}: {len(batch)} names, "
              f"{time.monotonic() - start:.2f}s, exit={result.returncode}", flush=True)
        failed |= result.returncode != 0
    if index == 0:
        start = time.monotonic()
        result = subprocess.run(cargo + ["--doc", "--"] + harness, check=False)
        print(f"Nightly doctests: {time.monotonic() - start:.2f}s, "
              f"exit={result.returncode}", flush=True)
        failed |= result.returncode != 0
    return int(failed)


def main():
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--index", type=int, required=True)
    parser.add_argument("--count", type=int, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not 0 <= args.index < args.count <= 16:
        parser.error("partition index must be in range and count must be 1..16")
    expected = ["cargo", "test", "--workspace", "--all-features", "--locked", "--no-fail-fast"]
    if command[:len(expected) + 1] != expected + ["--"]:
        parser.error("expected the unchanged nightly cargo test command")
    harness = command[len(expected) + 1:]
    if harness[:2] != ["--include-ignored", "--test-threads=1"]:
        parser.error("nightly must include ignored tests and remain serial")
    skips = harness[2:]
    if len(skips) % 2 or any(skips[i] != "--skip" or not skips[i + 1]
                            for i in range(0, len(skips), 2)):
        parser.error("only existing skip pairs may follow the serial ignored-test flags")
    return run_partition(command, args.index, args.count)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.SubprocessError) as exc:
        print(f"nightly partition failed: {exc}", file=sys.stderr)
        sys.exit(1)
