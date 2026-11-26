"""Reconstruct the inputs to protected PRE start with local synthetic material."""

from dataclasses import replace
import os
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch
from zipfile import ZipFile

from test_predecessor_bundle import fixture, metadata, pack
from test_pre_worker_upload import receive_multipart

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_target import Target, TargetError, make_config
    from pre_worker_upload import assemble_worker_upload
    from predecessor_bundle import verify_predecessor_bundle
    from predecessor_intake import admit_predecessor
    from script_intake import decode_json


class PreOperatorInputsTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(self.enterContext(TemporaryDirectory()))
        self.worker = b"export default {};"
        members, self.receipt = fixture(scratch, 123, self.worker)
        self.archive = pack(members, self.receipt)
        self.pins = metadata(self.archive, 123, 457)
        self.root = scratch / "material"
        self.root.mkdir(mode=0o700)
        # Stand in for materialization with only the known synthetic ZIP members.
        with ZipFile(self.archive) as bundle:
            for name in ("build.json", *members):
                path = self.root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(bundle.read(name))
        self.target = Target("pre-fixture-operator", 789, 1_800_000_000, "0473/0465")
        self.now = 1_800_003_599

    def proposed_config(self, target, receipt):
        return make_config(target, now=self.now, receipt=receipt, material_root=self.root,
                           synthetic_service=target.service_name)

    def prepare(self, approved, propose):
        with (patch.dict(os.environ, clear=True),
              patch("socket.socket.connect", side_effect=AssertionError("unexpected network")),
              patch("subprocess.Popen", side_effect=AssertionError("unexpected subprocess"))):
            receipt = verify_predecessor_bundle(self.archive, **self.pins)
            config = admit_predecessor(
                approved, now=self.now, expected_producer_run_id=self.pins["run_id"],
                verified_receipt=receipt, material_root=self.root, plan=propose(receipt),
            )
            return config, assemble_worker_upload(approved, now=self.now, receipt=receipt,
                                                  material_root=self.root, config=config)

    def test_reconstructs_both_pinned_pre_start_inputs_without_credentials(self):
        for run, shape, suffix in ((789, "0473/0465", "0473-0465"), (790, "0475/0465", "0475-0465")):
            with self.subTest(shape=shape):
                approved = replace(self.target, run_id=run, shape=shape)
                config, upload = self.prepare(approved, lambda receipt: self.proposed_config(approved, receipt))
                name = f"two-bot-next-pre-{run}-1800003600"
                database = f"postgresql://pre_fixture@{name}-{suffix}.pre-fixture-operator.invalid/pre"
                self.assertEqual(config["name"], name)
                self.assertEqual(config["vars"]["DATABASE_URL"], database)
                self.assertEqual(upload.target, approved)
                self.assertEqual(upload.target.expires_at, 1_800_003_600)
                parts = receive_multipart(upload.content_type, upload.body)
                metadata_part = decode_json(parts[0][3])
                self.assertIn({"type": "plain_text", "name": "DATABASE_URL", "text": database},
                              metadata_part["bindings"])
                self.assertEqual(metadata_part["annotations"]["workers/message"], self.receipt["source_sha"])
                self.assertIn(("index.js", "index.js", "application/javascript+module", self.worker), parts)

    def test_refuses_wrong_environment_before_preparing_start_input(self):
        for environment, name in (("staging", "two-bot-next-staging"), ("production", "two-bot-next")):
            with self.subTest(environment=environment), self.assertRaisesRegex(
                TargetError, "^predecessor_plan_mismatch$",
            ):
                self.prepare(self.target, lambda receipt: self.proposed_config(self.target, receipt) | {
                    "env": {environment: {"name": name}},
                })

        wrong_scope = replace(self.target, scope="pre-fixture-other")
        for foreign in (wrong_scope, replace(self.target, shape="0475/0465")):
            with self.subTest(foreign=foreign), self.assertRaisesRegex(TargetError, "^predecessor_plan_mismatch$"):
                self.assertEqual(foreign.worker_name, self.target.worker_name)
                self.prepare(self.target, lambda receipt: self.proposed_config(foreign, receipt))
