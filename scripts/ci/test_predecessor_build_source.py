"""Offline evidence for the exact predecessor source and both stamp inputs."""

import hashlib
from io import BytesIO
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "scripts/predecessor_build_source.py"
BASE = "d314f4c63ecbea263aa953bc8a649a444a97da7c"
sys.path.insert(0, str(TOOL.parent))
from predecessor_build_source import verify_archive


class PredecessorBuildSourceTests(unittest.TestCase):
    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=ROOT, timeout=30)

    def export(self, revision, destination, *, env=None):
        return subprocess.run([sys.executable, TOOL, "--source", revision, destination],
                              capture_output=True, text=True, timeout=30, env=env)

    def test_exports_reviewed_inputs_and_single_stamp(self):
        revision = self.git("rev-parse", "HEAD").decode().strip()
        revision_tree = self.git("rev-parse", "HEAD^{tree}").decode().strip()
        # A caller's ambient Git routing cannot substitute another checkout.
        with tempfile.TemporaryDirectory() as scratch:
            dest = Path(scratch) / "predecessor"
            result = self.export(revision, dest, env={**os.environ, "GIT_DIR": scratch})
            self.assertEqual(result.returncode, 0, result.stderr)
            receipt = json.loads((dest / "provenance.json").read_text())
            with (dest / "source.tar").open("rb") as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            self.assertEqual(receipt, {
                "source_revision": revision,
                "source_tree": revision_tree,
                "historical_base_revision": BASE,
                "worker_delta_blob": "1b5dc7baa84758545d6193d0c5f2f3255d064d77",
                "input_tree": "217e63471c39fc6177e13cefa87693c2a3c1572a",
                "archive_sha256": digest,
                "worker_message": revision,
                "container_build_args": {"BOT_BUILD_REVISION": revision},
            })
            with tarfile.open(dest / "source.tar", "r:") as source:
                self.assertNotIn(".git", source.getnames())
                for path in ("Dockerfile", "Cargo.lock", "migrations.lock", "rust-toolchain.toml",
                             "crates/bot/src/server.rs", "wrangler/package-lock.json", "wrangler/wrangler.toml"):
                    with source.extractfile(path) as archived:
                        self.assertEqual(archived.read(), self.git("show", f"{BASE}:{path}"), path)
                with source.extractfile("wrangler/package-lock.json") as archived:
                    lock = json.load(archived)
                self.assertEqual(lock["packages"]["node_modules/wrangler"]["version"], "4.152.0")
                with source.extractfile("wrangler/wrangler.toml") as archived:
                    config = tomllib.loads(archived.read().decode())
                self.assertEqual(config["env"]["staging"]["version_metadata"]["binding"], "CF_VERSION_METADATA")
                with source.extractfile("wrangler/src/index.ts") as archived:
                    self.assertIn(b'"x-two-bot-exact-takeover": "1"', archived.read())
                with source.extractfile("Dockerfile") as archived:
                    dockerfile = archived.read()
                revision_arg = b"ARG BOT_BUILD_REVISION=unknown"
                self.assertLess(dockerfile.index(revision_arg), dockerfile.index(b"RUN cargo build"))
                self.assertIn(b"org.opencontainers.image.revision=$BOT_BUILD_REVISION", dockerfile)
                with source.extractfile("crates/bot/src/server.rs") as archived:
                    self.assertIn(b'build_revision: option_env!("BOT_BUILD_REVISION")', archived.read())

    def test_refuses_changed_or_missing_archive_bytes(self):
        tree = self.git("rev-parse", f"{BASE}^{{tree}}").decode().strip()
        original = self.git("show", f"{BASE}:Dockerfile")
        for content, reason in ((original + b"\n", "archive bytes or file mode differ"),
                                (original, "archive is missing files")):
            with self.subTest(reason=reason), tempfile.TemporaryDirectory() as scratch:
                archive = Path(scratch) / "source.tar"
                with tarfile.open(archive, "w:") as output:
                    info = tarfile.TarInfo("Dockerfile")
                    info.size = len(content)
                    output.addfile(info, BytesIO(content))
                with self.assertRaisesRegex(ValueError, reason):
                    verify_archive(archive, tree)

    def test_refuses_mismatched_revision_and_existing_destination(self):
        revision = self.git("rev-parse", "HEAD").decode().strip()
        with tempfile.TemporaryDirectory() as scratch:
            dest = Path(scratch) / "predecessor"
            result = self.export(BASE, dest)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("source SHA must be the exact checked-out commit", result.stderr)
            self.assertFalse(dest.exists())
            dest.mkdir()
            sentinel = dest / "source.tar"
            sentinel.write_text("prior output")
            result = self.export(revision, dest)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("destination must be a new directory", result.stderr)
            self.assertEqual(sentinel.read_text(), "prior output")


if __name__ == "__main__":
    unittest.main()
