"""Run staging admission and prepare offline with the retained rollback pin."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from test_workflows import ROOT, load_workflows

with patch.object(sys, "path", [str(ROOT / "scripts"), *sys.path]):
    from rollback_proof import proof_dispatch

RETAINED_PIN = "710deeef896c182a9a57a8e1758553e4a03d12f6"

# Replace urllib transport for both versions of staging_rollout.py. The log
# names the executable, real workflow identity and fixture reads it attempted.
PROVIDER_FIXTURE = '''
import io
import json
import os
from pathlib import Path
import runpy
import sys
import urllib.request

def open_fixture(self, request, *args, **kwargs):
    path = request.selector.split("/containers/", 1)[1]
    with open(os.environ["PROVIDER_READS"], "a") as log:
        print(Path(sys.argv[0]).resolve(), os.environ["GITHUB_SHA"], path, file=log)
    result = {
        "applications": [{"name": "two-bot-next-twobotcontainer-staging", "id": "app_fixture",
                          "durable_objects": {"namespace_id": "fixture_namespace"}}],
        "applications/app_fixture/rollouts?limit=100": [],
    }[path]
    response = io.BytesIO(json.dumps({"success": True, "result": result}).encode())
    response.status, response.headers = 200, {}
    return response

urllib.request.OpenerDirector.open = open_fixture
sys.argv = sys.argv[1:]
script = Path(sys.argv[0]).resolve()
sys.path.insert(0, str(script.parent))
runpy.run_path(str(script), run_name="__main__")
'''


class StagingRollbackPrepareTests(unittest.TestCase):
    def test_real_wrapper_pin_routes_through_workflow_prepare(self):
        job = load_workflows()["deploy-staging.yml"]["jobs"]["deploy"]
        admit = next(step["run"] for step in job["steps"]
                     if step.get("name") == "Deploy SHA must be exact and reachable from main")
        prepare = next(step["run"] for step in job["steps"] if step.get("id") == "prepare")
        prepare = 'python3() { "$FIXTURE_PYTHON" "$FIXTURE_ENTRY" "$@"; }\n' + prepare
        dispatch = proof_dispatch(RETAINED_PIN)
        with tempfile.TemporaryDirectory() as directory:
            temp = Path(directory)
            source = temp / "source"
            tooling = source / "tooling"
            git_env = {"PATH": os.environ["PATH"], "GIT_CONFIG_NOSYSTEM": "1",
                       "GIT_CONFIG_GLOBAL": str(temp / "gitconfig")}

            def git(cwd, *args):
                return subprocess.run(["git", *args], cwd=cwd, env=git_env, check=True,
                                      capture_output=True, text=True, timeout=30).stdout.strip()

            workflow_sha = git(ROOT, "rev-parse", "HEAD")
            self.assertNotEqual(workflow_sha, RETAINED_PIN)
            origin = temp / "origin.git"
            git(temp, "clone", "--bare", "--shared", str(ROOT), str(origin))
            git(origin, "update-ref", "refs/heads/main", workflow_sha)
            git(temp, "clone", "--shared", "--no-checkout", str(origin), str(source))
            git(source, "checkout", "--detach", RETAINED_PIN)
            git(source, "clone", "--shared", "--no-checkout", str(origin), str(tooling))
            git(tooling, "checkout", "--detach", workflow_sha)
            fixture_entry = temp / "provider_fixture.py"
            fixture_entry.write_text(PROVIDER_FIXTURE)

            def run(shell, env):
                workdir = source / job["defaults"]["run"]["working-directory"]
                return subprocess.run(["bash", "-e", "-c", shell], cwd=workdir, env=env,
                                      capture_output=True, text=True, timeout=30)

            child_inputs = dispatch["inputs"]
            rehearsal_flag = child_inputs["inject_error_rate_spike"] and child_inputs["recover_prior_sha"]
            self.assertIs(rehearsal_flag, True)
            child_env = dict(git_env, SOURCE_SHA=child_inputs["sha"], WORKFLOW_REF=f"refs/heads/{dispatch['ref']}",
                             RECOVER_PRIOR_SHA=str(child_inputs["recover_prior_sha"]).lower(),
                             GITHUB_SHA=workflow_sha, GITHUB_WORKSPACE=str(source), GITHUB_ENV=str(temp / "env"))
            accepted = run(admit, child_env)
            self.assertEqual(accepted.returncode, 0, accepted.stderr)
            key, sha = (temp / "env").read_text().strip().split("=", 1)
            self.assertEqual((key, sha), ("DEPLOY_SHA", RETAINED_PIN))
            for rejected_env, error in (
                ({"WORKFLOW_REF": "refs/heads/untrusted"}, "This job must run from main"),
                ({"GITHUB_SHA": sha}, "Staging dispatch superseded by main"),
                ({"SOURCE_SHA": workflow_sha}, "Checkout did not land"),
            ):
                result = run(admit, child_env | rejected_env | {"GITHUB_ENV": str(temp / "rejected")})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(error, result.stderr)
                self.assertFalse((temp / "rejected").exists())

            def exercise(label, serving_sha, route, *, error=None, **overrides):
                output = temp / label
                reads = temp / f"{label}.reads"
                env = dict(git_env, GITHUB_SHA=workflow_sha, GITHUB_RUN_ID="123", GITHUB_RUN_ATTEMPT="1",
                           STAGING_SOURCE_SHA=serving_sha, DEPLOY_ID=f"2026.11.25.1+{serving_sha[:7]}",
                           CLOUDFLARE_API_TOKEN="offline-fixture", CLOUDFLARE_ACCOUNT_ID="a" * 32,
                           STAGING_URL="https://two-bot-next-staging.offline-fixture.workers.dev",
                           ROLLOUT_DIR=str(output), WRANGLER_OUTPUT_FILE_PATH=str(output / "wrangler.ndjson"),
                           GITHUB_OUTPUT=str(temp / f"{label}.outputs"), PROVIDER_READS=str(reads),
                           FIXTURE_ENTRY=str(fixture_entry), FIXTURE_PYTHON=sys.executable,
                           PYTHONDONTWRITEBYTECODE="1", STAGING_GATE_ROOT="..",
                           ROLLBACK_REHEARSAL_PREPARE="false", INJECT_GATEWAY_FAILURE="false",
                           INJECT_VOICE_STALL="false", INJECT_UNPAIRED_DISCONNECT="false")
                result = run(prepare, env | overrides)
                report = result.stdout + result.stderr
                if error is not None:
                    self.assertNotEqual(result.returncode, 0, report)
                    self.assertIn(error, report)
                    self.assertFalse(reads.exists())
                    self.assertFalse((output / "baseline.json").exists())
                    self.assertFalse((output / "staging-deploy.json").exists())
                    return
                self.assertEqual(result.returncode, 0, report)
                provider_reads = reads.read_text().splitlines()
                self.assertEqual(provider_reads, [f"{route}/scripts/staging_rollout.py {workflow_sha} {path}"
                                                  for path in ("applications", "applications/app_fixture/rollouts?limit=100")])
                baseline = json.loads((output / "baseline.json").read_text())
                config = json.loads((output / "staging-deploy.json").read_text())
                self.assertEqual((baseline["revision"], baseline["deploy_id"]), (serving_sha, env["DEPLOY_ID"]))
                self.assertEqual(config["main"], str(source / "wrangler/src/index.ts"))
                stage = config["env"]["staging"]
                self.assertEqual(stage["vars"]["CONTAINER_RELEASE"], serving_sha)
                container = stage["containers"][0]
                self.assertEqual(container["image"], str(source / "Dockerfile"))
                self.assertEqual(container["image_build_context"], str(source))
                self.assertEqual(container["image_vars"], {"BOT_BUILD_REVISION": serving_sha,
                                 "BOT_BUILD_ID": "123-1", "BOT_DEPLOY_ID": env["DEPLOY_ID"]})
                self.assertEqual(Path(env["GITHUB_OUTPUT"]).read_text(), "promotion_clean=true\n")

            exercise("legacy", sha, source, error="invalid_build_identity")
            rehearsal = {"ROLLBACK_REHEARSAL_PREPARE": str(rehearsal_flag).lower()}
            exercise("rehearsal", sha, tooling, **rehearsal)
            exercise("wrong_suffix", sha, tooling, error="invalid_build_identity", DEPLOY_ID="2026.11.25.1+0000000",
                     **rehearsal)
            exercise("empty_source", sha, tooling, error="Missing validated staging SHA",
                     STAGING_SOURCE_SHA="", **rehearsal)
            exercise("mismatched_source", sha, tooling, error="Staging prepare source checkout mismatch",
                     STAGING_SOURCE_SHA=sha[:7] + "0" * 33, **rehearsal)
            git(tooling, "checkout", "--detach", sha)
            exercise("wrong_tooling", sha, tooling, error="Staging prepare workflow checkout mismatch", **rehearsal)
            git(tooling, "checkout", "--detach", workflow_sha)
            exercise("reviewed", sha, tooling, STAGING_GATE_ROOT="../tooling")
            git(source, "checkout", "--detach", workflow_sha)
            exercise("ordinary", workflow_sha, source)
