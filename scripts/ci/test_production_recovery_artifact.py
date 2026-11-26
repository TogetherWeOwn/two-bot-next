"""Synthetic provenance checks for the protected inverse artifact."""

from copy import deepcopy
from dataclasses import asdict
from hashlib import sha256
from io import BytesIO
import json
from pathlib import Path
import sys
import unittest
from zipfile import ZipFile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import production_recovery_artifact as artifacts
from production_recovery_takeover import RecoveryPin
from staging_rollout import GateError


def artifact_archive(run, artifact, artifact_id, document, archive_name=None):
    archive = BytesIO()
    with ZipFile(archive, "w") as bundle:
        bundle.writestr(archive_name or f"{artifact}.json",
                        document if isinstance(document, str) else json.dumps(document))
    raw = archive.getvalue()
    digest = sha256(raw).hexdigest()
    entry = {"id": artifact_id, "name": f"{artifact}-{run['id']}-1", "digest": f"sha256:{digest}",
             "expired": False, "size_in_bytes": len(raw)}
    meta = {**entry, "workflow_run": {"id": run["id"], "repository_id": artifacts.REPO_ID,
                                     "head_repository_id": artifacts.REPO_ID, "head_branch": "main",
                                     "head_sha": run["head_sha"]}}
    path = f"actions/artifacts/{artifact_id}"
    return digest, entry, {path: meta, f"{path}/zip": raw}


def release_run_fixture():
    repo = {"full_name": artifacts.REPO, "id": artifacts.REPO_ID}
    run = {"id": 123, "head_sha": "d" * 40, "head_branch": "main", "run_attempt": 1,
           "status": "in_progress", "event": "workflow_dispatch", "path": artifacts.WORKFLOW,
           "repository": repo, "head_repository": repo, "actor": {"id": 42}, "triggering_actor": {"id": 42}}
    env = {"GITHUB_RUN_ID": "123", "GITHUB_SHA": "d" * 40, "GITHUB_WORKFLOW_SHA": "d" * 40,
           "GITHUB_REPOSITORY": artifacts.REPO, "GITHUB_REPOSITORY_ID": str(artifacts.REPO_ID),
           "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_ACTOR_ID": "42",
           "GITHUB_REF": "refs/heads/main", "GITHUB_REF_PROTECTED": "true",
           "GITHUB_WORKFLOW_REF": f"{artifacts.REPO}/{artifacts.WORKFLOW}@refs/heads/main"}
    return env, run


def producer(run, name, steps):
    return {"name": name, "run_id": run["id"], "run_attempt": run["run_attempt"], "head_sha": run["head_sha"],
            "status": "completed", "conclusion": "success", "steps": [
                {"name": step, "status": "completed", "conclusion": "success"} for step in steps
            ]}


class RecoveryArtifactTests(unittest.TestCase):
    def setUp(self):
        self.pin = RecoveryPin("123", 1, "a" * 40, "b" * 40, "1" * 8 + "-1111" * 3 + "-" + "1" * 12,
                               "sha256:" + "1" * 64, "2" * 8 + "-2222" * 3 + "-" + "2" * 12, "2" * 32,
                               "3" * 64, "4" * 64, "5" * 64, "6" * 64, 21)
        self.result = {"state": "restored", **asdict(self.pin)}
        self.approval = {"plan_run_id": "121"}

    def artifact_fixture(self, document=None, archive_name=None):
        document = document if document is not None else {"operation": "undo-0480", "approval": self.approval,
                                                          "result": self.result}
        env, run = release_run_fixture()
        root = "actions/runs/123"
        digest, entry, archive_routes = artifact_archive(run, artifacts.ARTIFACT, 987, document, archive_name)
        return env, digest, {root: run,
                             root + "/attempts/1/jobs?per_page=30": {"total_count": 1, "jobs": [
                                 producer(run, artifacts.JOB, (artifacts.WRITE_STEP, artifacts.UPLOAD_STEP))]},
                             root + "/artifacts?per_page=30": {"total_count": 1, "artifacts": [entry]},
                             **archive_routes}

    def authenticated(self, env, digest, routes):
        return artifacts.authenticated_inverse(self.pin, self.approval, "987", digest, env,
                                                lambda path, **_: routes[path])

    def test_exact_inverse_artifact_and_producer_required(self):
        env, digest, routes = self.artifact_fixture()
        self.assertEqual(self.authenticated(env, digest, routes), self.result)
        jobs = "actions/runs/123/attempts/1/jobs?per_page=30"
        mutations = (
            lambda r: r[jobs]["jobs"][0].update(run_attempt=2),
            lambda r: r[jobs]["jobs"][0].update(name="A different job"),
            lambda r: r[jobs]["jobs"][0]["steps"][0].update(conclusion="failure"),
            lambda r: r["actions/artifacts/987"]["workflow_run"].update(head_repository_id=1),
            lambda r: r["actions/artifacts/987"].update(expired=True),
            lambda r: r.update({"actions/artifacts/987/zip": r["actions/artifacts/987/zip"][::-1]}),
        )
        for mutate in mutations:
            changed = deepcopy(routes)
            mutate(changed)
            with self.subTest(mutate=mutate), self.assertRaises(GateError):
                self.authenticated(env, digest, changed)
        with self.assertRaises(GateError):
            self.authenticated({**env, "GITHUB_RUN_ATTEMPT": "2"}, digest, routes)

    def test_inverse_archive_rejects_wrong_members_and_payload(self):
        for spec in ({"archive_name": "other.json"},
                     {"document": {"operation": "undo-0480", "approval": self.approval,
                                   "result": {**self.result, "fence_epoch": 22}}}):
            with self.subTest(spec=spec), self.assertRaises(GateError):
                self.authenticated(*self.artifact_fixture(**spec))

    def test_inverse_accepts_the_upload_digest_but_refuses_other_prefixes(self):
        env, digest, routes = self.artifact_fixture()
        self.assertEqual(self.authenticated(env, f"sha256:{digest}", routes), self.result)
        for prefix in ("sha512:", "sha256:sha256:"):
            with self.subTest(prefix=prefix), self.assertRaisesRegex(GateError, "artifact_pin_invalid"):
                self.authenticated(env, prefix + digest, routes)

    def test_inverse_archive_rejects_duplicate_key_in_valid_envelope(self):
        valid = {"operation": "undo-0480", "approval": self.approval, "result": self.result}
        duplicate = '{"operation":"other",' + json.dumps(valid)[1:]
        self.assertEqual(json.loads(duplicate), valid)
        self.assertEqual(self.authenticated(*self.artifact_fixture(document=valid)), self.result)
        with self.assertRaisesRegex(GateError, "^invalid_json$"):
            self.authenticated(*self.artifact_fixture(document=duplicate))
