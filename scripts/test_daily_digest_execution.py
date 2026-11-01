#!/usr/bin/env python3
"""Execute the digest adapter through the workflow's own bindings and gates with no network."""

import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

import yaml

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = yaml.safe_load((ROOT / ".github/workflows/daily-digest.yml").read_text())
JOB = WORKFLOW["jobs"]["digest"]
STEPS = {step.get("id"): step for step in JOB["steps"] if step.get("id")}
DISPATCH = JOB["steps"][-1]
SPEC = importlib.util.spec_from_file_location("digest_alert", ROOT / "scripts/digest_alert.py")
ALERT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ALERT)
CALVER = "TogetherWeOwn/.github/actions/calver@"
SHA = "b" * 40
VIEW = "release view {} --json tagName,assets,publishedAt"
ASSETS = [{"name": name} for name in (
    "rust-workspace.cdx.json", "container-image.cdx.json", "SHA256SUMS", "source-sha.txt", "image-id.txt")]


def gate(expression, values, cancelled=False):
    """Evaluate the workflow's small boolean/`||` expression with actual step outputs."""
    text = expression.removeprefix("${{").removesuffix("}}").strip()
    text = text.replace("!cancelled()", repr(not cancelled))
    for key, value in sorted(values.items(), key=lambda item: -len(item[0])):
        text = text.replace(key, repr(value))
    # An output a skipped or failed step never wrote evaluates to ''.
    text = re.sub(r"\bsteps\.[\w-]+\.outputs\.[\w-]+", "''", text)
    text = text.replace("&&", " and ").replace("||", " or ").replace("!= true", "!= True")
    return eval(text, {"__builtins__": {}}, {})


class DigestExecutionTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)
        self.calls = self.dir / "calls.jsonl"
        gh = self.dir / "gh"
        gh.write_text(
            "#!" + shutil.which("python3") + "\n"
            "import json, os, sys\n"
            "with open(os.environ['GH_CALLS'], 'a') as f: f.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "fixtures = json.loads(os.environ.get('GH_FIXTURES', '{}'))\n"
            "key = ' '.join(sys.argv[1:])\n"
            "if key in fixtures:\n"
            "    print(json.dumps(fixtures[key]))\n"
            "    sys.exit(0)\n"
            "if sys.argv[1:3] == ['release', 'view']:\n"
            "    view = os.environ.get('RELEASE_VIEW_JSON')\n"
            "    print(view or '')\n"
            "    sys.exit(0 if view else 1)\n"
            "sys.exit(0 if sys.argv[1:3] == ['workflow', 'run'] else 1)\n"
        )
        gh.chmod(0o755)
        # Never inherit a host credential, GITHUB_OUTPUT or DRY_RUN.
        self.env = {
            "PATH": str(self.dir) + os.pathsep + os.environ["PATH"], "HOME": str(self.dir),
            "GH_REPO": "o/r", "GH_CALLS": str(self.calls),
        }

    def step(self, step_id, context):
        """Run a `run:` step with its env resolved from the workflow's own bindings."""
        step = STEPS[step_id]
        out = self.dir / f"{step_id}.out"
        out.write_text("")
        self.calls.write_text("")
        env = dict(self.env, GITHUB_OUTPUT=str(out))
        for key, value in (step.get("env") or {}).items():
            expr = value.removeprefix("${{").removesuffix("}}").strip()
            value = context.get(expr, "")
            env[key] = str(value).lower() if isinstance(value, bool) else str(value)
        result = subprocess.run(["bash", "-euc", step["run"]], cwd=ROOT, env=env,
                                capture_output=True, text=True, timeout=30)
        outputs = {}
        for line in out.read_text().splitlines():
            key, _, value = line.partition("=")
            outputs[f"steps.{step_id}.outputs.{key}"] = value
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        return result, outputs, calls

    def dispatch(self, context):
        """Run the dispatch step's loop with TAGS built as its format() binding does."""
        self.assertEqual(DISPATCH["env"]["TAGS"], "${{ format('{0} {1}', steps.digest.outputs.sbom_tags, "
                                                 "steps.record.outputs.sbom_tags) }}")
        tags = "{0} {1}".format(context.get("steps.digest.outputs.sbom_tags", ""),
                                context.get("steps.record.outputs.sbom_tags", ""))
        self.calls.write_text("")
        done = subprocess.run(["bash", "-euc", DISPATCH["run"]], cwd=self.dir,
                              env={**self.env, "TAGS": tags}, capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def job_outputs(self, context):
        return {f"needs.digest.outputs.{name}": gate(value, context)
                for name, value in JOB["outputs"].items()}

    def alerts(self, needs, dry_run=False, cancelled=False):
        return gate(WORKFLOW["jobs"]["alert"]["if"], {**needs, "inputs.dry_run": dry_run}, cancelled)

    def planned(self, dry_run=False):
        """Resolve -> request -> (plan stub) -> adapt, through the real bindings."""
        context = {"steps.resolve.outputs.live_sha": SHA, "inputs.dry_run": dry_run,
                   "secrets.GITHUB_TOKEN": "fixture", "github.repository": "o/r"}
        result, outputs, _ = self.step("request", context)
        self.assertEqual(result.returncode, 0, result.stderr)
        request = json.loads(outputs["steps.request.outputs.request"])
        self.assertEqual(request["input"]["sha"], SHA)
        config = json.loads((ROOT / "release-please-config.json").read_text())
        self.assertEqual(request["input"]["sections"],
                         config["packages"]["."]["changelog-sections"])
        self.assertIn({"type": "security", "section": "Fixed"},
                      request["input"]["sections"])
        plan = {"schema": "calver-plan-v1", "sha": SHA, "policy": "live-digest", "tag": "v2026.1101.0"}
        context.update({"steps.plan.outputs.plan": json.dumps(plan),
                        "steps.plan.outputs.previous-stable-tag": "v2026.10.23"})
        result, outputs, _ = self.step("digest", context)
        self.assertEqual(result.returncode, 0, result.stderr)
        context.update(outputs)
        return context

    def test_workflow_pins_and_bindings(self):
        self.assertEqual(STEPS["resolve"]["run"], "python3 scripts/digest_resolve.py")
        self.assertEqual(STEPS["request"]["env"]["PROD_SHA"], "${{ steps.resolve.outputs.live_sha }}")
        self.assertEqual(STEPS["digest"]["env"]["PROD_SHA"], "${{ steps.resolve.outputs.live_sha }}")
        for step_id, mode in (("request", "request"), ("digest", "adapt"), ("record", "record")):
            self.assertEqual(STEPS[step_id]["run"], f"node .github/scripts/daily-digest.cjs {mode}")
        pins = {STEPS[step_id]["uses"] for step_id in ("plan", "publish")}
        self.assertEqual(len(pins), 1, "plan and publish must pin the same shared commit")
        pin = pins.pop()
        self.assertRegex(pin, "^" + re.escape(CALVER) + "[0-9a-f]{40}$")
        self.assertEqual(STEPS["plan"]["with"]["request"], "${{ steps.request.outputs.request }}")
        self.assertEqual(STEPS["publish"]["with"]["request"], "${{ steps.digest.outputs.publish_request }}")
        for step_id in ("plan", "publish"):
            self.assertEqual(STEPS[step_id]["with"]["runs-on-json"], '"ubuntu-latest"')
        self.assertEqual(STEPS["publish"]["if"], "steps.digest.outputs.publish == 'true'")
        self.assertEqual(STEPS["record"]["if"], "steps.publish.outcome == 'success'")
        self.assertEqual(JOB["outputs"]["live_sha"], "${{ steps.resolve.outputs.live_sha }}")

    def test_failed_publication_reaches_workflow_alert_and_incident_issue(self):
        context = self.planned()
        publish = json.loads(context["steps.digest.outputs.publish_request"])
        self.assertEqual((publish["dryRun"], publish["repair"]), (False, True))
        self.assertTrue(gate(STEPS["publish"]["if"], context))
        # The publish step failed, so record never ran.
        needs = self.job_outputs(context)
        self.assertEqual(needs["needs.digest.outputs.released"], "false")
        self.assertEqual(needs["needs.digest.outputs.prev_tag"], "v2026.10.23")
        self.assertTrue(self.alerts(needs))
        self.assertFalse(self.alerts(needs, cancelled=True))
        self.assertFalse(self.alerts(needs, dry_run=True))
        self.assertFalse(gate(DISPATCH["if"], context))
        alert_context = {"needs.digest.outputs.live_sha": SHA,
                         "needs.digest.outputs.prev_tag": needs["needs.digest.outputs.prev_tag"],
                         "github.run_id": "1", "secrets.GITHUB_TOKEN": "fixture"}
        env = {key: alert_context[value.removeprefix("${{").removesuffix("}}").strip()]
               for key, value in WORKFLOW["jobs"]["alert"]["steps"][-1]["env"].items()}
        with patch.dict(os.environ, env), \
                patch.object(ALERT.incident_issue, "open_incident", return_value=("opened", 3)) as opened:
            self.assertEqual(ALERT.main(["--live-sha", env["LIVE_SHA"], "--prev-tag", env["PREV_TAG"]]), 0)
        opened.assert_called_once()
        self.assertEqual(opened.call_args.args[1], "daily-digest")
        self.assertIn(SHA[:7], opened.call_args.args[3])

    def test_fresh_release_records_and_dispatches_sbom(self):
        context = self.planned()
        context.update({"steps.publish.outputs.created": "true", "steps.publish.outputs.tag": "v2026.1101.0"})
        result, outputs, calls = self.step("record", context)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [])
        context.update(outputs)
        needs = self.job_outputs(context)
        self.assertEqual(needs["needs.digest.outputs.released"], "true")
        self.assertEqual(needs["needs.digest.outputs.tag_name"], "v2026.1101.0")
        self.assertFalse(self.alerts(needs))
        self.assertTrue(gate(DISPATCH["if"], context))
        self.assertEqual(self.dispatch(context), [
            ["workflow", "run", "release.yml", "--ref", "main", "-f", "release_tag=v2026.1101.0"]])

    def test_skips_never_alert_and_redispatch_missing_assets(self):
        partial = json.dumps({"tagName": "v2026.10.23", "assets": [{"name": "SHA256SUMS"}],
                              "publishedAt": "2026-10-20T18:08:11Z"})
        for reason in ("quiet-day", "rollback", "digest-already-published"):
            with self.subTest(reason=reason):
                self.env["RELEASE_VIEW_JSON"] = partial
                context = {"steps.resolve.outputs.live_sha": SHA, "steps.plan.outputs.skipped": "true",
                           "steps.plan.outputs.reason": reason, "inputs.dry_run": False}
                result, outputs, calls = self.step("digest", context)
                self.assertEqual(result.returncode, 0, result.stderr)
                context.update(outputs)
                self.assertFalse(gate(STEPS["publish"]["if"], context))
                self.assertFalse(self.alerts(self.job_outputs(context)))
                self.assertTrue(gate(DISPATCH["if"], context))
                self.assertFalse(any(call[:2] == ["release", "create"] for call in calls))
                self.assertEqual(self.dispatch(context), [
                    ["workflow", "run", "release.yml", "--ref", "main", "-f", "release_tag=v2026.10.23"]])

    def test_latest_and_published_tags_dispatch_once_each(self):
        # Latest lacks assets and the fresh publication is a second tag: both
        # dispatch; a tag owed twice dispatches once.
        context = {"steps.digest.outputs.sbom_tags": "v2026.1101.0",
                   "steps.record.outputs.sbom_tags": "v2026.1102.0"}
        self.assertEqual([call[-1] for call in self.dispatch(context)],
                         ["release_tag=v2026.1101.0", "release_tag=v2026.1102.0"])
        context["steps.record.outputs.sbom_tags"] = "v2026.1101.0"
        self.assertEqual([call[-1] for call in self.dispatch(context)], ["release_tag=v2026.1101.0"])

    def test_completed_replay_neither_publishes_nor_alerts(self):
        # Production stays on (or rolls back to) an already-published CalVer
        # digest: the planner recovers its plan, the Release exists.
        self.env["GH_FIXTURES"] = json.dumps({VIEW.format("v2026.1101.0"): {
            "tagName": "v2026.1101.0", "assets": ASSETS, "publishedAt": "2026-11-01T12:00:00Z"}})
        context = self.planned()
        self.assertEqual(context["steps.digest.outputs.publish"], "false")
        self.assertEqual(context["steps.digest.outputs.prod_changed"], "false")
        self.assertFalse(gate(STEPS["publish"]["if"], context))
        needs = self.job_outputs(context)
        self.assertEqual(needs["needs.digest.outputs.released"], "false")
        self.assertFalse(self.alerts(needs))
        self.assertFalse(gate(DISPATCH["if"], context))

    def test_unfinished_tag_is_finished_from_its_frozen_plan(self):
        # Today's tag lost its Release, then production moved: the planner
        # skips the new SHA, and the adapter repairs the old tag's own plan.
        today = time.gmtime()
        tag = f"v{today.tm_year}.{today.tm_mon}{today.tm_mday:02d}.0"
        frozen = {"schema": "calver-plan-v1", "sha": "a" * 40, "policy": "live-digest",
                  "tag": tag, "previousStableTag": "v2026.10.23", "id": "y"}
        self.env["GH_FIXTURES"] = json.dumps({
            f"api repos/o/r/git/matching-refs/tags/{tag[:-1]}": [
                {"ref": f"refs/tags/{tag}", "object": {"type": "tag", "sha": "f" * 40}}],
            "api repos/o/r/git/tags/" + "f" * 40: {
                "message": "calver-plan-v1\n" + json.dumps({**frozen, "publicationEvidence": None})},
        })
        context = {"steps.resolve.outputs.live_sha": SHA, "steps.plan.outputs.skipped": "true",
                   "steps.plan.outputs.reason": "digest-already-published", "inputs.dry_run": False,
                   "github.repository": "o/r", "secrets.GITHUB_TOKEN": "fixture"}
        result, outputs, calls = self.step("digest", context)
        self.assertEqual(result.returncode, 0, result.stderr)
        context.update(outputs)
        self.assertTrue(gate(STEPS["publish"]["if"], context))
        publish = json.loads(context["steps.digest.outputs.publish_request"])
        self.assertEqual(publish, {"phase": "publish", "plan": frozen, "dryRun": False, "repair": True})
        self.assertIn(["release", "view", tag, "--json", "tagName,assets,publishedAt"], calls)
        # A failed repair is owed work: it reaches the alert gate.
        needs = self.job_outputs(context)
        self.assertEqual(needs["needs.digest.outputs.prod_changed"], "true")
        self.assertEqual(needs["needs.digest.outputs.prev_tag"], "v2026.10.23")
        self.assertTrue(self.alerts(needs))
        # A successful repair of an existing tag records and dispatches SBOM.
        context.update({"steps.publish.outputs.created": "true", "steps.publish.outputs.tag": tag})
        result, outputs, _ = self.step("record", context)
        self.assertEqual(result.returncode, 0, result.stderr)
        context.update(outputs)
        self.assertFalse(self.alerts(self.job_outputs(context)))
        self.assertEqual([call[-1] for call in self.dispatch(context)], [f"release_tag={tag}"])

    def test_dry_run_publishes_nothing_and_never_alerts(self):
        context = self.planned(dry_run=True)
        self.assertEqual(json.loads(context["steps.digest.outputs.publish_request"])["dryRun"], True)
        context.update({"steps.publish.outputs.created": "false", "steps.publish.outputs.tag": "v2026.1101.0"})
        result, outputs, calls = self.step("record", context)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [])
        context.update(outputs)
        needs = self.job_outputs(context)
        self.assertEqual(needs["needs.digest.outputs.released"], "false")
        self.assertFalse(self.alerts(needs, dry_run=True))
        self.assertFalse(gate(DISPATCH["if"], context))


if __name__ == "__main__":
    unittest.main()
