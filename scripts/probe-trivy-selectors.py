"""Measure pinned Trivy ignore semantics using isolated copies of a retained report.

This is an offline report-conversion probe, not a rescan or exception activation.
No repository ignore file, raw scan report, vulnerability gate or clock is changed.
"""

import argparse
from copy import deepcopy
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from urllib.parse import quote

spec = importlib.util.spec_from_file_location("preflight", Path(__file__).with_name("vulnerability-preflight.py"))
preflight = importlib.util.module_from_spec(spec)
spec.loader.exec_module(preflight)


def replace_tuple(scan, evidence, row, name=None, version=None, architecture=None):
    old = preflight.package_tuple(row["PkgName"], row["InstalledVersion"], row["PkgIdentifier"]["PURL"])
    name, version, architecture = name or old[0], version or old[1], architecture or old[2]
    epoch, separator, plain = version.partition(":")
    purl = f"pkg:deb/debian/{name}@{quote(plain if separator else version)}?arch={architecture}&distro=debian-12.15"
    if separator:
        purl += "&epoch=" + epoch
    identifier = {**row["PkgIdentifier"], "PURL": purl}
    row.update(PkgName=name, InstalledVersion=version, PkgID=f"{name}@{version}", PkgIdentifier=identifier)
    package = scan["Results"][0]["Packages"][0]
    package.update(ID=row["PkgID"], Name=name, Arch=architecture, Identifier=identifier)
    probe = evidence["probes"]["installed_packages"]
    lines = []
    for line in probe["stdout"].splitlines():
        fields = line.split("\t")
        if (fields[0].removesuffix(":" + fields[2]), fields[1], fields[2]) == old:
            fields[:3] = [name, version, architecture]
        lines.append("\t".join(fields))
    probe["stdout"] = "\n".join(lines) + "\n"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def mutations():
    return {
        "exact": lambda scan, evidence, row: None,
        "another-package": lambda scan, evidence, row: row.update(
            PkgName="not-approved", PkgIdentifier={**row["PkgIdentifier"], "PURL": row["PkgIdentifier"]["PURL"].replace(
                "/" + row["PkgName"] + "@", "/not-approved@")}),
        "another-version": lambda scan, evidence, row: row.update(
            InstalledVersion="0.0.0", PkgIdentifier={**row["PkgIdentifier"], "PURL": row["PkgIdentifier"]["PURL"].split("@")[0]
                                                   + "@0.0.0?" + row["PkgIdentifier"]["PURL"].split("?")[1]}),
        "another-architecture": lambda scan, evidence, row: row["PkgIdentifier"].update(
            PURL=row["PkgIdentifier"]["PURL"].replace("arch=amd64", "arch=arm64")),
        "consistent-package": lambda scan, evidence, row: replace_tuple(scan, evidence, row, name="not-approved"),
        "consistent-version": lambda scan, evidence, row: replace_tuple(scan, evidence, row, version="0.0.0"),
        "consistent-architecture": lambda scan, evidence, row: replace_tuple(scan, evidence, row, architecture="all"),
        "missing-identifier": lambda scan, evidence, row: row.pop("PkgIdentifier"),
        "null-identifier": lambda scan, evidence, row: row.update(PkgIdentifier=None),
        "missing-purl": lambda scan, evidence, row: row["PkgIdentifier"].pop("PURL"),
        "null-purl": lambda scan, evidence, row: row["PkgIdentifier"].update(PURL=None),
        "wrong-epoch": lambda scan, evidence, row: row["PkgIdentifier"].update(
            PURL=row["PkgIdentifier"]["PURL"].replace("epoch=1", "epoch=2")),
        "missing-epoch": lambda scan, evidence, row: row["PkgIdentifier"].update(
            PURL=row["PkgIdentifier"]["PURL"].replace("&epoch=1", "")),
        "missing-image": lambda scan, evidence, row: scan["Metadata"].pop("ImageID"),
        "another-image": lambda scan, evidence, row: scan["Metadata"].update(ImageID="sha256:" + "0" * 64),
        "another-source": lambda scan, evidence, row: evidence.update(source_sha="0" * 40),
        "missing-source": lambda scan, evidence, row: evidence.pop("source_sha"),
        "another-distro": lambda scan, evidence, row: scan["Metadata"]["OS"].update(Name="13"),
        "another-purl-distro": lambda scan, evidence, row: row["PkgIdentifier"].update(
            PURL=row["PkgIdentifier"]["PURL"].replace("debian-12.15", "debian-13")),
        "unknown-cve": lambda scan, evidence, row: row.update(VulnerabilityID="CVE-2026-9999999"),
        "expired": lambda scan, evidence, row: None,
    }


def run_probe(executable, directory, output):
    originals = ("image-vulnerabilities.json", "runtime-image-evidence.json", "source-sha.txt", "image-id.txt")
    if output.resolve() in {(directory / name).resolve() for name in originals}:
        raise ValueError("Probe output must not replace source evidence")
    raw = (directory / "image-vulnerabilities.json").read_bytes()
    diagnostics = (directory / "runtime-image-evidence.json").read_bytes()
    scan = json.loads(raw)
    evidence = json.loads(diagnostics)
    source_bytes = (directory / "source-sha.txt").read_bytes()
    image_bytes = (directory / "image-id.txt").read_bytes()
    source = source_bytes.decode().strip()
    image = image_bytes.decode().strip()
    now = datetime.now(timezone.utc)
    # Refuse to probe candidates from reports that fail the prerequisite bindings.
    preflight.preflight(scan, evidence, source, image, now)
    candidates = []
    for result in scan["Results"]:
        for row in result.get("Vulnerabilities", []):
            key = (row["VulnerabilityID"], *preflight.package_tuple(
                row["PkgName"], row["InstalledVersion"], row["PkgIdentifier"]["PURL"]))
            if key in preflight.CONDITIONAL:
                candidates.append((result, row))
    executable = Path(executable).resolve()
    binary = executable.read_bytes()
    receipts = []
    scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")
    with tempfile.TemporaryDirectory(dir=scratch) as temporary:
        root = Path(temporary)
        # Runner installations may share a HOME. Execute one private snapshot,
        # not a PATH target that a concurrent setup action can replace mid-matrix.
        pinned_binary = root / "trivy"
        pinned_binary.write_bytes(binary)
        pinned_binary.chmod(0o700)
        config = root / "config.yaml"
        config.write_text("{}\n")
        # Do not inherit TRIVY_* policy overrides, registry credentials or run JWTs.
        env = {"PATH": os.environ.get("PATH", os.defpath), "HOME": str(root)}
        prefix = [str(pinned_binary), "--quiet", "--config", str(config), "--cache-dir", str(root / "cache")]
        version = subprocess.run([*prefix, "--version"], env=env, capture_output=True, text=True, timeout=20, check=True)
        if "Version: 0.69.3" not in version.stdout.splitlines():
            raise ValueError("Selector probes require Trivy v0.69.3")
        for result, original in candidates:
            for label, mutate in mutations().items():
                if label in ("wrong-epoch", "missing-epoch") and "epoch=" not in original["PkgIdentifier"]["PURL"]:
                    continue
                trial = deepcopy(scan)
                trial_result = deepcopy(result)
                row = deepcopy(original)
                # Keep the original UID: convert's backward-compatibility repair
                # must not silently replace a deliberately missing PURL fixture.
                trial_result["Vulnerabilities"] = [row]
                trial_result["Packages"] = [deepcopy(p) for p in result["Packages"] if p["ID"] == original["PkgID"]]
                trial["Results"] = [trial_result]
                observed = deepcopy(evidence)
                mutate(trial, observed, row)
                ignore = {"vulnerabilities": [{
                    "id": original["VulnerabilityID"], "purls": [original["PkgIdentifier"]["PURL"]],
                    # Synthetic test dates only. The real decision expiry is not
                    # extended, and this file never becomes the repository policy.
                    "expired_at": "2000-01-01T00:00:00Z" if label == "expired" else "2099-01-01T00:00:00Z",
                    "statement": "Offline synthetic selector probe only; no exception activation.",
                }]}
                serialized = json.dumps(trial).encode()
                (root / "input.json").write_bytes(serialized)
                (root / "ignore.yaml").write_text(json.dumps(ignore))
                destination = root / "output.json"
                if destination.exists():
                    destination.unlink()
                command = [*prefix, "convert", "--severity", "HIGH,CRITICAL", "--format", "json",
                           "--exit-code", "1", "--ignorefile", str(root / "ignore.yaml"),
                           "--output", str(destination), str(root / "input.json")]
                converted = subprocess.run(command, env=env, capture_output=True, text=True, timeout=20)
                decoder_error = None
                output_hash = None
                if (label == "null-identifier" and converted.returncode == 2 and not destination.exists()
                        and "(*PkgIdentifier).UnmarshalJSON" in converted.stderr):
                    # v0.69.3 dereferences a nil auxiliary pointer while decoding
                    # JSON null. Record that exact malformed-fixture observation,
                    # not a filtered finding or a successful selector conversion.
                    decoder_error = "Trivy JSON decoder panic for null PkgIdentifier"
                    remaining = None
                else:
                    if converted.returncode not in (0, 1) or not destination.exists():
                        raise RuntimeError(f"Trivy conversion failed for {label}: {converted.stderr[:1000]}")
                    converted_bytes = destination.read_bytes()
                    output_hash = digest(converted_bytes)
                    filtered = json.loads(converted_bytes)
                    remaining = sum(len(item.get("Vulnerabilities", [])) for item in filtered["Results"])
                    if remaining not in (0, 1) or converted.returncode != remaining:
                        raise ValueError("Unexpected converted finding count/exit code")
                if label == "exact" and remaining != 0:
                    raise ValueError("Exact selector positive control failed")
                negative_selectors = ("unknown-cve", "expired", "another-package", "another-version",
                                      "another-architecture", "consistent-package", "consistent-version",
                                      "consistent-architecture", "wrong-epoch", "missing-epoch", "another-purl-distro")
                if label in negative_selectors and remaining != 1:
                    raise ValueError(f"Selector negative control failed: {label}")
                evaluated_at = preflight.EXPIRES if label == "expired" else now
                try:
                    bound = preflight.preflight(trial, observed, source, image, evaluated_at)
                    guard = bound["findings"][0]["status"]
                except ValueError as error:
                    guard = "rejected: " + str(error)
                if label in ("consistent-package", "consistent-version", "consistent-architecture", "unknown-cve"):
                    if guard != "not-conditionally-accepted":
                        raise ValueError("Consistent unaccepted tuple inherited a conditional decision")
                elif label == "expired":
                    if guard != "expired":
                        raise ValueError("Original preflight expiry boundary failed")
                elif label != "exact" and not guard.startswith("rejected:"):
                    raise ValueError("Malformed prerequisite binding was not rejected")
                receipts.append({"cve": original["VulnerabilityID"], "package": original["PkgName"],
                                 "version": original["InstalledVersion"], "purl": original["PkgIdentifier"]["PURL"],
                                 "case": label, "trivy_exit": converted.returncode, "remaining": remaining,
                                 "synthetic_ignore_expires_at": ignore["vulnerabilities"][0]["expired_at"],
                                 "preflight": guard, "preflight_evaluated_at": evaluated_at.isoformat(),
                                 "input_sha256": digest(serialized), "ignore_sha256": digest(json.dumps(ignore).encode()),
                                 "output_sha256": output_hash, "decoder_error": decoder_error,
                                 "stderr_sha256": digest(converted.stderr.encode())})
        report = {"schema_version": 1, "trivy_version": "0.69.3", "trivy_binary_sha256": digest(binary),
                  "source_sha": source, "image_id": image, "observed_at": now.isoformat(),
                  "decision": preflight.DECISION, "decision_expires_at": preflight.EXPIRES.isoformat(),
                  "conversion_flags": ["convert", "--severity", "HIGH,CRITICAL", "--format", "json", "--exit-code", "1"],
                  "raw_report_sha256": digest(raw), "diagnostics_sha256": digest(diagnostics),
                  "candidate_count": len(candidates), "case_count": len(receipts), "receipts": receipts,
                  "yaml_boundary_not_enforced": [r for r in receipts if r["case"] not in ("exact", "expired") and r["remaining"] == 0],
                  "repository_suppressed_count": 0,
                  "limitations": ["Actual Trivy convert uses the pinned scanner's result.Filter; no new image or DB scan.",
                                  "Only isolated synthetic report/ignore copies are filtered; real gates and ignores are unchanged.",
                                  "Synthetic YAML uses distant past/future controls, not renewal of the original 2026-10-08 decision.",
                                  "Preflight expiry is separately evaluated at the original boundary; Trivy's wall clock is not changed.",
                                  "Zero candidates means zero selector coverage, not proof that exceptions would be safe.",
                                  "Tuple preflight is not affected-code/source/payload/condition enforcement or activation approval.",
                                  "Convert cannot enforce source/image/distro/affected-code conditions through YAML PURLs alone."]}
    # Verify that even diagnostic fixture generation left the source evidence intact.
    originals = {"image-vulnerabilities.json": raw, "runtime-image-evidence.json": diagnostics,
                 "source-sha.txt": source_bytes, "image-id.txt": image_bytes}
    if any((directory / name).read_bytes() != data for name, data in originals.items()):
        raise ValueError("Source report/provenance was modified")
    output.write_text(json.dumps(report, indent=2) + "\n")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--trivy", default="trivy")
    args = parser.parse_args()
    executable = shutil.which(args.trivy)
    if not executable:
        raise SystemExit("Pinned Trivy executable unavailable; no selector proof claimed")
    output = args.directory / "trivy-selector-probes.json"
    report = run_probe(executable, args.directory, output)
    print(f"Observed {report['case_count']} cases across {report['candidate_count']} conditional tuples; "
          f"{len(report['yaml_boundary_not_enforced'])} cases require guards beyond YAML; repository suppression 0")


if __name__ == "__main__":
    main()
