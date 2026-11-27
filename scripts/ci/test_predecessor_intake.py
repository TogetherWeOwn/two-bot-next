"""Exercise the final handoff with two synthetic predecessor builds."""

from dataclasses import replace
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch
from zipfile import ZipFile

from test_pre_start_stop import PreFixture
from test_pre_worker_upload import database_fixture, receive_multipart
from test_predecessor_bundle import fixture, metadata, pack

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from build_predecessor import digest, image_manifest
    from pre_target import Target, TargetError, make_config
    from pre_worker_upload import assemble_worker_upload
    from predecessor_bundle import BUILD, IMAGE, verify_predecessor_bundle
    from predecessor_intake import admit_predecessor
    from script_intake import decode_json


class PredecessorIntakeTests(unittest.TestCase):
    def setUp(self):
        self.scratch = Path(self.enterContext(TemporaryDirectory()))
        self.target = Target("pre-fixture-intake", 789, 1_800_000_000, "0473/0465")
        self.first = self.candidate(123, b"export default { candidate: 1 };")
        self.second = self.candidate(124, b"export default { candidate: 2 };")

    def candidate(self, run_id, worker):
        root = self.scratch / str(run_id)
        members, build = fixture(self.scratch, run_id, worker)
        archive = pack(members, build)
        receipt = verify_predecessor_bundle(archive, **metadata(archive, run_id, run_id + 100))
        # Mirror the private materializer using only the verified inventory.
        with ZipFile(archive) as bundle:
            for name in (BUILD, IMAGE, *(f"worker/{name}" for name in receipt["worker_modules_sha256"])):
                member = root / name
                member.parent.mkdir(parents=True, exist_ok=True)
                member.write_bytes(bundle.read(name))
        return {"expected_producer_run_id": run_id, "verified_receipt": receipt, "material_root": root,
                "plan": self.plan(root, receipt)}

    def plan(self, root, receipt):
        return make_config(self.target, now=self.target.started_at, receipt=receipt, material_root=root,
                           synthetic_service=self.target.service_name)

    def admit(self, candidate, **overrides):
        options = {"now": self.target.started_at} | candidate | overrides
        return admit_predecessor(self.target, **options)

    def assemble(self, **database):
        candidate = self.first
        plan = self.admit(candidate)
        return assemble_worker_upload(self.target, now=self.target.started_at, receipt=candidate["verified_receipt"],
                                      material_root=candidate["material_root"], config=plan, **database)

    def start(self, runtime, **database):
        return runtime.controller(runtime.authorize).start(self.assemble(**database))

    def test_exact_archive_keeps_disposable_database_secret_and_refuses_discord_start(self):
        candidate = self.first
        root, receipt = candidate["material_root"], candidate["verified_receipt"]
        image = root / IMAGE
        original_files = {path: path.read_bytes() for path in root.rglob("*") if path.is_file()}
        runtime = PreFixture(self.target)
        database = database_fixture(self.target)

        upload = self.assemble(**database)
        self.assertEqual(upload.target, self.target)
        parts = receive_multipart(upload.content_type, upload.body)
        metadata = decode_json(parts[0][3])
        self.assertEqual([binding for binding in metadata["bindings"] if binding["name"] == "DATABASE_URL"],
                         [{"name": "DATABASE_URL", "type": "secret_text", "text": database["database_url"]}])
        self.assertNotIn(database["database_url"], repr(upload))
        self.assertEqual(metadata["annotations"], {"workers/message": receipt["source_sha"]})
        self.assertEqual({name: data for name, _, _, data in parts[1:]},
                         {name: original_files[root / "worker" / name] for name in receipt["worker_modules_sha256"]})
        self.assertEqual(digest(image), receipt["image_archive_sha256"])
        self.assertEqual(image_manifest(image, {"org.opencontainers.image.revision": receipt["source_sha"],
                                               "com.togetherweown.build-id": receipt["BOT_BUILD_ID"],
                                               "com.togetherweown.deploy-id": receipt["BOT_DEPLOY_ID"]}),
                         receipt["image_manifest_digest"])
        self.assertTrue(candidate["plan"]["containers"][0]["image"].endswith("@" + receipt["image_manifest_digest"]))
        self.assertEqual({path: path.read_bytes() for path in original_files}, original_files)

        with self.assertRaisesRegex(TargetError, "^pre_discord_fixture_overrides_unavailable$"):
            runtime.controller(runtime.authorize).start(upload)
        self.assertEqual(runtime.effects.mock_calls, [])

    def test_refuses_missing_shared_and_foreign_database_before_start(self):
        database = database_fixture(self.target)
        endpoint, url = database["database_endpoint"], database["database_url"]
        wrong_target = "wrong_pre_database_target"
        invalid_url = "invalid_pre_database_url"
        mismatch = "pre_database_endpoint_mismatch"
        for label, overrides, reason in (
            ("missing endpoint", {"database_endpoint": None}, wrong_target),
            ("missing URL", {"database_url": None}, invalid_url),
            ("shared endpoint", {"database_endpoint": replace(endpoint, disposable=False)}, wrong_target),
            ("shared service", {"database_endpoint": replace(endpoint, service="shared-staging")}, wrong_target),
            ("shared URL", {"database_url": url.replace(endpoint.host, "shared-staging.invalid")}, mismatch),
            ("stale run", {"database_endpoint": replace(endpoint, target=replace(self.target, run_id=1))}, wrong_target),
            ("wrong scope", {"database_endpoint": replace(endpoint, target=replace(
                self.target, scope="pre-fixture-other"))}, wrong_target),
            ("wrong shape", {"database_endpoint": replace(endpoint, target=replace(
                self.target, shape="0475/0465"))}, wrong_target),
            ("wrong time", {"database_endpoint": replace(endpoint, target=replace(self.target, started_at=1))},
             wrong_target),
            ("wrong database", {"database_url": url.replace("/pre?", "/other?")}, mismatch),
            ("wrong port", {"database_url": url.replace(":5432/", ":5433/")}, mismatch),
            ("wrong user", {"database_url": url.replace(endpoint.user, "other_user")}, mismatch),
            ("host override", {"database_url": url + "&%68ost=shared-staging.invalid"}, invalid_url),
            ("port omitted", {"database_url": url.replace(":5432/", "/")}, invalid_url),
            ("TLS omitted", {"database_url": url.split("?")[0]}, invalid_url),
            ("plan placeholder", {"database_url": self.first["plan"]["vars"]["DATABASE_URL"]}, invalid_url),
        ):
            with self.subTest(label=label):
                runtime = PreFixture(self.target)
                with self.assertRaisesRegex(TargetError, f"^{reason}$"):
                    self.start(runtime, **(database | overrides))
                self.assertEqual(runtime.effects.mock_calls, [])

    def test_admits_coherent_candidates_but_refuses_crossed_plans(self):
        self.assertNotEqual(self.first["plan"]["base_dir"], self.second["plan"]["base_dir"])
        self.assertNotEqual(self.first["plan"]["containers"], self.second["plan"]["containers"])
        for candidate, other in ((self.first, self.second), (self.second, self.first)):
            root, receipt = candidate["material_root"], candidate["verified_receipt"]
            self.assertEqual(self.admit(candidate), candidate["plan"])
            for plan in (other["plan"], self.plan(root, other["verified_receipt"]),
                         self.plan(other["material_root"], receipt)):
                with self.assertRaisesRegex(TargetError, "^predecessor_plan_mismatch$"):
                    self.admit(candidate, plan=plan)

    def test_refuses_unexpected_producer_and_crossed_manifest(self):
        with self.assertRaisesRegex(TargetError, "^unexpected_predecessor_build$"):
            self.admit(self.first, expected_producer_run_id=self.second["expected_producer_run_id"])
        with self.assertRaisesRegex(TargetError, "^unexpected_predecessor_build$"):
            self.admit(self.first, verified_receipt=self.first["verified_receipt"] | {"source_sha": "a" * 40})
        with self.assertRaisesRegex(TargetError, "^predecessor_handoff_manifest_mismatch$"):
            self.admit(self.first, material_root=self.second["material_root"])

    def test_refuses_foreign_payloads_behind_expected_paths(self):
        root, other_root = self.first["material_root"], self.second["material_root"]
        for name, payload in (("worker/index.js", "worker"), ("image.oci.tar", "image")):
            path = root / name
            original = path.read_bytes()
            with self.subTest(payload=payload):
                path.write_bytes((other_root / name).read_bytes())
                with self.assertRaisesRegex(TargetError, f"^predecessor_handoff_{payload}_mismatch$"):
                    self.admit(self.first)
                path.write_bytes(original)

    def test_refuses_ambiguous_manifest_and_expired_admission(self):
        with self.assertRaisesRegex(TargetError, "^pre_admission_expired$"):
            self.admit(self.first, now=self.target.expires_at)
        (self.first["material_root"] / "build.json").write_bytes(b'{"source_sha": "a", "source_sha": "b"}')
        with self.assertRaisesRegex(TargetError, "^invalid_predecessor_handoff_manifest$"):
            self.admit(self.first)
