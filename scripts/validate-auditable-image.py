"""Reconcile native runtime-crate inventory and Grype matches with Cargo.lock.

Lockfile-only entries remain in the existing dependency gate. This check never
suppresses a finding or claims function-level reachability.
"""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import sys


spec = importlib.util.spec_from_file_location(
    "validate_sbom", Path(__file__).with_name("validate-sbom.py"))
sbom = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sbom)
CAP = 8 << 20
CATALOGER = "cargo-auditable-binary-cataloger"
BINARY = "/home/nonroot/two-bot"


def read_json(path):
    value = sbom.parse_bom_text(sbom.read_capped_text(path, CAP))
    if not isinstance(value, dict):
        raise ValueError("Expected a native scanner JSON object")
    return value


def package_tuple(package):
    name, version = package["name"], package["version"]
    if not isinstance(name, str) or not isinstance(version, str) or not name or not version:
        raise ValueError("Invalid crate tuple")
    return name, version


def validate(directory):
    image_id = sbom.read_capped_text(directory / "image-id.txt", 100).strip()
    source_sha = sbom.read_capped_text(directory / "source-sha.txt", 100).strip()
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id) or not re.fullmatch(r"[0-9a-f]{40}", source_sha):
        raise ValueError("Invalid build provenance")
    lockfile = directory / "Cargo.lock"
    locked = {package_tuple(p) for p in sbom.parse_lockfile_text(
        sbom.read_capped_text(lockfile, sbom.TEXT_CAP))["package"]}
    inventory = read_json(directory / "runtime-crates.syft.json")
    source = inventory["source"]
    metadata = source["metadata"]
    config_id = metadata["imageID"]
    repo_digests = metadata.get("repoDigests", [])
    if (not isinstance(config_id, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", config_id)
            or not isinstance(repo_digests, list)
            or any(not isinstance(digest, str) or not re.fullmatch(
                r"[^@\s]+@sha256:[0-9a-f]{64}", digest) for digest in repo_digests)):
        raise ValueError("Invalid native image identity")
    # Docker's containerd store reports a manifest digest as .Id; Syft's imageID
    # is the config digest. Require the exact immutable input and a native digest
    # binding, never a mutable tag, while retaining both identities in the receipt.
    identities = {config_id} | {digest.rsplit("@", 1)[1] for digest in repo_digests}
    if (source["type"] != "image" or metadata["userInput"] != image_id or image_id not in identities
            or metadata["os"] != "linux" or metadata["architecture"] != "amd64"):
        raise ValueError("Runtime crate inventory is not for this linux/amd64 image ID")
    if inventory["descriptor"]["name"] != "syft" or inventory["descriptor"]["version"] != "1.56.0":
        raise ValueError("Unexpected inventory tool")
    crates = set()
    for package in inventory["artifacts"]:
        if (package["type"] != "rust-crate" or package["foundBy"] != CATALOGER
                or not package["locations"]
                or any(location["path"] != BINARY for location in package["locations"])):
            raise ValueError("Crate evidence did not come from the runtime binary")
        crates.add(package_tuple(package))
    if not {"two-bot", "tokio", "libc"}.issubset({name for name, _ in crates}):
        raise ValueError("Empty or incomplete runtime crate inventory")
    if crates - locked:
        raise ValueError("Runtime inventory contains crates absent from Cargo.lock")

    report = read_json(directory / "runtime-crates.grype.json")
    if report["descriptor"]["name"] != "grype" or report["descriptor"]["version"] != "0.121.1":
        raise ValueError("Unexpected matching tool")
    # Grype preserves the native Syft image source when reading a Syft SBOM.
    if (report["source"]["type"] != "image" or report["source"]["target"]["imageID"] != config_id
            or report["source"]["target"]["userInput"] != image_id):
        raise ValueError("Grype did not match this runtime image inventory")
    if not isinstance(report["matches"], list) or report.get("ignoredMatches"):
        raise ValueError("Invalid or suppressed Grype matches")
    for match in report["matches"]:
        if match["artifact"]["type"] != "rust-crate" or package_tuple(match["artifact"]) not in crates:
            raise ValueError("Grype match is absent from the runtime inventory")
    checksums = {}
    for name in ("Cargo.lock", "runtime-crates.syft.json", "runtime-crates.grype.json"):
        checksums[name] = hashlib.sha256((directory / name).read_bytes()).hexdigest()
    return {
        "source_sha": source_sha, "image_id": image_id, "config_id": config_id, "sha256": checksums,
        "runtime_crates": len(crates), "grype_matches": len(report["matches"]),
        "lockfile_only": [dict(name=name, version=version) for name, version in sorted(locked - crates)],
        "suppressed_count": 0,
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    try:
        result = validate(args.directory)
        (args.directory / "runtime-crates-check.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"PASS runtime inventory: {result['runtime_crates']} locked crates; {result['grype_matches']} matches (not a vulnerability verdict)")
    except (ValueError, KeyError, TypeError, OSError, RecursionError):
        print("Runtime crate inventory validation failed", file=sys.stderr)
        raise SystemExit(1)
