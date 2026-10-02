"""Pin deploy-production.yml's guards and run its inline steps offline.

No GitHub or Cloudflare credentials: `gh` is faked through subprocess, and
curl/sleep through PATH. Nothing here dispatches the workflow.
"""

import contextlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/deploy-production.yml"
STAGING = ROOT / ".github/workflows/deploy-staging.yml"
LINES = WORKFLOW.read_text().splitlines()
REPO = "TogetherWeOwn/two-bot-next"
SHA = "0123456789abcdef0123456789abcdef01234567"
VERSION = "0b6e5e3c-1f2a-4b3c-8d4e-5f6a7b8c9d0e"
OLD_VERSION = "9f8e7d6c-5b4a-4c3d-9e2f-1a0b9c8d7e6f"
GUARD_STEP = "Refuse unverified SHA or unprotected Environment"
WRANGLER_ACTION = "cloudflare/wrangler-action@"


def children(lines, indent):
    """Group `lines` into the YAML mapping keys found at `indent` spaces."""
    blocks, key, pad = {}, None, " " * indent
    for line in lines:
        if not line.strip() or line.strip().startswith("#"):
            continue
        if line.startswith(pad) and line[indent] != " ":
            key = line.strip().split(":", 1)[0]
            blocks[key] = [line]
        elif key is not None:
            blocks[key].append(line)
    return blocks


def value(block):
    return block[0].split(":", 1)[1].split(" #", 1)[0].strip()


def steps(job):
    found = []
    for line in children(job[1:], 4)["steps"][1:]:
        if line.startswith("      - "):
            found.append([])
        found[-1].append(line)
    return found


def inline_script(name):
    start = LINES.index(f"      - name: {name}")
    start = next(i for i in range(start, len(LINES)) if LINES[i].startswith("        run: |")) + 1
    end = start
    while end < len(LINES) and (not LINES[end].strip() or LINES[end].startswith("          ")):
        end += 1
    return textwrap.dedent("\n".join(LINES[start:end]))


def run_blocks():
    blocks, current = [], None
    for line in LINES:
        if current is not None:
            if not line.strip() or line.startswith(current[0]):
                current[1].append(line)
                continue
            blocks.append("\n".join(current[1]))
            current = None
        match = re.match(r"^(\s*)(?:- )?run: (.*)$", line)
        if match:
            current = (match.group(1) + "  ", [match.group(2)])
    if current is not None:
        blocks.append("\n".join(current[1]))
    return blocks


TOP = children(LINES, 0)
JOBS = children(TOP["jobs"][1:], 2)
GUARD = inline_script(GUARD_STEP)
RECORD = inline_script("Record the Worker version and SHA")
URL_CHECK = inline_script("Require a production Worker URL distinct from staging")
GATE = inline_script("Gate on /health + truthful /readyz")


def green():
    return {
        "compare": {"status": "ahead"},
        "check": {"check_runs": [{"name": "check", "conclusion": "success"}]},
        "worker check": {"check_runs": [{"name": "worker check", "conclusion": "success"}]},
        "staging": {"total_count": 1, "workflow_runs": [{"conclusion": "success"}]},
        "environment": {
            "name": "production",
            "protection_rules": [
                {"type": "wait_timer", "wait_timer": 0},
                {"type": "required_reviewers", "prevent_self_review": True,
                 "reviewers": [{"type": "User", "reviewer": {"login": "approver"}}]},
            ],
            "deployment_branch_policy": {"protected_branches": False, "custom_branch_policies": True},
        },
        "branches": {"total_count": 1, "branch_policies": [{"name": "main", "type": "branch"}]},
    }


def failed_call():
    return subprocess.CalledProcessError(1, ["gh", "api"], "gh: Not Found (HTTP 404)")


class StaticGuardTests(unittest.TestCase):
    def test_dispatch_is_the_only_trigger_and_sha_is_required(self):
        triggers = children(TOP["on"][1:], 2)
        self.assertEqual(list(triggers), ["workflow_dispatch"])
        inputs = children(children(triggers["workflow_dispatch"][1:], 4)["inputs"][1:], 6)
        self.assertEqual(value(children(inputs["sha"][1:], 8)["required"]), "true")
        self.assertEqual(value(children(inputs["rollback"][1:], 8)["required"]), "false")
        text = "\n".join(TOP["on"])
        for trigger in ("push", "pull_request", "pull_request_target", "schedule", "workflow_run", "workflow_call"):
            self.assertNotRegex(text, rf"(?m)^\s*{trigger}:", trigger)

    def test_wrangler_and_secrets_only_run_in_the_production_environment(self):
        production = children(JOBS["production"][1:], 4)
        self.assertEqual(value(production["environment"]), "production")
        self.assertEqual(value(production["needs"]), "guard")
        guard = children(JOBS["guard"][1:], 4)
        self.assertNotIn("environment", guard)
        self.assertNotIn("secrets.", "\n".join(JOBS["guard"]))
        wrangler = [s for s in steps(JOBS["production"]) if WRANGLER_ACTION in "\n".join(s)]
        self.assertEqual(len(wrangler), 4)
        for step in wrangler:
            self.assertIn("          environment: production", step)

    def test_concurrency_queues_instead_of_cancelling(self):
        concurrency = children(TOP["concurrency"][1:], 2)
        self.assertEqual(value(concurrency["group"]), "deploy-production")
        self.assertEqual(value(concurrency["cancel-in-progress"]), "false")

    def test_every_job_runs_on_the_self_hosted_pool(self):
        self.assertEqual(list(JOBS), ["guard", "production"])
        for name, job in JOBS.items():
            self.assertEqual(value(children(job[1:], 4)["runs-on"]), "[self-hosted, two-selfhosted]", name)
        self.assertNotIn("ubuntu-", WORKFLOW.read_text())

    def test_permissions_are_least_privilege(self):
        self.assertEqual(value(TOP["permissions"]), "{}")
        for name, job in JOBS.items():
            granted = {
                key: value(block)
                for key, block in children(children(job[1:], 4)["permissions"][1:], 6).items()
            }
            self.assertTrue(granted, name)
            self.assertEqual(set(granted.values()), {"read"}, name)
        self.assertEqual(
            set(children(children(JOBS["production"][1:], 4)["permissions"][1:], 6)), {"contents"}
        )

    def test_actions_are_pinned_and_wrangler_matches_staging(self):
        uses = [line.split("uses:", 1)[1].strip() for line in LINES if "uses:" in line]
        self.assertTrue(uses)
        for ref in uses:
            self.assertRegex(ref, r"^[\w.-]+/[\w.-]+@[0-9a-f]{40}(\s|$)", ref)
        staging = {
            line.split("uses:", 1)[1].split()[0]
            for line in STAGING.read_text().splitlines() if WRANGLER_ACTION in line
        }
        production = {ref.split()[0] for ref in uses if ref.startswith(WRANGLER_ACTION)}
        self.assertEqual(len(staging), 1)
        self.assertEqual(production, staging)

    def test_the_sha_guard_is_the_first_step_of_the_first_job(self):
        first_job = next(iter(JOBS))
        self.assertEqual(first_job, "guard")
        first = steps(JOBS[first_job])[0]
        self.assertEqual(first[0], f"      - name: {GUARD_STEP}")
        self.assertIn("        id: guard", first)
        self.assertIn("        shell: python3 {0}", first)
        self.assertIn("          GH_TOKEN: ${{ github.token }}", first)
        for check in ("[0-9a-f]{40}", "compare/{sha}...main", "check-runs", "deploy-staging.yml/runs"):
            self.assertIn(check, GUARD)

    def test_production_deploys_only_validated_guard_outputs(self):
        # Raw inputs reach only the guard step's env; everything downstream
        # reads the guard's validated outputs.
        input_lines = [line.strip() for line in LINES if "inputs." in line]
        self.assertEqual(input_lines, ["SHA: ${{ inputs.sha }}", "ROLLBACK: ${{ inputs.rollback }}"])
        production = "\n".join(JOBS["production"])
        self.assertNotIn("inputs.", production)
        self.assertIn("ref: ${{ needs.guard.outputs.sha }}", production)
        self.assertIn("persist-credentials: false", production)
        self.assertIn('git merge-base --is-ancestor "$SHA" origin/main', production)
        self.assertIn("command: deploy --message ${{ needs.guard.outputs.sha }}", production)
        self.assertIn(
            "command: rollback ${{ needs.guard.outputs.version }} --message ${{ needs.guard.outputs.sha }} --yes",
            production,
        )
        conditions = {
            step[0]: [line.strip() for line in step if line.strip().startswith("if:")]
            for step in steps(JOBS["production"])
        }
        self.assertEqual(conditions["      - name: Deploy to production"], ["if: env.MODE == 'deploy'"])
        self.assertEqual(conditions["      - name: Roll back production"], ["if: env.MODE == 'rollback'"])

    def test_run_scripts_never_interpolate_expressions(self):
        blocks = run_blocks()
        self.assertGreaterEqual(len(blocks), 6)
        for block in blocks:
            self.assertNotIn("${{", block)


class GuardBehaviourTests(unittest.TestCase):
    def guard(self, sha=SHA, rollback="", ref="refs/heads/main", **responses):
        api = {**green(), **responses}
        calls = []

        def fake_check_output(args, **kwargs):
            self.assertEqual(args[:4], ["gh", "api", "--method", "GET"])
            self.assertTrue(all(flag == "-f" for flag in args[5::2]), args)
            path, query = args[4], dict(arg.split("=", 1) for arg in args[6::2])
            calls.append((path, query))
            if path == f"repos/{REPO}/compare/{sha.strip().lower()}...main":
                key = "compare"
            elif path == f"repos/{REPO}/commits/{sha.strip().lower()}/check-runs":
                self.assertEqual(query["app_id"], "15368")
                self.assertEqual(query["filter"], "latest")
                key = query["check_name"]
            elif path == f"repos/{REPO}/actions/workflows/deploy-staging.yml/runs":
                self.assertEqual(
                    {k: query[k] for k in ("head_sha", "branch", "status")},
                    {"head_sha": sha.strip().lower(), "branch": "main", "status": "success"},
                )
                key = "staging"
            elif path == f"repos/{REPO}/environments/production":
                key = "environment"
            elif path == f"repos/{REPO}/environments/production/deployment-branch-policies":
                key = "branches"
            else:
                self.fail(f"unexpected API call {path}")
            result = api[key]
            if isinstance(result, Exception):
                raise result
            return result if isinstance(result, str) else json.dumps(result)

        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            output, summary = Path(tmp) / "output", Path(tmp) / "summary"
            env = {
                "GITHUB_REPOSITORY": REPO, "GITHUB_REF": ref, "SHA": sha, "ROLLBACK": rollback,
                "GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(summary),
            }
            stdout, code = io.StringIO(), 0
            with patch.dict(os.environ, env, clear=True), \
                    patch("subprocess.check_output", side_effect=fake_check_output), \
                    contextlib.redirect_stdout(stdout):
                try:
                    exec(GUARD, {})
                except SystemExit as exit_:
                    code = exit_.code
            outputs = dict(
                line.split("=", 1) for line in (output.read_text().splitlines() if output.exists() else [])
            )
            return code, outputs, summary.read_text() if summary.exists() else "", calls, stdout.getvalue()

    def assertRefused(self, result, reason):
        code, outputs, _, _, stdout = result
        self.assertEqual(code, 1)
        self.assertEqual(outputs, {})
        self.assertIn("::error::Refusing production", stdout)
        self.assertIn(reason, stdout)

    def test_green_main_sha_passes_and_reports(self):
        code, outputs, summary, calls, _ = self.guard()
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"sha": SHA, "mode": "deploy", "version": ""})
        self.assertIn(SHA, summary)
        self.assertEqual(
            [query.get("check_name") for path, query in calls if path.endswith("/check-runs")],
            ["check", "worker check"],
        )

    def test_main_head_itself_passes_and_sha_is_normalised(self):
        code, outputs, _, _, _ = self.guard(sha=f" {SHA.upper()}\n", compare={"status": "identical"})
        self.assertEqual(code, 0)
        self.assertEqual(outputs["sha"], SHA)

    def test_malformed_sha_is_refused_before_any_api_call(self):
        for sha in ("", "main", SHA[:39], SHA + "0", "g" * 40, "$(id)", f"{SHA};id", f"{SHA[:20]} {SHA[20:]}"):
            with self.subTest(sha=sha):
                result = self.guard(sha=sha)
                self.assertRefused(result, "40-character hex")
                self.assertEqual(result[3], [])

    def test_rollback_needs_a_version_uuid(self):
        for bad in ("1234", "latest", "../v", f"{VERSION} --force", VERSION[:-1]):
            with self.subTest(rollback=bad):
                result = self.guard(rollback=bad)
                self.assertRefused(result, "Worker version ID")
                self.assertEqual(result[3], [])
        code, outputs, summary, _, _ = self.guard(rollback=VERSION.upper())
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"sha": SHA, "mode": "rollback", "version": VERSION})
        self.assertIn(VERSION, summary)

    def test_dispatch_from_another_ref_is_refused(self):
        for ref in ("refs/heads/feature", "refs/tags/v1", "refs/heads/main2", ""):
            with self.subTest(ref=ref):
                result = self.guard(ref=ref)
                self.assertRefused(result, "from main only")
                self.assertEqual(result[3], [])

    def test_sha_not_on_main_is_refused(self):
        for status in ("diverged", "behind", None):
            with self.subTest(status=status):
                self.assertRefused(self.guard(compare={"status": status}), "not an ancestor of origin/main")

    def test_missing_or_red_checks_are_refused(self):
        for name in ("check", "worker check"):
            for runs in ([], [{"conclusion": "failure"}], [{"conclusion": None}],
                         [{"conclusion": "success"}, {"conclusion": "cancelled"}], [{"conclusion": "skipped"}]):
                with self.subTest(name=name, runs=runs):
                    self.assertRefused(self.guard(**{name: {"check_runs": runs}}), f"`{name}` has no successful run")

    def test_sha_without_a_successful_staging_deploy_is_refused(self):
        self.assertRefused(self.guard(staging={"total_count": 0, "workflow_runs": []}), "deploy-staging has no successful run")

    def test_missing_production_environment_is_refused(self):
        # A missing Environment would be auto-created with no reviewers.
        self.assertRefused(self.guard(environment=failed_call()), "production Environment from the GitHub API")

    def test_environment_without_required_reviewers_is_refused(self):
        for rules in ([], [{"type": "wait_timer", "wait_timer": 30}],
                      [{"type": "required_reviewers", "reviewers": []}], None):
            with self.subTest(rules=rules):
                environment = {**green()["environment"], "protection_rules": rules}
                self.assertRefused(self.guard(environment=environment), "no required reviewers")

    def test_environment_must_deploy_from_main_only(self):
        for policy in (None, {"protected_branches": True, "custom_branch_policies": False}):
            with self.subTest(policy=policy):
                environment = {**green()["environment"], "deployment_branch_policy": policy}
                self.assertRefused(self.guard(environment=environment), "restrict deployments to main")
        for branches in ([], [{"name": "main", "type": "branch"}, {"name": "release/*", "type": "branch"}],
                         [{"name": "*", "type": "branch"}], [{"name": "main", "type": "tag"}]):
            with self.subTest(branches=branches):
                result = self.guard(branches={"branch_policies": branches})
                self.assertRefused(result, "from main only")

    def test_api_failures_fail_closed(self):
        for key in ("compare", "check", "worker check", "staging", "branches"):
            for broken in (failed_call(), "not json"):
                with self.subTest(key=key, broken=broken):
                    self.assertRefused(self.guard(**{key: broken}), "failing closed")


def deployment(*versions):
    return json.dumps({
        "id": "d0000000-0000-4000-8000-000000000000", "source": "wrangler", "strategy": "percentage",
        "author_email": "deployer@example.com", "created_on": "2026-10-02T00:00:00Z",
        "versions": [{"version_id": vid, "percentage": pct} for vid, pct in versions],
    })


class RecordStepTests(unittest.TestCase):
    def record(self, mode="deploy", before="", after="", target=""):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            summary = Path(tmp) / "summary"
            env = {
                "SHA": SHA, "MODE": mode, "TARGET_VERSION": target, "BEFORE": before, "AFTER": after,
                "GITHUB_STEP_SUMMARY": str(summary),
            }
            stdout, code = io.StringIO(), 0
            with patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(stdout):
                try:
                    exec(RECORD, {})
                except SystemExit as exit_:
                    code = exit_.code
            text = summary.read_text()
            self.assertNotIn("deployer@example.com", text + stdout.getvalue())
            return code, text

    def test_deploy_records_sha_new_version_and_rollback_target(self):
        code, summary = self.record(
            before=deployment((OLD_VERSION, 100)), after=f"⛅️ wrangler 4\n{deployment((VERSION, 100))}\n"
        )
        self.assertEqual(code, 0)
        self.assertIn(f"- SHA: `{SHA}`", summary)
        self.assertIn(f"now active: `{VERSION}` (100%)", summary)
        self.assertIn(f"Previously active: `{OLD_VERSION}` (100%)", summary)
        self.assertIn(f"`rollback={OLD_VERSION}`", summary)

    def test_first_deploy_has_no_previous_version(self):
        code, summary = self.record(before="", after=deployment((VERSION, 100)))
        self.assertEqual(code, 0)
        self.assertIn("Previously active: none", summary)
        self.assertNotIn("rollback=", summary)

    def test_deploy_that_changed_nothing_fails(self):
        same = deployment((OLD_VERSION, 100))
        self.assertEqual(self.record(before=same, after=same)[0], 1)

    def test_unreadable_new_version_fails_but_still_records(self):
        for after in ("", "X [ERROR] Authentication error", "{not json}", json.dumps({"versions": "?"})):
            with self.subTest(after=after):
                code, summary = self.record(before=deployment((OLD_VERSION, 100)), after=after)
                self.assertEqual(code, 1)
                self.assertIn(f"`rollback={OLD_VERSION}`", summary)

    def test_rollback_must_leave_only_the_target_serving(self):
        before = deployment((OLD_VERSION, 100))
        self.assertEqual(self.record("rollback", before, deployment((VERSION, 100)), VERSION)[0], 0)
        self.assertEqual(self.record("rollback", before, before, VERSION)[0], 1)
        split = deployment((VERSION, 50), (OLD_VERSION, 50))
        self.assertEqual(self.record("rollback", before, split, VERSION)[0], 1)


FAKE_CURL = """#!/bin/sh
out= url=
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out=$2; shift 2 ;;
    -w|--max-time) shift 2 ;;
    -*) shift ;;
    *) url=$1; shift ;;
  esac
done
echo "$url" >> "$FAKE_LOG"
case "$url" in
  */health)
    n=$(grep -c '/health$' "$FAKE_LOG")
    code=$(echo $FAKE_HEALTH | awk -v n="$n" '{print (n <= NF) ? $n : $NF}') ;;
  */readyz) code=$FAKE_READYZ ;;
  *) code=000 ;;
esac
printf '{"components":"fake"}' > "$out"
printf '%s' "$code"
"""


class ShellStepTests(unittest.TestCase):
    def bash(self, script, **env):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            bin_dir = Path(tmp) / "bin"
            bin_dir.mkdir()
            for name, body in (("curl", FAKE_CURL), ("sleep", "#!/bin/sh\nexit 0\n")):
                (bin_dir / name).write_text(body)
                (bin_dir / name).chmod(0o755)
            step = Path(tmp) / "step.sh"
            step.write_text(script)
            log, summary = Path(tmp) / "curl.log", Path(tmp) / "summary"
            log.touch()
            run_env = {
                "PATH": f"{bin_dir}:{os.environ['PATH']}", "RUNNER_TEMP": tmp,
                "GITHUB_STEP_SUMMARY": str(summary), "FAKE_LOG": str(log), **env,
            }
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-eo", "pipefail", str(step)],
                env=run_env, capture_output=True, text=True, timeout=60,
            )
            return result.returncode, log.read_text().split(), summary.read_text() if summary.exists() else ""

    def gate(self, health="200", readyz="200"):
        return self.bash(GATE, PRODUCTION_URL="https://prod.example.workers.dev/",
                         FAKE_HEALTH=health, FAKE_READYZ=readyz)

    def test_ready_or_truthfully_parked_gateway_passes(self):
        for readyz, state in (("200", "gateway ready"), ("503", "gateway parked")):
            with self.subTest(readyz=readyz):
                code, urls, summary = self.gate(readyz=readyz)
                self.assertEqual(code, 0)
                self.assertEqual(urls, ["https://prod.example.workers.dev/health",
                                        "https://prod.example.workers.dev/readyz"])
                self.assertIn(f"/readyz {readyz} ({state}", summary)

    def test_health_is_polled_through_a_cold_start(self):
        code, urls, _ = self.gate(health="000 502 503 200")
        self.assertEqual(code, 0)
        self.assertEqual(urls.count("https://prod.example.workers.dev/health"), 4)

    def test_broken_readyz_fails(self):
        for readyz in ("500", "404", "000", "302"):
            with self.subTest(readyz=readyz):
                code, _, summary = self.gate(readyz=readyz)
                self.assertEqual(code, 1)
                self.assertIn("roll back", summary)

    def test_health_that_never_recovers_fails_without_probing_readyz(self):
        code, urls, summary = self.gate(health="502")
        self.assertEqual(code, 1)
        self.assertEqual(len(urls), 30)
        self.assertNotIn("https://prod.example.workers.dev/readyz", urls)
        self.assertIn("never 200", summary)

    def test_production_url_must_be_https_and_not_staging(self):
        staging = "https://staging.example.workers.dev"
        for url, ok in (("", False), ("http://prod.example.workers.dev", False), (staging, False),
                        (staging + "/", False), ("https://prod.example.workers.dev", True)):
            with self.subTest(url=url):
                code, urls, _ = self.bash(URL_CHECK, PRODUCTION_URL=url, STAGING_URL=staging)
                self.assertEqual(code == 0, ok)
                self.assertEqual(urls, [])


if __name__ == "__main__":
    unittest.main()
