"""Collect raw applicability observations from the scanned image, not a deployment.

The distroless runtime has no shell, package manager or coreutils, so nothing
executes inside the image: one never-started container is exported and its
filesystem is read here. Anything unparseable fails closed.
"""

import argparse
from datetime import datetime, timezone
import hashlib
import io
import json
from pathlib import Path
import re
import struct
import subprocess
import tarfile
import uuid

STATUS_DIR = "var/lib/dpkg/status.d/"
INSTALLED = "install ok installed"
BINARY = "home/nonroot/two-bot"
LIBRARY_DIRS = ("usr/lib/x86_64-linux-gnu", "lib/x86_64-linux-gnu", "usr/lib64", "lib64", "usr/lib", "lib")
TOOL_DIRS = ("usr/local/sbin", "usr/local/bin", "usr/sbin", "usr/bin", "sbin", "bin")
# Presence of any of these would contradict the shell-free runtime claim.
TOOLS = ("sh", "bash", "dash", "ash", "busybox", "apt", "apt-get", "dpkg", "perl",
         "python3", "openssl", "mount", "nsenter", "su", "sudo", "getcap", "setcap")
CAPABILITY = "SCHILY.xattr.security.capability"


def docker(*args, timeout=30):
    return subprocess.run(
        ["docker", *args], capture_output=True, timeout=timeout, check=False
    )


def observation(*args, timeout):
    """Run one Docker command; returns (record, raw stdout bytes)."""
    timed_out = False
    try:
        result = docker(*args, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        result = subprocess.CompletedProcess(args, None, error.stdout, error.stderr)
        timed_out = True
    except OSError as error:
        result = subprocess.CompletedProcess(args, None, b"", str(error).encode())

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

    raw = result.stdout if isinstance(result.stdout, bytes) else (result.stdout or "").encode()
    stderr = text(result.stderr, "stderr")
    if timed_out:
        stderr += f"\nDocker command exceeded {timeout} seconds"
    return {"returncode": result.returncode, "stderr": stderr, "timed_out": timed_out,
            "lossy_decoding": lossy_decoding, "stdout_bytes": len(raw),
            "stdout_sha256": hashlib.sha256(raw).hexdigest()}, raw


def export_filesystem(image_id):
    # Name before creating: a timed-out Docker client must not orphan a
    # container on the shared daemon. Created, never started; no network.
    name = "two-bot-inspect-" + uuid.uuid4().hex
    archive = b""
    try:
        create, _ = observation("create", "--name", name, "--network", "none",
                                "--no-healthcheck", image_id, timeout=30)
        export = None
        if create["returncode"] == 0 and not create["timed_out"]:
            export, archive = observation("export", name, timeout=120)
    finally:
        cleanup, raw = observation("rm", "--force", name, timeout=30)
    status = "removed" if cleanup["returncode"] == 0 else "failed"
    # Accept only the daemon's exact absence response for this UUID name, not
    # arbitrary cleanup errors. Preserve both the creation and removal results.
    if (cleanup["returncode"] == 1 and not raw
            and cleanup["stderr"].strip() == f"Error response from daemon: No such container: {name}"):
        status = "absent"
    container = {"name": name, "create": create, "export": export, "cleanup": {"status": status, **cleanup}}
    error = None
    if status == "failed":
        error = "Inspection container cleanup failed"
    elif export is None or export["returncode"] != 0 or export["timed_out"] or not archive:
        error = "Image filesystem export failed"
    return container, archive, error


def normalize(name):
    return name.removeprefix("./").strip("/")


def resolve(members, path, hops=40):
    """Resolve symlinks inside the exported image (absolute links stay inside)."""
    parts = [part for part in path.split("/") if part not in ("", ".")]
    done = []
    while parts:
        part = parts.pop(0)
        if part == "..":
            if done:
                done.pop()
            continue
        member = members.get("/".join([*done, part]))
        if member is not None and member.issym():
            hops -= 1
            if hops < 0:
                raise ValueError(f"Symlink loop resolving {path}")
            target = member.linkname
            if target.startswith("/"):
                done = []
            parts = [piece for piece in target.split("/") if piece not in ("", ".")] + parts
            continue
        done.append(part)
    resolved = "/".join(done)
    return resolved if resolved in members else None


def stanza(text, source):
    """Parse exactly one deb822 control paragraph; continuation lines are kept."""
    fields, current, ended = {}, None, False
    for line in text.split("\n"):
        if not line:
            ended = ended or bool(fields)
            continue
        if ended:
            raise ValueError(f"{source}: more than one paragraph")
        if line[0] in " \t":
            if current is None:
                raise ValueError(f"{source}: continuation before any field")
            fields[current] += "\n" + line
            continue
        key, separator, value = line.partition(":")
        if not separator or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*", key) or key in fields:
            raise ValueError(f"{source}: malformed or duplicate field")
        current = key
        fields[key] = value.strip()
    return fields


def installed_packages(members, contents):
    if "var/lib/dpkg/status" in members:
        raise ValueError("dpkg status database present; expected only the distroless status.d layout")
    rows = []
    seen = set()
    for name in sorted(members):
        if not name.startswith(STATUS_DIR) or name == STATUS_DIR.rstrip("/") or name.endswith(".md5sums"):
            continue
        member = members[name]
        if not member.isreg() or "/" in name[len(STATUS_DIR):]:
            raise ValueError(f"{name}: not a regular status.d file")
        try:
            text = contents[name].decode("utf-8")
        except UnicodeDecodeError as error:
            raise ValueError(f"{name}: not UTF-8") from error
        fields = stanza(text, name)
        package, version, arch = fields.get("Package"), fields.get("Version"), fields.get("Architecture")
        if not (package and version and arch) or any(re.search(r"\s", value) for value in (package, version, arch)):
            raise ValueError(f"{name}: missing Package, Version or Architecture")
        if name[len(STATUS_DIR):] not in (package, f"{package}:{arch}"):
            raise ValueError(f"{name}: file name differs from Package")
        # Distroless writes control paragraphs without dpkg's Status field;
        # Trivy treats those as installed. Any recorded Status must be exact.
        status = fields.get("Status")
        if status is not None and status != INSTALLED:
            raise ValueError(f"{name}: package is not installed ({status})")
        if (package, arch) in seen:
            raise ValueError(f"{name}: duplicate package")
        seen.add((package, arch))
        rows.append({"package": package, "version": version, "architecture": arch,
                     "status": status, "source_file": "/" + name})
    if not rows:
        raise ValueError("Empty status.d package inventory")
    return rows


def package_files(contents):
    owners = {}
    for name, data in contents.items():
        if name.startswith(STATUS_DIR) and name.endswith(".md5sums"):
            package = name[len(STATUS_DIR):-len(".md5sums")]
            for line in data.decode("utf-8", errors="replace").splitlines():
                if match := re.fullmatch(r"[0-9a-f]{32}  (.+)", line):
                    owners.setdefault(normalize(match[1]), set()).add(package)
    return owners


def elf_linkage(data):
    """Interpreter and DT_NEEDED sonames of a little-endian ELF64 executable."""
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        raise ValueError("Runtime binary is not a little-endian ELF64 file")
    phoff, = struct.unpack_from("<Q", data, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    loads, dynamic, interpreter = [], None, None
    for index in range(phnum):
        kind, _, offset, vaddr, _, filesz = struct.unpack_from("<IIQQQQ", data, phoff + index * phentsize)
        if kind == 1:
            loads.append((vaddr, offset, filesz))
        elif kind == 2:
            dynamic = (offset, filesz)
        elif kind == 3:
            interpreter = data[offset:offset + filesz].rstrip(b"\0").decode()
    if dynamic is None:
        return interpreter, []
    entries = [struct.unpack_from("<qQ", data, dynamic[0] + at) for at in range(0, dynamic[1], 16)]
    strtab = next((value for tag, value in entries if tag == 5), None)
    offset = next((off + strtab - vaddr for vaddr, off, size in loads
                   if strtab is not None and vaddr <= strtab < vaddr + size), None)
    if offset is None:
        raise ValueError("Runtime binary has no loadable dynamic string table")
    needed = []
    for tag, value in entries:
        if tag == 0:
            break
        if tag == 1:
            end = data.index(b"\0", offset + value)
            needed.append(data[offset + value:end].decode())
    return interpreter, needed


def inspect_filesystem(archive):
    members, contents = {}, {}
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as tar:
        for member in tar:
            name = normalize(member.name)
            if not name:
                continue
            if name in members:
                raise ValueError(f"{name}: duplicate archive entry")
            members[name] = member
            if member.isreg() and (name.startswith(STATUS_DIR) or name == BINARY):
                contents[name] = tar.extractfile(member).read()
    owners = package_files(contents)
    binary = members.get(BINARY)
    if binary is None or not binary.isreg():
        raise ValueError(f"/{BINARY} is not a regular file in the image")
    interpreter, needed = elf_linkage(contents[BINARY])

    def library(path):
        resolved = resolve(members, path)
        if resolved is None or not members[resolved].isreg():
            return None
        return {"path": "/" + path, "resolved": "/" + resolved,
                "packages": sorted(owners.get(resolved, ()))}

    linkage = {}
    for soname in needed:
        found = next((hit for hit in (library(f"{directory}/{soname}") for directory in LIBRARY_DIRS) if hit), None)
        if found is None:
            raise ValueError(f"DT_NEEDED {soname} does not resolve inside the image")
        linkage[soname] = found
    interp = library(interpreter.lstrip("/")) if interpreter else None
    if interpreter and interp is None:
        raise ValueError(f"ELF interpreter {interpreter} does not resolve inside the image")
    tools = sorted({"/" + path for path in (f"{directory}/{tool}" for directory in TOOL_DIRS for tool in TOOLS)
                    if resolve(members, path)})
    return {
        "installed_packages": installed_packages(members, contents),
        "setid_files": [{"path": "/" + name, "mode": oct(member.mode), "uid": member.uid, "gid": member.gid}
                        for name, member in sorted(members.items()) if member.isreg() and member.mode & 0o6000],
        "file_capabilities": ["/" + name for name, member in sorted(members.items()) if CAPABILITY in member.pax_headers],
        "tools_present": tools,
        "binary": {"path": "/" + BINARY, "size": binary.size, "mode": oct(binary.mode),
                   "uid": binary.uid, "gid": binary.gid,
                   "sha256": hashlib.sha256(contents[BINARY]).hexdigest(),
                   "interpreter": interp, "needed": linkage},
    }


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
        "schema_version": 2,
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
            "Read from an exported, never-started container; nothing executed inside the image.",
            "Package rows come from distroless status.d control files, which carry no dpkg Status field.",
            "Only the security.capability extended attribute is recorded; other xattrs and runtime privileges are not.",
            "File presence and ELF linkage do not establish application input reachability.",
        ],
    }
    destination = directory / "runtime-image-evidence.json"
    destination.write_text(json.dumps(report, indent=2) + "\n")
    report["container"], archive, error = export_filesystem(image_id)
    if error is None:
        try:
            report["filesystem"] = inspect_filesystem(archive)
        except (ValueError, tarfile.TarError, struct.error, UnicodeDecodeError) as failure:
            error = f"Image filesystem evidence is unparseable: {failure}"
    if error is not None:
        report["collection_error"] = error
    report["complete"] = error is None
    destination.write_text(json.dumps(report, indent=2) + "\n")
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
