"""Fail closed on empty CycloneDX inventories or missing Cargo.lock packages."""

import json
from pathlib import Path
import re
import sys
import tomllib


def components(bom):
    for component in bom.get("components", []):
        yield component
        yield from components(component)


def read_bom(path):
    bom = json.loads(path.read_text())
    if bom.get("bomFormat") != "CycloneDX" or not bom.get("specVersion"):
        raise ValueError(f"Not a CycloneDX SBOM: {path}")
    inventory = list(components(bom))
    if not inventory:
        raise ValueError(f"Empty SBOM: {path}")
    return inventory


def validate(lockfile, directory):
    rust = read_bom(directory / "rust-workspace.cdx.json")
    expected = {(pkg["name"], pkg["version"]) for pkg in tomllib.loads(lockfile.read_text())["package"]}
    actual = {(pkg.get("name"), pkg.get("version")) for pkg in rust}
    missing = expected - actual
    if missing:
        raise ValueError(f"Rust SBOM omitted locked packages: {sorted(missing)}")
    image = read_bom(directory / "container-image.cdx.json")
    if not any(pkg.get("purl", "").startswith("pkg:deb/debian/") for pkg in image):
        raise ValueError("Image SBOM omitted Debian runtime packages")
    for name, pattern in [("source-sha.txt", r"[0-9a-f]{40}"), ("image-id.txt", r"sha256:[0-9a-f]{64}")]:
        if not re.fullmatch(pattern, (directory / name).read_text().strip()):
            raise ValueError(f"Missing or invalid provenance: {name}")
    print(f"PASS CycloneDX inventories: {len(expected)} locked Rust packages; {len(image)} image components")


if __name__ == "__main__":
    validate(Path(sys.argv[1]), Path(sys.argv[2]))
