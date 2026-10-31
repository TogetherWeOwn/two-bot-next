"""Fail closed on empty CycloneDX inventories or missing Cargo.lock packages."""

import argparse
import json
import math
from pathlib import Path
import re
import sys
import tomllib


OVER_CAP = "SBOM input exceeds the byte cap"
DUP_KEYS = "SBOM JSON carries duplicate object keys"
BAD_CONST = "SBOM JSON carries a non-finite number"
TOO_DEEP = "SBOM components exceed the nesting bound"
LOCK_TOO_DEEP = "SBOM lockfile exceeds the nesting bound"
MISSING_FILE = "SBOM input file is missing or unreadable"
# Real intake is ~17 KiB BOM / ~66 KiB lockfile / <100 B provenance; the
# 8 MiB JSON / 1 MiB text fixtures must refuse, and the <64 KiB nesting
# probe must reach the depth bound instead of the byte cap.
JSON_CAP = 1 << 20
TEXT_CAP = 1 << 20
MAX_DEPTH = 100


def _reject_duplicate_keys(pairs):
    seen = set()
    for key, _ in pairs:
        if key in seen:
            raise ValueError(DUP_KEYS)
        seen.add(key)
    return dict(pairs)


def _reject_constant(value):
    raise ValueError(BAD_CONST)


def _reject_non_finite_float(text):
    number = float(text)
    if not math.isfinite(number):
        raise ValueError(BAD_CONST)
    return number


def read_capped_text(path, cap):
    try:
        if path.stat().st_size > cap:
            raise ValueError(OVER_CAP)
        return path.read_text()
    except OSError:
        raise ValueError(MISSING_FILE) from None


def parse_bom_text(text):
    try:
        return json.loads(
            text,
            object_pairs_hook=_reject_duplicate_keys,
            parse_constant=_reject_constant,
            parse_float=_reject_non_finite_float,
        )
    except RecursionError:
        raise ValueError(TOO_DEEP) from None


def parse_lockfile_text(text):
    try:
        return tomllib.loads(text)
    except RecursionError:
        raise ValueError(LOCK_TOO_DEEP) from None


def components(bom, _depth=0):
    if _depth > MAX_DEPTH:
        raise ValueError(TOO_DEEP)
    for component in bom.get("components", []):
        yield component
        yield from components(component, _depth + 1)


def read_bom(path):
    bom = parse_bom_text(read_capped_text(path, JSON_CAP))
    if bom.get("bomFormat") != "CycloneDX" or not bom.get("specVersion"):
        raise ValueError(f"Not a CycloneDX SBOM: {path}")
    inventory = list(components(bom))
    if not inventory:
        raise ValueError(f"Empty SBOM: {path}")
    return inventory


def validate(lockfile, directory):
    rust = read_bom(directory / "rust-workspace.cdx.json")
    expected = {(pkg["name"], pkg["version"]) for pkg in parse_lockfile_text(
        read_capped_text(lockfile, JSON_CAP))["package"]}
    actual = {(pkg.get("name"), pkg.get("version")) for pkg in rust}
    missing = expected - actual
    if missing:
        raise ValueError(f"Rust SBOM omitted locked packages: {sorted(missing)}")
    image = read_bom(directory / "container-image.cdx.json")
    if not any(pkg.get("purl", "").startswith("pkg:deb/debian/") for pkg in image):
        raise ValueError("Image SBOM omitted Debian runtime packages")
    for name, pattern in [("source-sha.txt", r"[0-9a-f]{40}"), ("image-id.txt", r"sha256:[0-9a-f]{64}")]:
        if not re.fullmatch(pattern, read_capped_text(directory / name, TEXT_CAP).strip()):
            raise ValueError(f"Missing or invalid provenance: {name}")
    print(f"PASS CycloneDX inventories: {len(expected)} locked Rust packages; {len(image)} image components")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("lockfile", type=Path)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    try:
        validate(args.lockfile, args.directory)
    except ValueError as error:
        print(error, file=sys.stderr)
        raise SystemExit(1)
