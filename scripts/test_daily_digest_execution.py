#!/usr/bin/env python3
"""Execute digest failure/repair paths and the workflow gates with no network."""

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import yaml

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = yaml.safe_load((ROOT / ".github/workflows/daily-digest.yml").read_text())
SPEC = importlib.util.spec_from_file_location("digest_alert", ROOT / "scripts/digest_alert.py")
ALERT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ALERT)


def gate(expression, values, cancelled=False):
    """Evaluate the workflow's small boolean expression with actual step outputs."""
    text = expression.removeprefix("${{").removesuffix("}}").strip()
    text = text.replace("!cancelled()", repr(not cancelled))
    for key, value in sorted(values.items(), key=lambda item: -len(item[0])):
        text = text.replace(key, repr(value))
    text = text.replace("&&", " and ").replace("||", " or ").replace("!= true", "!= True")
    return eval(text, {"__builtins__": {}}, {})


class DigestExecutionTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)
        self.bin = self.dir / "bin"
        self.bin.mkdir()
        self.calls = self.dir / "calls.jsonl"
        # Never inherit a host credential, GITHUB_OUTPUT, DRY_RUN or git override.
        self.env = {
            "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
            "HOME": str(self.dir), "RUNNER_TEMP": str(self.dir),
            "GH_REPO": "o/r", "DIGEST_DATE": "2026-10-21", "DRY_RUN": "false",
            "GITHUB_OUTPUT": str(self.dir / "outputs"), "GH_CALLS": str(self.calls),
        }
        gh = self.bin / "gh"
        gh.write_text(
            "#!" + shutil.which("python3") + "\n"
            "import json, os, sys\n"
            "with open(os.environ['GH_CALLS'], 'a') as f: f.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "if sys.argv[1:3] == ['release', 'view']:\n"
            "    print(os.environ.get('RELEASE_VIEW_JSON', '{}'))\n"
            "    sys.exit(0 if os.environ.get('RELEASE_EXISTS') == 'true' else 1)\n"
            "sys.exit(1 if os.environ.get('FAIL_CREATE') == 'true' else 0)\n"
        )
        gh.chmod(0o755)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        (self.dir / "release-please-config.json").write_text(json.dumps({
            "packages": {".": {"changelog-sections": [{"type": "fix", "section": "Fixed"}]}}
        }))
        self.git("add", ".")
        self.git("commit", "-qm", "fix: first")
        self.first = self.git("rev-parse", "HEAD")
        self.git("tag", "v2026.10.20")
        self.git("commit", "-q", "--allow-empty", "-m", "fix: second")
        self.sha = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/origin/main", self.sha)
        self.env["PROD_SHA"] = self.sha

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.dir, env=self.env, text=True).strip()

    def run_digest(self, **extra):
        self.calls.write_text("")
        Path(self.env["GITHUB_OUTPUT"]).write_text("")
        result = subprocess.run(
            ["node", str(ROOT / ".github/scripts/daily-digest.cjs")],
            cwd=self.dir, env={**self.env, **extra}, capture_output=True, text=True, timeout=30,
        )
        outputs = dict(line.split("=", 1) for line in Path(self.env["GITHUB_OUTPUT"]).read_text().splitlines())
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        return result, outputs, calls

    def alerts(self, result, outputs, dry_run=False, cancelled=False):
        values = {"needs.digest.result": "success" if result.returncode == 0 else "failure",
                  "inputs.dry_run": dry_run}
        for name in ("prod_changed", "released"):
            values["needs.digest.outputs." + name] = outputs.get(name, "")
        return gate(WORKFLOW["jobs"]["alert"]["if"], values, cancelled)

    def test_workflow_uses_executed_producers_and_output_bindings(self):
        job = WORKFLOW["jobs"]["digest"]
        steps = {step.get("id"): step for step in job["steps"] if step.get("id")}
        self.assertEqual(steps["digest"]["run"], "node .github/scripts/daily-digest.cjs")
        self.assertEqual(steps["resolve"]["run"], "python3 scripts/digest_resolve.py")
        self.assertEqual(steps["digest"]["env"]["PROD_SHA"], "${{ steps.resolve.outputs.live_sha }}")
        for name in ("released", "tag_name", "prod_changed", "prev_tag"):
            self.assertEqual(job["outputs"][name], "${{ steps.digest.outputs." + name + " }}")
        self.assertEqual(job["outputs"]["live_sha"], "${{ steps.resolve.outputs.live_sha }}")

    def test_failed_repair_also_reaches_the_alert_gate(self):
        self.git("tag", "v2026.10.21", self.sha)
        result, outputs, _ = self.run_digest(FAIL_CREATE="true")
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.alerts(result, outputs))
        self.assertEqual(outputs["released"], "false")

    def test_failed_publication_reaches_workflow_alert_and_incident_issue(self):
        result, outputs, _ = self.run_digest(FAIL_CREATE="true")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(outputs.get("prod_changed"), "true")
        self.assertEqual(outputs.get("released"), "false")
        self.assertTrue(self.alerts(result, outputs))
        # Use the workflow's alert env bindings, not a second set of test names.
        context = {"needs.digest.outputs.live_sha": self.sha,
                   "needs.digest.outputs.prev_tag": outputs["prev_tag"],
                   "github.run_id": "1", "secrets.GITHUB_TOKEN": "fixture"}
        env = {}
        for key, value in WORKFLOW["jobs"]["alert"]["steps"][-1]["env"].items():
            env[key] = context[value.removeprefix("${{").removesuffix("}}").strip()]
        with patch.dict(os.environ, env), \
                patch.object(ALERT.incident_issue, "open_incident", return_value=("opened", 3)) as opened:
            self.assertEqual(ALERT.main(["--live-sha", env["LIVE_SHA"], "--prev-tag", env["PREV_TAG"]]), 0)
        opened.assert_called_once()
        self.assertEqual(opened.call_args.args[1], "daily-digest")
        self.assertIn(self.sha[:7], opened.call_args.args[3])
        self.assertFalse(self.alerts(result, outputs, cancelled=True))
        self.assertFalse(self.alerts(result, outputs, dry_run=True))

    def test_success_and_policy_skips_do_not_alert(self):
        for extra in ({}, {"DRY_RUN": "true"}, {"PROD_SHA": self.first}):
            with self.subTest(extra=extra):
                result, outputs, _ = self.run_digest(**extra)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertFalse(self.alerts(result, outputs, dry_run=extra.get("DRY_RUN") == "true"))
        self.git("tag", "v2026.10.21", self.sha)
        result, outputs, _ = self.run_digest(RELEASE_EXISTS="true")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.alerts(result, outputs))

    def test_existing_release_rerun_dispatches_sbom_even_on_quiet_day(self):
        self.git("tag", "v2026.10.21", self.sha)
        result, outputs, calls = self.run_digest(RELEASE_EXISTS="true")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs.get("tag_name"), "v2026.10.21")
        step = WORKFLOW["jobs"]["digest"]["steps"][-1]
        self.assertTrue(gate(step["if"], {"steps.digest.outputs.released": outputs.get("released", "")}))
        dispatched = subprocess.run(["bash", "-euc", step["run"]], cwd=self.dir,
                                    env={**self.env, "TAG": outputs["tag_name"]}, capture_output=True, text=True)
        self.assertEqual(dispatched.returncode, 0, dispatched.stderr)
        all_calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertEqual(all_calls[-1], ["workflow", "run", "release.yml", "--ref", "main", "-f", "release_tag=v2026.10.21"])
        self.assertFalse(any(call[:2] == ["release", "create"] for call in calls))
        result, outputs, calls = self.run_digest(DRY_RUN="true")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs["released"], "false")
        self.assertEqual(calls, [])

    def test_next_day_quiet_and_rollback_redispatch_sbom_when_assets_missing(self):
        full = json.dumps({"assets": [{"name": name} for name in (
            "rust-workspace.cdx.json", "container-image.cdx.json", "SHA256SUMS",
            "source-sha.txt", "image-id.txt")]})
        partial = json.dumps({"assets": [{"name": "rust-workspace.cdx.json"}]})
        step = WORKFLOW["jobs"]["digest"]["steps"][-1]
        # Next morning: production still serves the v2026.10.20 target while
        # the date rolled to 2026-10-21, so the quiet branch runs for a tag
        # that is not today's. Missing assets redispatch the SBOM chain for
        # the last digest without cutting a tag or alerting.
        result, outputs, calls = self.run_digest(
            PROD_SHA=self.first, RELEASE_EXISTS="true", RELEASE_VIEW_JSON=partial)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs.get("tag_name"), "v2026.10.20")
        self.assertEqual(outputs.get("released"), "true")
        self.assertEqual(outputs.get("prod_changed"), "false")
        self.assertTrue(gate(step["if"], {"steps.digest.outputs.released": outputs.get("released", "")}))
        self.assertFalse(self.alerts(result, outputs))
        self.assertIn(["release", "view", "v2026.10.20", "--json", "assets"], calls)
        self.assertFalse(any(call[:2] == ["release", "create"] for call in calls))
        # Complete assets still skip green without dispatching.
        result, outputs, _ = self.run_digest(
            PROD_SHA=self.first, RELEASE_EXISTS="true", RELEASE_VIEW_JSON=full)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs.get("tag_name"), "")
        self.assertEqual(outputs.get("released"), "false")
        # A failed lookup skips green too: never red, never a dispatch.
        result, outputs, _ = self.run_digest(PROD_SHA=self.first)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs.get("released"), "false")
        # A dry-run quiet day makes no gh calls at all.
        result, outputs, calls = self.run_digest(PROD_SHA=self.first, DRY_RUN="true")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs.get("released"), "false")
        self.assertEqual(calls, [])
        # Rollback serving likewise reconciles the last digest's assets.
        self.git("tag", "-f", "v2026.10.20", self.sha)
        result, outputs, calls = self.run_digest(
            PROD_SHA=self.first, RELEASE_EXISTS="true", RELEASE_VIEW_JSON=partial)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs.get("tag_name"), "v2026.10.20")
        self.assertEqual(outputs.get("released"), "true")
        self.assertEqual(outputs.get("prod_changed"), "false")
        self.assertFalse(self.alerts(result, outputs))
        self.assertFalse(any(call[:2] == ["release", "create"] for call in calls))

    def test_older_repair_and_backdated_fresh_cut_preserve_latest(self):
        self.git("tag", "v2026.10.22", self.sha)
        for date in ("2026-10-20", "2026-10-21"):
            with self.subTest(date=date):
                # Forward production allows the backdated fresh-cut path too.
                self.git("commit", "-q", "--allow-empty", "-m", "fix: forward")
                sha = self.git("rev-parse", "HEAD")
                self.git("update-ref", "refs/remotes/origin/main", sha)
                result, _, calls = self.run_digest(DIGEST_DATE=date, PROD_SHA=sha)
                self.assertEqual(result.returncode, 0, result.stderr)
                creates = [call for call in calls if call[:2] == ["release", "create"]]
                self.assertEqual(len(creates), 1)
                self.assertIn("--latest=false", creates[0])
                self.assertNotIn("--latest", creates[0])

    def test_nonexistent_dates_refuse_before_git_or_gh(self):
        for date in ("2026-02-30", "2026-02-29", "2026-13-01"):
            with self.subTest(date=date):
                result, outputs, calls = self.run_digest(DIGEST_DATE=date)
                self.assertNotEqual(result.returncode, 0)
                self.assertRegex(result.stderr, r"DIGEST_DATE.*calendar date")
                self.assertEqual(outputs, {})
                self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
