"""Synthetic GitHub run and ZIP evidence for the dormant inventory reader."""

from contextlib import redirect_stderr, redirect_stdout
from hashlib import sha256
from io import BytesIO, StringIO
import json
from pathlib import Path
import sys
import unittest
from zipfile import ZIP_DEFLATED, ZipFile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import staging_automod_inventory_reader as reader
from rollback_proof import ProofError, REPO
from staging_rollout import GateError

SHA = "a" * 40
TIP = "b" * 40
RUN = "1234"
RUN_ROUTE = f"actions/runs/{RUN}"
LIST_ROUTE = f"{RUN_ROUTE}/artifacts?per_page=2"
ZIP_ROUTE = "actions/artifacts/42/zip"
COMPARE_ROUTE = f"compare/{SHA}...{TIP}?per_page=1&page=1"
DEPLOYMENT = "11111111-1111-1111-1111-111111111111"
VERSION = "22222222-2222-2222-2222-222222222222"
APP = "33333333-3333-3333-3333-333333333333"
ROLLOUT = "44444444-4444-4444-4444-444444444444"
FILENAME = "staging-recovery-status.json"


def receipt():
    return {
        "schema_version": 1, "workflow_sha": SHA, "workflow_run_id": int(RUN),
        "observed_at": "2026-11-25T12:00:00.123456+00:00",
        "verification": "provider_only", "runtime_database": "plan_not_supplied", "plan": None,
        "worker": {"deployment_id": DEPLOYMENT, "version_id": VERSION, "source_sha": "c" * 40,
                   "traffic_percent": 100, "runtime_policy": "bounded"},
        "container": {"application_id": APP, "namespace_id": "e" * 32, "rollout_id": ROLLOUT,
                      "rollout_status": "completed", "image_digest": "sha256:" + "f" * 64},
        "ownership": {"phase": "active", "epoch": 7, "running": False},
    }


class InventoryReaderTests(unittest.TestCase):
    def setUp(self):
        self.requests = []
        repository = {"id": reader.REPOSITORY_ID, "full_name": REPO}
        self.run_record = {"id": int(RUN), "path": reader.WORKFLOW, "head_branch": "main",
                           "head_sha": SHA, "event": "workflow_dispatch", "status": "completed",
                           "conclusion": "success", "run_attempt": 1, "repository": dict(repository),
                           "head_repository": dict(repository)}
        self.artifact = {"id": 42, "name": f"staging-recovery-status-{RUN}-1", "expired": False}
        self.listing = {"total_count": 1, "artifacts": [self.artifact]}
        self.routes = {RUN_ROUTE: self.run_record, "branches/main": {"protected": True, "commit": {"sha": SHA}},
                       LIST_ROUTE: self.listing}
        self.archive(json.dumps(receipt()))

    def api(self, path, *, binary=False):
        self.requests.append((path, binary))
        self.assertEqual(binary, path == ZIP_ROUTE)
        return self.routes[path]

    def archive(self, content, *, member=FILENAME, extra=False):
        buffer = BytesIO()
        with ZipFile(buffer, "w", compression=ZIP_DEFLATED) as archive:
            archive.writestr(member, content)
            if extra:
                archive.writestr("extra.json", "{}")
        raw = self.routes[ZIP_ROUTE] = buffer.getvalue()
        self.artifact.update(size_in_bytes=len(raw), digest="sha256:" + sha256(raw).hexdigest())

    def read(self):
        return reader.read_inventory(SHA, RUN, self.api)

    def test_reads_exact_inventory_from_successful_protected_main(self):
        expected = receipt()
        for key in ("verification", "runtime_database", "plan"):
            del expected[key]
        del expected["ownership"]["running"]
        self.assertEqual(self.read(), expected)
        self.assertEqual(self.requests, [(RUN_ROUTE, False), ("branches/main", False),
                                         (LIST_ROUTE, False), (ZIP_ROUTE, True)])
        stdout, stderr = StringIO(), StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            self.assertEqual(reader.main([SHA, RUN], self.api), 0)
        self.assertEqual(json.loads(stdout.getvalue()), expected)
        self.assertEqual(stderr.getvalue(), "")

    def test_refuses_wrong_run_producer_attempt_or_completion(self):
        for change in (
            {"id": int(RUN) + 1}, {"head_sha": TIP}, {"run_attempt": 2}, {"run_attempt": True},
            {"path": ".github/workflows/staging-automod-preview.yml"}, {"path": reader.WORKFLOW + "@other"},
            {"status": "in_progress"}, {"conclusion": "failure"}, {"event": "push"}, {"head_branch": "topic"},
            {"repository": {"id": 21, "full_name": REPO}}, {"head_repository": {"id": 21, "full_name": REPO}},
        ):
            with self.subTest(change=change):
                self.setUp()
                self.run_record.update(change)
                with self.assertRaises((GateError, ProofError)):
                    self.read()
                self.assertEqual(self.requests, [(RUN_ROUTE, False)])

    def test_refuses_bad_source_pins_before_an_api_call(self):
        for sha, run in (("main", RUN), (SHA, "0"), (SHA, "01234"), (SHA, str(2**53))):
            with self.subTest(sha=sha, run=run):
                with self.assertRaises(GateError):
                    reader.read_inventory(sha, run, self.api)
        self.assertEqual(self.requests, [])

    def test_requires_protected_main_and_proved_ancestry_if_it_advanced(self):
        branch = self.routes["branches/main"]
        branch["protected"] = False
        with self.assertRaises(GateError):
            self.read()
        branch.update(protected=True, commit={"sha": TIP})
        comparison = self.routes[COMPARE_ROUTE] = {
            "status": "ahead", "behind_by": 0, "ahead_by": 1,
            "merge_base_commit": {"sha": SHA}, "base_commit": {"sha": SHA},
        }
        self.assertEqual(self.read()["workflow_sha"], SHA)
        for change in ({"merge_base_commit": {"sha": "c" * 40}}, {"status": "diverged"}, {"behind_by": 1}):
            with self.subTest(change=change):
                self.routes[COMPARE_ROUTE] = {**comparison, **change}
                with self.assertRaises(GateError):
                    self.read()

    def test_refuses_missing_ambiguous_or_unpinned_artifact(self):
        for change in (
            {"total_count": 0, "artifacts": []}, {"total_count": 2}, {"total_count": True},
            {"name": f"staging-recovery-status-{RUN}-2"}, {"expired": True}, {"digest": None},
            {"size_in_bytes": reader.MAX_PLAN + 1},
        ):
            with self.subTest(change=change):
                self.setUp()
                target = self.listing if "total_count" in change else self.artifact
                target.update(change)
                with self.assertRaises(GateError):
                    self.read()
                self.assertNotIn((ZIP_ROUTE, True), self.requests)
        self.setUp()
        self.listing.update(total_count=2, artifacts=[self.artifact, {**self.artifact, "id": 43}])
        with self.assertRaises(GateError):
            self.read()
        self.assertNotIn((ZIP_ROUTE, True), self.requests)

    def test_refuses_bad_archive_partial_receipt_or_mismatched_payload(self):
        complete = json.dumps(receipt())
        cases = [
            (complete, {"member": "different.json"}), (complete, {"extra": True}), ("{", {}),
            ('{"workflow_sha":"wrong",' + complete[1:], {}),
            (json.dumps({**receipt(), "workflow_sha": TIP}), {}),
            (json.dumps({**receipt(), "workflow_run_id": int(RUN) + 1}), {}),
            (json.dumps({**receipt(), "ownership": {"phase": "active", "epoch": 7}}), {}),
            (json.dumps({**receipt(), "verification": "full_tuple"}), {}),
            (json.dumps({key: value for key, value in receipt().items() if key != "container"}), {}),
        ]
        for content, options in cases:
            with self.subTest(content=content, options=options):
                self.archive(content, **options)
                with self.assertRaises(GateError):
                    self.read()
        self.archive(complete)
        self.artifact["size_in_bytes"] += 1
        with self.assertRaises(GateError):
            self.read()
        self.archive(complete)
        self.artifact["digest"] = "sha256:" + "0" * 64
        self.assert_cli_refusal()

    def test_cli_refuses_oversized_projections_and_malformed_evidence(self):
        value = receipt()
        value["worker"]["version_id"] = "x" * reader.MAX_OUTPUT_BYTES
        self.archive(json.dumps(value))
        self.assert_cli_refusal()
        del self.routes[LIST_ROUTE]
        self.assert_cli_refusal()

    def assert_cli_refusal(self):
        stdout, stderr = StringIO(), StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            self.assertEqual(reader.main([SHA, RUN], self.api), 1)
        self.assertEqual(stdout.getvalue(), "")
        self.assertEqual(stderr.getvalue(), "Staging inventory evidence unconfirmed\n")
