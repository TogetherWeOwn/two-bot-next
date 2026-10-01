"""Collect raw applicability observations from the scanned image, not a deployment."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import re
import subprocess
import uuid

PACKAGES = (
    "zlib1g libtinfo6 ncurses-base ncurses-bin perl-base libsystemd0 libudev1 "
    "gzip bsdutils libblkid1 libmount1 libsmartcols1 libuuid1 mount util-linux "
    "util-linux-extra libacl1 libpcre2-8-0 libssl3 openssl"
)
BINARY = "/home/two-bot/two-bot"
UTIL_LINUX_IDENTITY = (
    "dpkg-query -W -f='${binary:Package}\t${Version}\t${Architecture}\t${source:Package}\t${source:Version}\n' "
    "bsdutils libblkid1 libmount1 libsmartcols1 libuuid1 mount util-linux util-linux-extra || exit $?; "
    "for path in /usr/bin/mount /usr/bin/umount /usr/bin/nsenter /usr/lib/x86_64-linux-gnu/libmount.so.1; do "
    "resolved=$(readlink -e \"$path\") || exit $?; "
    "printf 'resolved\\t%s\\t%s\\n' \"$path\" \"$resolved\"; "
    "sha256sum \"$resolved\" || exit $?; done"
)
PROBES = {
    "installed_packages": "dpkg-query -W -f='${binary:Package}\t${Version}\t${Architecture}\t${Essential}\t${Status}\n'",
    "affected_package_files": "dpkg-query -L " + PACKAGES,
    "package_dependencies": "dpkg-query -W -f='${binary:Package}\t${Version}\t${Architecture}\t${Essential}\t${Status}\t${Depends}\t${Pre-Depends}\n'",
    "affected_utility_paths": "for utility in infocmp gzip perl openssl mount umount nsenter systemd-homed; do printf '%s: ' \"$utility\"; command -v \"$utility\" || true; done",
    "affected_files": "find / -xdev -type f \\( -iname '*minizip*' -o -name 'homed*' -o -name 'systemd-homed*' -o -path '*/Archive/Tar*' -o -path '*/IO/Compress*' -o -path '*/File/GlobMapper*' -o -name 'Storable*' \\) -print",
    "perl_build_width": "perl -V:version -V:archname -V:ptrsize -V:ivsize -V:longsize",
    "perl_archive_tar": "perl -MArchive::Tar -e 'print \"$INC{q(Archive/Tar.pm)}\\n\"'",
    "perl_glob_mapper": "perl -MFile::GlobMapper -e 'print \"$INC{q(File/GlobMapper.pm)}\\n\"'",
    "perl_io_compress": "perl -MIO::Compress::Gzip -e 'print \"$INC{q(IO/Compress/Gzip.pm)}\\n\"'",
    "perl_storable": "perl -MStorable -e 'print \"$INC{q(Storable.pm)}\\n\"'",
    "elf_needed": "readelf -d " + BINARY,
    "runtime_linkage": "ldd " + BINARY,
    "openssl_build": "openssl version -a",
    "suid_sgid_files": "find / -xdev -type f -perm /6000 -printf '%m %u %g %p\\n'",
    "file_capabilities": "if command -v getcap > /dev/null 2>&1; then getcap -r /; else printf 'getcap unavailable; cannot establish capability absence\\n' >&2; exit 127; fi",
    "mount_configuration": UTIL_LINUX_IDENTITY + "; for path in /etc/fstab /etc/mtab /etc/mount.conf; do printf '\\n%s\\n' \"$path\"; if [ -e \"$path\" ]; then ls -ld \"$path\"; cat \"$path\"; else printf 'absent\\n'; fi; done; mount --version; nsenter --version; nsenter --help",
}


def docker(*args, timeout=30):
    return subprocess.run(
        ["docker", *args], capture_output=True, timeout=timeout, check=False
    )


def observation(*args, timeout):
    timed_out = False
    try:
        result = docker(*args, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        result = subprocess.CompletedProcess(args, None, error.stdout, error.stderr)
        timed_out = True
    except OSError as error:
        result = subprocess.CompletedProcess(args, None, "", str(error))

    lossy_decoding = []

    def text(value, stream):
        if isinstance(value, bytes):
            try:
                return value.decode("utf-8")
            except UnicodeDecodeError:
                # A valid UTF-8 U+FFFD is not evidence of decoding loss.
                lossy_decoding.append(stream)
                return value.decode("utf-8", errors="replace")
        return value or ""

    stdout = text(result.stdout, "stdout")
    stderr = text(result.stderr, "stderr")
    if timed_out:
        stderr += f"\nDocker command exceeded {timeout} seconds"
    return {"returncode": result.returncode, "stdout": stdout,
            "stderr": stderr, "timed_out": timed_out, "lossy_decoding": lossy_decoding}


def probe(image_id, command):
    # Name before starting: a timed-out Docker client must not orphan a container
    # on the shared daemon. No mounts, host environment, ports or network access.
    name = "two-bot-inspect-" + uuid.uuid4().hex
    try:
        result = observation(
            "run", "--name", name, "--read-only", "--no-healthcheck",
            "--network", "none", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--user", "0:0",
            "--pids-limit", "32", "--memory", "128m", "--memory-swap", "128m",
            "--cpus", "0.5", "--entrypoint", "/bin/sh", image_id, "-c", command,
            timeout=20,
        )
    finally:
        cleanup = observation("rm", "--force", name, timeout=30)
    status = "removed" if cleanup["returncode"] == 0 else "failed"
    # Accept only the daemon's exact absence response for this UUID name, not
    # arbitrary cleanup errors. Preserve both the startup and removal results.
    if cleanup["returncode"] == 1 and cleanup["stderr"].strip() == f"Error response from daemon: No such container: {name}" and not cleanup["stdout"]:
        status = "absent"
    result["container_name"] = name
    result["cleanup"] = {"status": status, **cleanup}
    return result


def collect(image, directory):
    source_sha = (directory / "source-sha.txt").read_text().strip()
    image_id = (directory / "image-id.txt").read_text().strip()
    if not re.fullmatch(r"[0-9a-f]{40}", source_sha) or not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
        raise ValueError("Invalid recorded provenance")
    if not image or image.startswith("-"):
        raise ValueError("Invalid image reference")
    inspection = docker("image", "inspect", image)
    if inspection.returncode != 0:
        raise RuntimeError(f"Image inspection failed: {inspection.stderr}")
    metadata = json.loads(inspection.stdout)[0]
    if metadata["Id"] != image_id:
        raise ValueError("Image identity differs from the scanned image provenance")
    config = metadata["Config"]
    report = {
        "schema_version": 1,
        "complete": False,
        "observed_at": datetime.now(timezone.utc).isoformat(),
        "source_sha": source_sha,
        "image_id": image_id,
        "architecture": metadata["Architecture"],
        "os": metadata["Os"],
        "configured_user": config.get("User"),
        "entrypoint": config.get("Entrypoint"),
        "cmd": config.get("Cmd"),
        "limitations": [
            "Raw observations only: no vulnerability acceptance or suppression.",
            "Missing tools, nonzero exits and timeouts do not establish non-applicability.",
            "Inspection runs as root with all capabilities dropped on a read-only, network-disabled filesystem; this is not production privilege evidence.",
            "Mount/kernel output describes an isolated CI container, not production kernel, fstab or namespace restrictions.",
            "Package files, ldd and module presence do not establish application input reachability.",
        ],
        "probes": {},
    }
    destination = directory / "runtime-image-evidence.json"
    destination.write_text(json.dumps(report, indent=2) + "\n")
    for key, command in PROBES.items():
        result = probe(image_id, command)
        report["probes"][key] = {"command": command, **result}
        if result["cleanup"]["status"] == "failed":
            report["collection_error"] = "Inspection container cleanup failed; further probes stopped"
        report["complete"] = len(report["probes"]) == len(PROBES) and "collection_error" not in report
        destination.write_text(json.dumps(report, indent=2) + "\n")
        if "collection_error" in report:
            break
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image")
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    report = collect(args.image, args.directory)
    if not report["complete"]:
        raise SystemExit(report["collection_error"])


if __name__ == "__main__":
    main()
