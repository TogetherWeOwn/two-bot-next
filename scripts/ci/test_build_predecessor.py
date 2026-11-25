"""Check predecessor artifact packaging with synthetic build outputs."""

from hashlib import sha256
from io import BytesIO
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from build_predecessor import GateError, image_manifest, package_worker

LABELS = {"org.opencontainers.image.revision": "a" * 40, "com.togetherweown.build-id": "123-1"}


def oci_fixture(path, *, labels=LABELS, bad_config_digest=False):
    def add(archive, name, raw):
        member = tarfile.TarInfo(name)
        member.size = len(raw)
        archive.addfile(member, BytesIO(raw))

    def blob(archive, subtype, payload, invalid=False):
        raw = json.dumps(payload).encode()
        digest = ("0" * 64) if invalid else sha256(raw).hexdigest()
        add(archive, f"blobs/sha256/{digest}", raw)
        return {"digest": f"sha256:{digest}", "size": len(raw), "mediaType": f"application/vnd.oci.image.{subtype}.v1+json"}

    with tarfile.open(path, "w") as archive:
        config = blob(archive, "config", {"os": "linux", "architecture": "amd64", "config": {"Labels": labels}},
                      invalid=bad_config_digest)
        manifest = blob(archive, "manifest", {"config": config})
        add(archive, "index.json", json.dumps({"manifests": [manifest]}).encode())
    return manifest["digest"]


class BuildPredecessorTests(unittest.TestCase):
    def test_worker_artifact_contains_only_hashed_modules(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            build = root / "dry-run"
            for name, data in {"index.js": b"entry", "assets/module.wasm": b"wasm",
                               "README.md": b"generated note", "index.js.map": b"map", ".dev.vars": b"placeholder"}.items():
                path = build / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
            destination = root / "artifact"
            result = package_worker(build, destination)
            self.assertEqual(result, {"assets/module.wasm": sha256(b"wasm").hexdigest(),
                                      "index.js": sha256(b"entry").hexdigest()})
            self.assertEqual({path.relative_to(destination).as_posix(): path.read_bytes()
                              for path in destination.rglob("*") if path.is_file()},
                             {"assets/module.wasm": b"wasm", "index.js": b"entry"})

    def test_oci_receipt_requires_matching_blob_digest_and_build_labels(self):
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "image.tar"
            manifest = oci_fixture(path)
            self.assertEqual(image_manifest(path, LABELS), manifest)
            with self.assertRaisesRegex(GateError, "image_build_stamp_mismatch"):
                image_manifest(path, {**LABELS, "com.togetherweown.build-id": "124-1"})
            oci_fixture(path, bad_config_digest=True)
            with self.assertRaisesRegex(GateError, "image_metadata_digest_mismatch"):
                image_manifest(path, LABELS)
