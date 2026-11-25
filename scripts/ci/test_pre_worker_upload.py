"""Replay synthetic predecessor modules through a mock multipart receiver."""

from copy import deepcopy
from dataclasses import replace
from email import policy
from email.parser import BytesParser
from hashlib import sha256
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from pre_target import Target, TargetError, make_config
from pre_worker_upload import assemble_worker_upload
from script_intake import decode_json


MODULES = {
    "index.js": ("application/javascript+module",
                 b'export class TwoBotContainer {}\r\nexport default {fetch() {return new Response("fixture")}};'),
    "lib/helper.mjs": ("application/javascript+module", b"export const sample = 1;\r\n"),
    "lib/runtime.wasm": ("application/wasm", bytes.fromhex("0061736d01000000")),
    "lib/message.txt": ("text/plain", b"fixture\r\n"),
    "lib/data.bin": ("application/octet-stream", bytes([0, 128, 255, 10, 13])),
}


def receive_multipart(content_type, body):
    headers = f"Content-Type: {content_type}\r\n\r\n".encode("ascii")
    message = BytesParser(policy=policy.default).parsebytes(headers + body)
    if not message.is_multipart() or message.defects:
        raise ValueError("invalid mock upload")
    return [(part.get_param("name", header="Content-Disposition"), part.get_filename(),
             part.get_content_type(), part.get_payload(decode=True)) for part in message.iter_parts()]


class PreWorkerUploadTests(unittest.TestCase):
    def setUp(self):
        scratch = TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.worker = self.root / "worker"
        (self.worker / "lib").mkdir(parents=True)
        (self.root / "image.oci.tar").write_bytes(b"synthetic image archive")
        self.target = Target("pre-fixture-upload", 195, 1700000000, "0473/0465")
        self.revision = "a" * 40
        self.receipt = {
            "source_sha": self.revision,
            "worker_message": self.revision,
            "BOT_BUILD_REVISION": self.revision,
            "image_manifest_digest": "sha256:" + "b" * 64,
            "worker_modules_sha256": {name: sha256(data).hexdigest() for name, (_, data) in MODULES.items()},
        }
        for name, (_, data) in MODULES.items():
            (self.worker / name).write_bytes(data)
        self.config = self.make_config(self.target)

    def make_config(self, target):
        return make_config(target, now=target.started_at, receipt=self.receipt,
                           material_root=self.root, synthetic_service=target.service_name)

    def assemble(self, **overrides):
        return assemble_worker_upload(**{
            "target": self.target, "now": self.target.started_at, "receipt": self.receipt,
            "material_root": self.root, "config": self.config, **overrides,
        })

    def test_multipart_preserves_module_names_types_bytes_and_version_metadata(self):
        upload = self.assemble()
        self.assertEqual(upload.target, self.target)
        parts = receive_multipart(upload.content_type, upload.body)
        self.assertEqual(parts[0][:3], ("metadata", None, "text/plain"))
        metadata = decode_json(parts[0][3])
        self.assertEqual(metadata, {
            "main_module": "index.js",
            "compatibility_date": self.config["compatibility_date"],
            "compatibility_flags": self.config["compatibility_flags"],
            "bindings": [
                {"name": "BOT_PORT", "type": "plain_text", "text": "8080"},
                {"name": "CONTAINER_RELEASE", "type": "plain_text", "text": self.revision},
                {"name": "DATABASE_URL", "type": "plain_text", "text": self.config["vars"]["DATABASE_URL"]},
                {"name": "TWO_BOT", "type": "durable_object_namespace", "class_name": "TwoBotContainer"},
                {"name": "CF_VERSION_METADATA", "type": "version_metadata"},
            ],
            "exports": {"TwoBotContainer": {"type": "durable-object", "storage": "sqlite"}},
            "containers": [{"name": self.target.container_name, "class_name": "TwoBotContainer"}],
            "code_update_strategy": {"mode": "deferred", "max_delay": 300},
            "annotations": {"workers/message": self.revision},
        })
        self.assertCountEqual(parts[1:], [(name, name, mime, data) for name, (mime, data) in MODULES.items()])
        self.assertEqual(self.assemble(), upload)
        for name, (_, data) in MODULES.items():
            self.assertEqual((self.worker / name).read_bytes(), data)

    def test_refuses_wrong_entrypoint_and_missing_entrypoint_hash(self):
        self.config["main"] = str(self.worker / "lib/helper.mjs")
        with self.assertRaisesRegex(TargetError, "^wrong_pre_worker_entrypoint$"):
            self.assemble()
        self.config = self.make_config(self.target)
        del self.receipt["worker_modules_sha256"]["index.js"]
        with self.assertRaisesRegex(TargetError, "^invalid_pre_worker_manifest$"):
            self.assemble()

    def test_refuses_missing_extra_and_substituted_modules(self):
        member = self.worker / "lib/data.bin"
        data = member.read_bytes()
        member.unlink()
        with self.assertRaisesRegex(TargetError, "^pre_worker_inventory_mismatch$"):
            self.assemble()
        member.write_bytes(data)
        for name in ("extra.js", ".env", "README.md", "index.js.map"):
            with self.subTest(name=name):
                extra = self.worker / name
                extra.write_bytes(b"unexpected")
                with self.assertRaisesRegex(TargetError, "^pre_worker_inventory_mismatch$"):
                    self.assemble()
                extra.unlink()
        member.write_bytes(b"!" * len(data))
        with self.assertRaisesRegex(TargetError, "^pre_worker_digest_mismatch$"):
            self.assemble()

    def test_refuses_linked_and_out_of_bounds_modules(self):
        member = self.worker / "lib/data.bin"
        moved = self.root / "substitute.bin"
        member.rename(moved)
        member.symlink_to(moved)
        with self.assertRaisesRegex(TargetError, "^pre_worker_inventory_mismatch$"):
            self.assemble()
        member.unlink()
        member.write_bytes(b"original")
        self.receipt["worker_modules_sha256"]["../substitute.bin"] = sha256(b"original").hexdigest()
        with self.assertRaisesRegex(TargetError, "^invalid_pre_worker_manifest$"):
            self.assemble()
        del self.receipt["worker_modules_sha256"]["../substitute.bin"]
        with member.open("wb") as handle:
            handle.truncate(8 * 1024 * 1024 + 1)
        with self.assertRaisesRegex(TargetError, "^pre_worker_size_limit$"):
            self.assemble()

    def test_refuses_shared_target_and_public_ingress_config(self):
        for change in (
            {"name": "two-bot-next-staging"}, {"workers_dev": True}, {"preview_urls": True},
            {"route": "shared.example.invalid/*"}, {"routes": [{"pattern": "public.example.invalid/*"}]},
            {"env": {"staging": {"name": "two-bot-next-staging"}}},
        ):
            with self.subTest(keys=change.keys()), self.assertRaisesRegex(TargetError, "^wrong_pre_worker_config$"):
                self.assemble(config={**self.config, **change})
        stale_config = deepcopy(self.config)
        stale_config["containers"][0]["name"] = "two-bot-next-staging"
        with self.assertRaisesRegex(TargetError, "^wrong_pre_worker_config$"):
            self.assemble(config=stale_config)
        for other in (replace(self.target, scope="pre-fixture-other"), replace(self.target, shape="0475/0465")):
            with self.subTest(other=other):
                self.assertEqual(other.worker_name, self.target.worker_name)
                with self.assertRaisesRegex(TargetError, "^wrong_pre_worker_config$"):
                    self.assemble(target=other)
                self.assertEqual(self.assemble(target=other, config=self.make_config(other)).target, other)
        with self.assertRaisesRegex(TargetError, "^pre_admission_expired$"):
            self.assemble(now=self.target.expires_at)

    def test_refuses_shared_public_or_incomplete_ingress_readback(self):
        upload = self.assemble()
        offline = dict(observed_target=self.target, complete=True, workers_dev=False,
                       preview_urls=False, routes=(), custom_domains=())
        upload.require_unrouted(**offline)
        for change in (
            {"complete": False}, {"workers_dev": True}, {"preview_urls": True}, {"preview_urls": None},
            {"routes": ("shared.example.invalid/*",)}, {"custom_domains": ("public.example.invalid",)},
        ):
            with self.subTest(keys=change.keys()), self.assertRaisesRegex(TargetError, "^pre_ingress_unconfirmed$"):
                upload.require_unrouted(**{**offline, **change})
        other = replace(self.target, scope="pre-fixture-other")
        with self.assertRaisesRegex(TargetError, "^wrong_pre_ingress_target$"):
            upload.require_unrouted(**{**offline, "observed_target": other})


if __name__ == "__main__":
    unittest.main()
