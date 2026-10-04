"""Offline producer contract tests; no Actions dispatch, database or approval."""
from copy import deepcopy
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import zipfile

from staging_migrate_claim import build_claim, projection_hash, Refused, unique_object
from test_workflows import load_workflows, staging_migrate_errors

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "crates/cutover/tests/fixtures"
PLAN = FIXTURES / "staging-migrate-plan.json"
CLAIM = FIXTURES / "staging-migrate-apply-claim.json"
ENV = {
    "GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next",
    "GITHUB_EVENT_NAME": "workflow_dispatch",
    "GITHUB_REF": "refs/heads/main",
    "GITHUB_WORKFLOW_REF": "TogetherWeOwn/two-bot-next/.github/workflows/staging-migrate.yml@refs/heads/main",
    "GITHUB_SHA": "b" * 40,
    "GITHUB_RUN_ID": "37110947191",
    "GITHUB_RUN_ATTEMPT": "2",
    "MODE": "apply",
    "PLAN_RUN_ID": "37110947190",
    "PLAN_MANIFEST_SHA256": "e4a3136346ca546df4f1f4f654322c278ff7df1d5902ec5685cd43036b41beea",
    "SOURCE_SHA": "a" * 40,
    "STAGING_HOST": "staging.invalid",
    "STAGING_DATABASE": "two_bot",
    "RECOVERY_REF": "recovery-review#decision",
    "ACL_REF": "acl-review#decision",
    "EXPECTED_PENDING": "1,9007199254740993",
}


class ClaimTests(unittest.TestCase):
    def setUp(self):
        self.plan = json.loads(PLAN.read_text())

    def test_actual_publisher_matches_shared_consumer_vector(self):
        expected = json.loads(CLAIM.read_text())
        self.assertEqual(build_claim(self.plan, ENV), expected)
        # The exact output script, not a handcrafted claim-shaped mock.
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as directory:
            output = Path(directory) / "claim.json"
            result = subprocess.run(["python3", str(ROOT / "scripts/ci/staging_migrate_claim.py"),
                                     "--manifest", str(PLAN), "--output", str(output)],
                                    env={**os.environ, **ENV}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(output.read_bytes(), CLAIM.read_bytes())
        self.assertEqual(expected["plan_run_id"], "37110947190")
        self.assertEqual(expected["expected_pending"], ["1", "9007199254740993"])
        self.assertNotEqual(expected["workflow_head_sha"], expected["source_sha"])

    def test_digest_is_exact_rust_projection_not_document_or_zip_bytes(self):
        digest = projection_hash(self.plan)
        self.assertEqual(digest, ENV["PLAN_MANIFEST_SHA256"])
        self.assertNotEqual(digest, hashlib.sha256(PLAN.read_bytes()).hexdigest())
        reformatted = json.loads(json.dumps(self.plan, separators=(",", ":")))
        self.assertEqual(projection_hash(reformatted), digest)
        for mutation in (lambda p: p["source_migrations"][0].update(description="changed"),
                         lambda p: p["source_migrations"][0].update(sha384="0" * 96),
                         lambda p: p.update(source_sha="c" * 40),
                         lambda p: p.update(pending_before=[1])):
            plan = deepcopy(self.plan)
            mutation(plan)
            self.assertNotEqual(projection_hash(plan), digest)
        # A source migration that is not pending still enters the projection.
        plan = deepcopy(self.plan)
        plan["pending_before"] = [1]
        digest = projection_hash(plan)
        plan["source_migrations"][1]["description"] = "changed already applied"
        self.assertNotEqual(projection_hash(plan), digest)

    def test_wrong_producer_context_or_binding_refuses(self):
        for key, bad in (("GITHUB_REPOSITORY", "other/repo"), ("GITHUB_EVENT_NAME", "pull_request"),
                         ("GITHUB_REF", "refs/heads/topic"), ("GITHUB_WORKFLOW_REF", "other/workflow"),
                         ("GITHUB_SHA", "short"), ("MODE", "plan"), ("GITHUB_RUN_ID", ""),
                         ("GITHUB_RUN_ATTEMPT", "0"), ("PLAN_RUN_ID", "37110947190x"),
                         ("PLAN_RUN_ID", "0"), ("PLAN_RUN_ID", "1" * 21),
                         ("PLAN_RUN_ID", ENV["GITHUB_RUN_ID"]),
                         ("PLAN_MANIFEST_SHA256", "c" * 64), ("SOURCE_SHA", "c" * 40),
                         ("STAGING_HOST", "elsewhere.invalid"), ("STAGING_DATABASE", "other_db"),
                         ("RECOVERY_REF", "other-review"), ("ACL_REF", "other-review"),
                         ("EXPECTED_PENDING", "1"), ("EXPECTED_PENDING", "01,9007199254740993"),
                         ("EXPECTED_PENDING", "1,9007199254740992")):
            with self.subTest(key=key, bad=bad):
                with self.assertRaises(Refused):
                    build_claim(self.plan, {**ENV, key: bad})

    def test_malformed_plan_or_secret_bearing_metadata_refuses(self):
        for key, bad in (("mode", "apply"), ("runner_version", 2), ("runner_version", True),
                         ("role", "two_bot_migrator"), ("plan_manifest_sha256", None),
                         ("applied_count", 1), ("plan_provenance_verified", True),
                         ("plan_run_id", "37110947190"), ("ledger_after", [{"changed": True}]),
                         ("pending_before", [True]), ("pending_before", [1, 1]),
                         ("pending_before", [1, 2**63]), ("source_migrations", None),
                         ("target", {"host": "staging.invalid", "database": "two_bot", "url": "secret"})):
            with self.subTest(key=key):
                plan = {**self.plan, key: bad}
                with self.assertRaises(Refused):
                    build_claim(plan, ENV)
        for host in ("postgres://user:sentinel@staging.invalid", "production.invalid", "a/b"):
            plan = deepcopy(self.plan)
            plan["target"]["host"] = host
            with self.assertRaises(Refused):
                build_claim(plan, {**ENV, "STAGING_HOST": host})
        for value in ("https://sentinel.invalid", "a@b", "two words"):
            plan = {**self.plan, "acl_plan_ref": value}
            with self.assertRaises(Refused):
                build_claim(plan, {**ENV, "ACL_REF": value})
        with self.assertRaises(Refused):
            json.loads('{"mode":"plan","mode":"apply"}', object_pairs_hook=unique_object)

    def test_unavailable_invalid_or_mismatched_manifest_leaves_no_claim(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as directory:
            directory = Path(directory)
            for body in (None, b"not json", b'{}', b'{"mode":"plan","mode":"apply"}',
                         json.dumps({**self.plan, "target": {"host": "sentinel", "database": "x"}}).encode(),
                         b"x" * (1024 * 1024 + 1)):
                with self.subTest(body_length=None if body is None else len(body)):
                    manifest, output = directory / "plan.json", directory / "claim.json"
                    if body is not None:
                        manifest.write_bytes(body)
                    else:
                        manifest.unlink(missing_ok=True)
                    result = subprocess.run(["python3", str(ROOT / "scripts/ci/staging_migrate_claim.py"),
                                             "--manifest", str(manifest), "--output", str(output)],
                                            env={**os.environ, **ENV}, capture_output=True, text=True)
                    self.assertEqual(result.returncode, 2)
                    self.assertFalse(output.exists())
                    self.assertNotIn("sentinel", result.stderr)

    def test_empty_pending_and_full_length_decimal_identity(self):
        plan = deepcopy(self.plan)
        plan["pending_before"] = []
        plan["plan_manifest_sha256"] = projection_hash(plan)
        env = {**ENV, "EXPECTED_PENDING": "", "PLAN_MANIFEST_SHA256": plan["plan_manifest_sha256"],
               "PLAN_RUN_ID": "18446744073709551615"}
        claim = build_claim(plan, env)
        self.assertEqual(claim["expected_pending"], [])
        self.assertEqual(claim["plan_run_id"], env["PLAN_RUN_ID"])

    def test_producer_files_roundtrip_as_stored_zip_entries(self):
        for path in (PLAN, CLAIM):
            archive = io.BytesIO()
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_STORED) as writer:
                writer.writestr(path.name, path.read_bytes())
            with zipfile.ZipFile(io.BytesIO(archive.getvalue())) as reader:
                self.assertEqual(reader.getinfo(path.name).compress_type, zipfile.ZIP_STORED)
                self.assertEqual(reader.read(path.name), path.read_bytes())
            # ZIP digest is not the request's embedded projection digest either.
            self.assertNotEqual(hashlib.sha256(archive.getvalue()).hexdigest(), ENV["PLAN_MANIFEST_SHA256"])


class ClaimWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.workflow = load_workflows()["staging-migrate.yml"]

    def test_claim_is_successful_prerequisite_not_waiting_apply_step(self):
        self.assertEqual(staging_migrate_errors(self.workflow), [])
        self.assertNotIn("environment", self.workflow["jobs"]["claim"])
        self.assertEqual(self.workflow["jobs"]["apply"]["needs"], ["plan", "claim"])

    def test_transport_softening_or_late_claim_fails_policy(self):
        changes = [
            lambda w: w["jobs"].pop("claim"),
            lambda w: w["jobs"]["apply"].update(needs="plan"),
            lambda w: w["jobs"]["claim"].update(environment="staging-migrate-apply"),
            lambda w: w["jobs"]["claim"].update(needs="apply"),
            lambda w: w["jobs"]["claim"].update(permissions={"contents": "read", "actions": "write"}),
            lambda w: w["jobs"]["claim"].update(name="other"),
            lambda w: w["jobs"]["claim"].update({"if": "always()"}),
            lambda w: w["jobs"]["claim"]["steps"][0]["with"].update(ref="${{ inputs.source_sha }}"),
            lambda w: w["jobs"]["claim"]["steps"][1]["with"].update({"run-id": "arbitrary"}),
            lambda w: w["jobs"]["claim"]["steps"][2]["env"].update(SECRET="${{ secrets.DATABASE_URL }}"),
            lambda w: w["jobs"]["claim"]["steps"][2]["env"].update(PLAN_RUN_ID="0"),
            lambda w: w["jobs"]["claim"]["steps"][2].update({"continue-on-error": "true"}),
            lambda w: w["jobs"]["claim"]["steps"][3].update({"if": "always()"}),
            lambda w: w["jobs"]["claim"]["steps"][3]["with"].update({"compression-level": "6"}),
            lambda w: w["jobs"]["plan"]["steps"][-1]["with"].pop("compression-level"),
        ]
        for index, change in enumerate(changes):
            with self.subTest(change=index):
                workflow = deepcopy(self.workflow)
                change(workflow)
                self.assertTrue(staging_migrate_errors(workflow))


if __name__ == "__main__":
    unittest.main()
