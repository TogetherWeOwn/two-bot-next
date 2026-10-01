"""Measure logical uncompressed layers, independent of Docker's storage driver.

Docker's containerd image-inspect Size includes packed content plus snapshot
usage. Stream image-save instead, binding config and layer digests to the
inspected immutable image. Count layer tar bytes, including whiteouts/deleted
files, not the merged filesystem or compressed registry transfer size.
"""

import gzip
import hashlib
import json
from pathlib import PurePosixPath
import re
import subprocess
import tarfile
import threading

CHUNK = 64 * 1024
METADATA_MAX_BYTES = 1024 * 1024
ARCHIVE_MAX_BYTES = 1024 * 1024 * 1024
SHA256 = re.compile(r"sha256:[0-9a-f]{64}\Z")


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class ArchiveReader:
    def __init__(self, source):
        self.source = source
        self.size = 0
        self.tail = b""

    def read(self, size):
        data = self.source.read(size)
        self.size += len(data)
        require(self.size <= ARCHIVE_MAX_BYTES, "image archive exceeds measurement bound")
        self.tail = (self.tail + data)[-1024:]
        return data


def archive_image_bytes(source, metadata):
    """Validate a single-image Docker save archive without extracting any files."""
    image_id = metadata["Id"]
    require(isinstance(image_id, str) and SHA256.fullmatch(image_id), "invalid image ID")
    expected_config = image_id.removeprefix("sha256:")
    reader = ArchiveReader(source)
    objects = {}
    documents = {}
    total = 0
    with tarfile.open(fileobj=reader, mode="r|") as archive:
        for member in archive:
            path = member.name
            require(len(objects) < 4096, "too many image archive objects")
            require(not PurePosixPath(path).is_absolute()
                    and ".." not in PurePosixPath(path).parts, "unsafe image archive path")
            if member.isdir():
                continue
            require(member.isfile() and not member.issparse(), "unsupported image archive object")
            require(path not in objects, "duplicate image archive object")
            capture = (path == "manifest.json" or path.endswith(".json")
                       or path == f"blobs/sha256/{expected_config}")
            require(not capture or member.size <= METADATA_MAX_BYTES, "oversized image metadata")
            stream = archive.extractfile(member)
            raw_digest = hashlib.sha256()
            digest = hashlib.sha256()
            size = 0
            document = bytearray()
            # ExFileObject is bounded to this tar member. Peek without seeking
            # the outer archive backwards, then include every encoded byte in
            # its descriptor digest, including compressed headers/trailers.
            prefix = stream.read(2)

            class ObjectReader:
                def read(self, count=-1):
                    nonlocal prefix
                    require(count >= 0, "unbounded image object read")
                    data = prefix[:count]
                    prefix = prefix[len(data):]
                    data += stream.read(count - len(data))
                    raw_digest.update(data)
                    return data

            encoded = ObjectReader()
            decoded = gzip.GzipFile(fileobj=encoded) if prefix == b"\x1f\x8b" else encoded
            while chunk := decoded.read(CHUNK):
                size += len(chunk)
                total += len(chunk)
                require(total <= ARCHIVE_MAX_BYTES, "uncompressed image exceeds measurement bound")
                digest.update(chunk)
                if capture:
                    require(size <= METADATA_MAX_BYTES, "oversized image metadata")
                    document.extend(chunk)
            if isinstance(decoded, gzip.GzipFile):
                decoded.close()
            objects[path] = (size, digest.hexdigest(), raw_digest.hexdigest())
            if path.startswith("blobs/sha256/"):
                require(path == "blobs/sha256/" + raw_digest.hexdigest(),
                        "image object descriptor digest mismatch")
            if capture:
                documents[path] = json.loads(document)
        content_end = archive.offset
    # Drain the subprocess pipe and require two terminator blocks after the
    # members, not merely zero bytes at the end of the final layer payload.
    while reader.read(CHUNK):
        pass
    require(reader.size >= content_end + 1024 and reader.size % 512 == 0
            and reader.tail == bytes(1024), "truncated image archive")
    manifest = documents.get("manifest.json")
    require(isinstance(manifest, list) and len(manifest) == 1, "ambiguous image archive manifest")
    entry = manifest[0]
    require(isinstance(entry, dict), "invalid image archive manifest")
    config_path = entry.get("Config")
    require(isinstance(config_path, str) and config_path in documents, "missing image config")
    require(objects[config_path][2] == expected_config, "image config digest mismatch")
    config = documents[config_path]
    require(isinstance(config, dict), "invalid image config")
    require(config.get("os") == metadata["Os"]
            and config.get("architecture") == metadata["Architecture"], "image platform mismatch")
    rootfs = config.get("rootfs", {})
    require(isinstance(rootfs, dict) and rootfs.get("type") == "layers", "invalid image rootfs")
    diff_ids = rootfs.get("diff_ids")
    layers = entry.get("Layers")
    require(isinstance(layers, list) and isinstance(diff_ids, list)
            and len(layers) == len(diff_ids) and layers, "invalid image layer manifest")
    require(metadata.get("RootFS") == {"Type": "layers", "Layers": diff_ids},
            "inspected image layer mismatch")
    unique = set()
    size = 0
    for path, diff_id in zip(layers, diff_ids):
        require(isinstance(path, str) and path in objects, "missing image layer")
        require(isinstance(diff_id, str) and SHA256.fullmatch(diff_id), "invalid layer diff ID")
        layer_size, digest, _ = objects[path]
        require("sha256:" + digest == diff_id, "image layer digest mismatch")
        if diff_id not in unique:
            size += layer_size
            unique.add(diff_id)
    return size


def image_bytes(metadata):
    """Bound a local export; never buffer/store the complete runtime image."""
    process = subprocess.Popen(
        ["docker", "image", "save", metadata["Id"]],
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    )
    timer = threading.Timer(120, process.kill)
    timer.daemon = True
    timer.start()
    try:
        size = archive_image_bytes(process.stdout, metadata)
        require(process.wait(timeout=5) == 0, "Docker image export failed")
        return size
    except (OSError, EOFError, tarfile.TarError, ValueError) as error:
        raise RuntimeError("invalid Docker image archive") from error
    finally:
        timer.cancel()
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)
        process.stdout.close()
