"""Pin deploy-production.yml's guards and run its inline steps offline.

No GitHub or Cloudflare credentials: `gh` is faked through subprocess and
sleep through PATH. The readyz gate talks to a loopback HTTP server that
scripts each answer. Nothing here dispatches the workflow.
"""

import contextlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import threading
import tomllib
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/deploy-production.yml"
STAGING = ROOT / ".github/workflows/deploy-staging.yml"
SCRIPT = ROOT / "scripts/production_deploy.py"
WRANGLER_DIR = ROOT / "wrangler"
WRANGLER_TOML = WRANGLER_DIR / "wrangler.toml"
LINES = WORKFLOW.read_text().splitlines()
REPO = "TogetherWeOwn/two-bot-next"
SHA = "0123456789abcdef0123456789abcdef01234567"
BUILD_ID = "700-1"
AGENT = "two-bot-next-production-rollout/1.0"
VERSION = "0b6e5e3c-1f2a-4b3c-8d4e-5f6a7b8c9d0e"
OLD_VERSION = "9f8e7d6c-5b4a-4c3d-9e2f-1a0b9c8d7e6f"
GUARD_STEP = "Refuse unverified SHA or unprotected Environment"
RENDER_STEP = "Render the production deploy config with build identity"
GATE_STEP = "Gate on /health + /readyz revision"
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
CONFIRM = inline_script("Confirm the taken-over version still serves")
URL_CHECK = inline_script("Require a production Worker URL distinct from staging")
RENDER = inline_script(RENDER_STEP)
GATE = inline_script(GATE_STEP)


CI_RUN_ID = 101
CI_SUITE_ID = 201
CHECK_IDS = {"ci-ok": 301, "worker check": 302, "check": 303}


def workflow_run(run_id=CI_RUN_ID, number=1, attempt=1, suite_id=CI_SUITE_ID,
                 status="completed", conclusion="success"):
    return {
        "id": run_id, "run_number": number, "run_attempt": attempt,
        "check_suite_id": suite_id, "head_sha": SHA, "status": status, "conclusion": conclusion,
    }


def check_run(name, status="completed", conclusion="success", app_id=15368,
              check_id=None, suite_id=CI_SUITE_ID):
    check_id = CHECK_IDS[name] if check_id is None else check_id
    return {
        "id": check_id, "url": f"https://api.github.com/repos/{REPO}/check-runs/{check_id}",
        "name": name, "head_sha": SHA, "status": status, "conclusion": conclusion,
        "app": {"id": app_id}, "check_suite": {"id": suite_id},
    }


def workflow_job(name, run_id=CI_RUN_ID, attempt=1, check_id=None):
    return {
        "name": name, "run_id": run_id, "run_attempt": attempt, "head_sha": SHA,
        "check_run_url": check_run(name, check_id=check_id)["url"],
        "status": "completed", "conclusion": "success",
    }


def green():
    return {
        "compare": {"status": "ahead"},
        "ci-runs": {"total_count": 1, "workflow_runs": [workflow_run()]},
        "ci-run": workflow_run(),
        "ci-jobs": {"total_count": 2, "jobs": [workflow_job(name) for name in ("ci-ok", "worker check")]},
        "ci-ok": {"check_runs": [check_run("ci-ok")]},
        "check": {"check_runs": [check_run("check")]},
        "worker check": {"check_runs": [check_run("worker check")]},
        "staging": {"total_count": 1, "workflow_runs": [{"conclusion": "success"}]},
        "staging-all": {"total_count": 1, "workflow_runs": [staging_run()]},
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


def staging_run(run_id=501, number=5, status="completed", conclusion="success", sha=SHA):
    return {"id": run_id, "run_number": number, "head_sha": sha, "status": status, "conclusion": conclusion}


def failed_call():
    return subprocess.CalledProcessError(1, ["gh", "api"], "gh: Not Found (HTTP 404)")


class StaticGuardTests(unittest.TestCase):
    def test_dispatch_is_the_only_trigger_and_sha_defaults_to_last_staged(self):
        triggers = children(TOP["on"][1:], 2)
        self.assertEqual(list(triggers), ["workflow_dispatch"])
        inputs = children(children(triggers["workflow_dispatch"][1:], 4)["inputs"][1:], 6)
        sha_input = children(inputs["sha"][1:], 8)
        self.assertEqual(value(sha_input["required"]), "false")
        self.assertEqual(value(sha_input["default"]), '""')
        self.assertEqual(value(children(inputs["rollback"][1:], 8)["required"]), "false")
        # The takeover flag is opt-in: a routine deploy leaves the fence held.
        takeover = children(inputs["takeover"][1:], 8)
        self.assertEqual(value(takeover["required"]), "false")
        self.assertEqual(value(takeover["type"]), "boolean")
        self.assertEqual(value(takeover["default"]), "false")
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
        # Deploy/rollback, the before/after version reads, and the post-takeover re-read.
        self.assertEqual(len(wrangler), 5)
        for step in wrangler:
            self.assertIn("          environment: production", step)

    def test_concurrency_queues_instead_of_cancelling(self):
        concurrency = children(TOP["concurrency"][1:], 2)
        self.assertEqual(value(concurrency["group"]), "deploy-production")
        self.assertEqual(value(concurrency["cancel-in-progress"]), "false")

    def test_every_job_uses_the_shared_runner_routing(self):
        # scripts/test-runner-routing.py pins the full expression (TOG-12339).
        self.assertEqual(list(JOBS), ["guard", "production"])
        for name, job in JOBS.items():
            runs_on = value(children(job[1:], 4)["runs-on"])
            self.assertTrue(runs_on.startswith("${{ fromJSON((!github.event.repository.private && "), name)
            self.assertIn(f"vars.CI_OVERFLOW_JOBS || '[]'), '{name}')", runs_on)

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
        self.assertEqual(input_lines, ["SHA: ${{ inputs.sha }}", "ROLLBACK: ${{ inputs.rollback }}",
                                       "TAKEOVER: ${{ inputs.takeover }}"])
        production = "\n".join(JOBS["production"])
        self.assertNotIn("inputs.", production)
        self.assertIn("ref: ${{ needs.guard.outputs.sha }}", production)
        self.assertIn("persist-credentials: false", production)
        self.assertIn('git merge-base --is-ancestor "$SHA" origin/main', production)
        self.assertIn("command: deploy --config ${{ env.PRODUCTION_DEPLOY_CONFIG }} --env production --message ${{ needs.guard.outputs.sha }}", production)
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

    def test_pre_freeze_candidate_requires_the_full_latest_ci_verdict(self):
        row = next(line for line in (ROOT / "docs/cutover-sequence.md").read_text().splitlines()
                   if line.startswith("| 0.1 |"))
        for gate in ("latest `check.yml` run/current attempt", "full `ci-ok` verdict",
                     "Rust/DB lanes", "`worker check`", "`pr-lint`", "`gitleaks`",
                     "on that exact head", "lint-only `check` is insufficient"):
            self.assertIn(gate, row)
        self.assertNotIn("`check` (`fmt`, `clippy -D warnings`, tests", row)

    def test_run_scripts_never_interpolate_expressions(self):
        blocks = run_blocks()
        self.assertGreaterEqual(len(blocks), 6)
        for block in blocks:
            self.assertNotIn("${{", block)

    def test_build_identity_comes_from_the_guarded_sha_and_run(self):
        production = "\n".join(JOBS["production"])
        self.assertNotIn("GITHUB_SHA", production)
        self.assertNotIn("github.sha", production)
        self.assertIn('--sha "$SHA" --run-id "$GITHUB_RUN_ID" --attempt "$GITHUB_RUN_ATTEMPT"', RENDER)
        self.assertIn('echo "PRODUCTION_DEPLOY_CONFIG=$config" >> "$GITHUB_ENV"', RENDER)
        names = [step[0].strip() for step in steps(JOBS["production"])]
        self.assertLess(names.index(f"- name: {RENDER_STEP}"), names.index("- name: Deploy to production"))

    def test_readyz_gate_judges_the_guarded_sha_and_mode_without_secrets(self):
        self.assertIn('production_deploy.py readyz --status "$code" --body "$readyz" --sha "$SHA" '
                      '--mode "$MODE" --build-id "$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT" 2>&1',
                      GATE)
        for text in (RENDER, GATE):
            self.assertNotIn("secrets.", text)
            self.assertNotIn("token", text.lower())

    def test_takeover_steps_run_only_when_requested_with_production_bindings(self):
        names = [step[0].strip() for step in steps(JOBS["production"])]
        preflight = "- name: Require production ownership-control configuration"
        status = "- name: Read production ownership state without starting"
        takeover = "- name: Take over production ownership at the read epoch"
        reread = "- name: Re-read the active production version after takeover"
        confirm = "- name: Confirm the taken-over version still serves"
        for name in (preflight, status, takeover, reread, confirm):
            self.assertIn(name, names)
        by_name = {step[0].strip(): step for step in steps(JOBS["production"])}
        for name in (preflight, status, takeover, reread, confirm):
            conditions = [line.strip() for line in by_name[name] if line.strip().startswith("if:")]
            self.assertEqual(conditions, ["if: env.TAKEOVER == 'true'"], name)
        text = "\n".join(sum((by_name[name] for name in (preflight, status, takeover)), []))
        # Production bindings only: the control token comes from the
        # production Environment secret, the URL from its variable.
        self.assertIn("OWNERSHIP_CONTROL_TOKEN: ${{ secrets.PRODUCTION_OWNERSHIP_CONTROL_TOKEN }}", text)
        self.assertIn("PRODUCTION_WORKER_URL: ${{ vars.PRODUCTION_WORKER_URL }}", text)
        self.assertIn("STAGING_WORKER_URL: ${{ vars.STAGING_WORKER_URL }}", text)
        self.assertNotIn("STAGING_OWNERSHIP_CONTROL_TOKEN", text)
        self.assertIn("node scripts/production-ownership-control.mjs preflight", text)
        self.assertIn("node scripts/production-ownership-control.mjs status", text)
        self.assertIn('node scripts/production-ownership-control.mjs takeover "$epoch"', text)
        # The takeover posts exactly the P2 epoch with an explicit release and
        # a run-derived audit actor naming the guarded SHA, never GITHUB_SHA.
        self.assertIn('OWNERSHIP_RELEASE_FENCE: "true"', text)
        self.assertIn("OWNERSHIP_ACTOR: github-actions:${{ github.run_id }}:${{ needs.guard.outputs.sha }}", text)
        self.assertIn("OWNERSHIP_EXPECTED_DEPLOYMENT: ${{ env.NEW_VERSION }}", text)
        production = "\n".join(JOBS["production"])
        self.assertNotIn("inputs.takeover", production)
        self.assertIn("TAKEOVER: ${{ needs.guard.outputs.takeover }}", production)


class GuardBehaviourTests(unittest.TestCase):
    def guard(self, sha=SHA, rollback="", takeover="false", ref="refs/heads/main", auto="", **responses):
        api = {**green(), "staging-latest": {"total_count": 1, "workflow_runs": [staging_run()]}, **responses}
        calls = []
        resolved = sha.strip().lower() or (SHA if not rollback else "")

        def fake_check_output(args, **kwargs):
            self.assertEqual(args[:4], ["gh", "api", "--method", "GET"])
            self.assertTrue(all(flag == "-f" for flag in args[5::2]), args)
            path, query = args[4], dict(arg.split("=", 1) for arg in args[6::2])
            calls.append((path, query))
            if path == f"repos/{REPO}/compare/{resolved}...main":
                key = "compare"
            elif path == f"repos/{REPO}/commits/{resolved}/check-runs":
                self.assertEqual(query["app_id"], "15368")
                self.assertEqual(query["filter"], "latest")
                key = query["check_name"]
            elif path == f"repos/{REPO}/actions/workflows/check.yml/runs":
                self.assertEqual(query["head_sha"], resolved)
                self.assertNotIn("status", query)
                self.assertNotIn("branch", query)
                key = "ci-runs"
            elif re.fullmatch(rf"repos/{REPO}/actions/runs/[0-9]+", path):
                key = "ci-run"
            elif re.fullmatch(rf"repos/{REPO}/actions/runs/[0-9]+/attempts/[0-9]+/jobs", path):
                key = "ci-jobs"
            elif path == f"repos/{REPO}/actions/workflows/deploy-staging.yml/runs" and "head_sha" not in query:
                # Promotion: the latest successful staging deploy on main.
                self.assertEqual(
                    {k: query[k] for k in ("branch", "status", "per_page")},
                    {"branch": "main", "status": "success", "per_page": "1"},
                )
                key = "staging-latest"
            elif path == f"repos/{REPO}/actions/workflows/deploy-staging.yml/runs" and "status" not in query:
                # Automated approval: every run on the SHA, never a success-only filter.
                self.assertEqual(
                    {k: query[k] for k in ("head_sha", "branch")},
                    {"head_sha": resolved, "branch": "main"},
                )
                key = "staging-all"
            elif path == f"repos/{REPO}/actions/workflows/deploy-staging.yml/runs":
                self.assertEqual(
                    {k: query[k] for k in ("head_sha", "branch", "status")},
                    {"head_sha": resolved, "branch": "main", "status": "success"},
                )
                key = "staging"
            elif path == f"repos/{REPO}/environments/production":
                key = "environment"
            elif path == f"repos/{REPO}/environments/production/deployment-branch-policies":
                key = "branches"
            else:
                self.fail(f"unexpected API call {path}")
            result = api[key]
            if callable(result):
                result = result(path, query)
            if isinstance(result, Exception):
                raise result
            if path.endswith("/check-runs") and isinstance(result, dict):
                # Match the GitHub API's name/App filters, but never filter by
                # status: a pending latest run must not disappear behind green.
                self.assertNotIn("status", query)
                result = {"check_runs": [
                    run for run in result.get("check_runs", [])
                    if run.get("name") == key and str(run.get("app", {}).get("id")) == query["app_id"]
                ]}
            return result if isinstance(result, str) else json.dumps(result)

        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            output, summary = Path(tmp) / "output", Path(tmp) / "summary"
            env = {
                "GITHUB_REPOSITORY": REPO, "GITHUB_REF": ref, "SHA": sha, "ROLLBACK": rollback,
                "TAKEOVER": takeover, "AUTO_APPROVE": auto,
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
        self.assertEqual(outputs, {"sha": SHA, "mode": "deploy", "version": "", "takeover": "false"})
        self.assertIn("Takeover: not requested", summary)
        self.assertIn(SHA, summary)
        self.assertIn("full `ci-ok` verdict", summary)
        self.assertIn("green ci-ok", "\n".join(TOP["on"]))
        self.assertEqual(
            [query.get("check_name") for path, query in calls if path.endswith("/check-runs")],
            ["ci-ok", "worker check"],
        )

    def test_main_head_itself_passes_and_sha_is_normalised(self):
        code, outputs, _, _, _ = self.guard(sha=f" {SHA.upper()}\n", compare={"status": "identical"})
        self.assertEqual(code, 0)
        self.assertEqual(outputs["sha"], SHA)

    def test_empty_sha_promotes_the_latest_successful_staging_deploy(self):
        code, outputs, _, calls, stdout = self.guard(sha="")
        self.assertEqual(code, 0)
        self.assertEqual(outputs["sha"], SHA)
        self.assertIn("Promoting the latest successful staging deploy", stdout)
        self.assertEqual(calls[0][0], f"repos/{REPO}/actions/workflows/deploy-staging.yml/runs")

    def test_rollback_with_empty_sha_is_refused_never_promoted(self):
        # During an incident the latest staged SHA is usually the faulty build:
        # a rollback must always name its commit explicitly.
        result = self.guard(sha="", rollback=VERSION)
        self.assertRefused(result, "40-character hex")
        self.assertEqual(result[3], [])

    def test_runbook_documents_promotion_and_explicit_sha_for_recovery(self):
        runbook = (ROOT / "docs/production-deploy.md").read_text()
        self.assertIn("Leave `sha` empty to promote", runbook)
        self.assertIn("always set `sha` explicitly", runbook)

    def test_empty_sha_without_any_staging_success_is_refused(self):
        self.assertRefused(
            self.guard(sha="", **{"staging-latest": {"total_count": 0, "workflow_runs": []}}),
            "no successful deploy-staging run on main to promote",
        )

    def test_promoted_sha_still_needs_its_own_green_gates(self):
        self.assertRefused(
            self.guard(sha="", compare={"status": "diverged"}),
            "is not an ancestor of origin/main",
        )

    def test_malformed_sha_is_refused_before_any_api_call(self):
        for sha in ("main", SHA[:39], SHA + "0", "g" * 40, "$(id)", f"{SHA};id", f"{SHA[:20]} {SHA[20:]}"):
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
        self.assertEqual(outputs, {"sha": SHA, "mode": "rollback", "version": VERSION, "takeover": "false"})
        self.assertIn(VERSION, summary)

    def test_takeover_flag_is_validated_before_any_api_call(self):
        for flag in ("", "yes", "True", "1", " true", "takeover"):
            with self.subTest(flag=flag):
                result = self.guard(takeover=flag)
                self.assertRefused(result, "takeover must be true or false")
                self.assertEqual(result[3], [])
        code, outputs, summary, _, _ = self.guard(takeover="true")
        self.assertEqual(code, 0)
        self.assertEqual(outputs["takeover"], "true")
        self.assertIn("Takeover: requested", summary)

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
        for name in ("ci-ok", "worker check"):
            for runs in ([], [check_run(name, conclusion="failure")],
                         [check_run(name, status="queued", conclusion=None)],
                         [check_run(name), check_run(name, conclusion="cancelled")],
                         [check_run(name, conclusion="skipped")]):
                with self.subTest(name=name, runs=runs):
                    self.assertRefused(
                        self.guard(**{name: {"check_runs": runs}}), f"`{name}` has no successful completed run"
                    )

    def test_incomplete_full_ci_verdict_is_refused_even_with_green_lint_worker_and_staging(self):
        # Lint and worker success cannot vouch for the moved Rust/DB lanes.
        bad_runs = [[]]
        bad_runs += [[check_run("ci-ok", conclusion=conclusion)] for conclusion in (
            "failure", "cancelled", "skipped", "neutral", "timed_out", "action_required", "stale", None,
        )]
        bad_runs += [[check_run("ci-ok", status=status, conclusion=conclusion)]
                     for status in ("queued", "in_progress", None)
                     for conclusion in (None, "success")]
        for rollback in ("", VERSION):
            for runs in bad_runs:
                with self.subTest(rollback=bool(rollback), runs=runs):
                    self.assertRefused(
                        self.guard(rollback=rollback, **{"ci-ok": {"check_runs": runs}}),
                        "`ci-ok` has no successful completed run",
                    )

    def test_latest_pending_or_red_verdict_cannot_be_hidden_by_an_older_green_run(self):
        for latest in (check_run("ci-ok", status="in_progress", conclusion=None),
                       check_run("ci-ok", conclusion="failure")):
            with self.subTest(latest=latest):
                # The API may return latest runs from more than one check suite.
                self.assertRefused(
                    self.guard(**{"ci-ok": {"check_runs": [check_run("ci-ok"), latest]}}),
                    "`ci-ok` has no successful completed run",
                )

    def test_old_green_cannot_hide_a_new_run_without_an_aggregate(self):
        for rollback in ("", VERSION):
            for status, conclusion in (("queued", None), ("in_progress", None),
                                       ("in_progress", "failure"), ("completed", "failure"),
                                       ("completed", "cancelled"), ("completed", "skipped")):
                with self.subTest(rollback=bool(rollback), status=status, conclusion=conclusion):
                    newer = workflow_run(run_id=102, number=2, status=status, conclusion=conclusion)
                    # The name-filtered aggregate, worker and staging remain green.
                    self.assertRefused(self.guard(rollback=rollback, **{
                        "ci-runs": {"total_count": 2, "workflow_runs": [workflow_run(), newer]},
                        "ci-run": newer,
                        "ci-jobs": {"total_count": 1, "jobs": [workflow_job("worker check", run_id=102)]},
                    }), "check.yml latest run/attempt has no successful completed verdict")

    def test_old_green_cannot_hide_a_new_attempt_without_an_aggregate(self):
        for rollback in ("", VERSION):
            for status, conclusion in (("queued", None), ("in_progress", None),
                                       ("completed", "failure"), ("completed", "cancelled")):
                with self.subTest(rollback=bool(rollback), status=status):
                    # The list response can lag; refresh the current attempt by run ID.
                    self.assertRefused(self.guard(rollback=rollback, **{
                        "ci-run": workflow_run(attempt=2, status=status, conclusion=conclusion),
                    }), "check.yml latest run/attempt has no successful completed verdict")

    def test_latest_green_run_and_attempt_pass_in_both_modes(self):
        latest = workflow_run(run_id=102, number=2, attempt=2, suite_id=202)
        for rollback in ("", VERSION):
            with self.subTest(rollback=bool(rollback)):
                result = self.guard(rollback=rollback, **{
                    # Deliberately newest-first: do not rely on response ordering.
                    "ci-runs": {"total_count": 2, "workflow_runs": [latest, workflow_run()]},
                    "ci-run": latest,
                    "ci-jobs": {"total_count": 2, "jobs": [
                        workflow_job(name, run_id=102, attempt=2, check_id=CHECK_IDS[name] + 100)
                        for name in ("ci-ok", "worker check")
                    ]},
                    **{name: {"check_runs": [check_run(name), check_run(
                        name, check_id=CHECK_IDS[name] + 100, suite_id=202,
                    )]} for name in ("ci-ok", "worker check")},
                })
                self.assertEqual(result[0], 0)
                self.assertEqual(result[1]["mode"], "rollback" if rollback else "deploy")
                self.assertIn("run `102`, attempt `2` (latest, completed/success)", result[2])
                self.assertIn((f"repos/{REPO}/actions/runs/102/attempts/2/jobs",
                               {"per_page": "100", "page": "1"}), result[3])

    def test_old_check_evidence_cannot_stand_in_for_the_latest_attempt(self):
        for rollback in ("", VERSION):
            for name in ("ci-ok", "worker check"):
                for key, bad in (("run_id", 100), ("run_attempt", 1), ("head_sha", "f" * 40),
                                 ("status", "in_progress"), ("conclusion", "failure"),
                                 ("check_run_url", None)):
                    with self.subTest(rollback=bool(rollback), name=name, key=key):
                        jobs = [workflow_job(n, attempt=2) for n in ("ci-ok", "worker check")]
                        next(job for job in jobs if job["name"] == name)[key] = bad
                        self.assertRefused(self.guard(rollback=rollback, **{
                            "ci-run": workflow_run(attempt=2),
                            "ci-jobs": {"total_count": 2, "jobs": jobs},
                        }), f"`{name}` has no successful completed job")
                for key, bad in (("url", "https://api.github.com/repos/other/repo/check-runs/1"),
                                 ("head_sha", "f" * 40), ("check_suite", {"id": 999})):
                    with self.subTest(rollback=bool(rollback), name=name, key=key):
                        check = {**check_run(name), key: bad}
                        self.assertRefused(self.guard(rollback=rollback, **{
                            name: {"check_runs": [check]},
                        }), f"`{name}` has no successful completed run")

    def test_missing_or_duplicate_required_attempt_jobs_are_refused(self):
        for name in ("ci-ok", "worker check"):
            other = "worker check" if name == "ci-ok" else "ci-ok"
            for selected in ([], [workflow_job(name), workflow_job(name)]):
                with self.subTest(name=name, count=len(selected)):
                    jobs = [workflow_job(other), *selected]
                    self.assertRefused(self.guard(**{
                        "ci-jobs": {"total_count": len(jobs), "jobs": jobs},
                    }), f"`{name}` has no successful completed job")

    def test_completed_run_without_current_aggregate_evidence_is_refused(self):
        for name in ("ci-ok", "worker check"):
            with self.subTest(name=name):
                # Run status alone is not evidence: only the old check URL exists.
                jobs = [workflow_job(n, check_id=CHECK_IDS[n] + (100 if n == name else 0))
                        for n in ("ci-ok", "worker check")]
                self.assertRefused(self.guard(**{
                    "ci-jobs": {"total_count": 2, "jobs": jobs},
                }), f"`{name}` has no successful completed run")

    def test_run_metadata_and_incomplete_enumeration_fail_closed(self):
        for runs in ({"total_count": 0, "workflow_runs": []},
                     {"total_count": 2, "workflow_runs": []},
                     {"total_count": 1001, "workflow_runs": [workflow_run()]},
                     {"workflow_runs": [workflow_run()]},
                     {"total_count": 1, "workflow_runs": [{**workflow_run(), "head_sha": "f" * 40}]},
                     {"total_count": 1, "workflow_runs": [{**workflow_run(), "run_number": None}]}):
            with self.subTest(runs=runs):
                self.assertRefused(self.guard(**{"ci-runs": runs}), "check.yml")
        for key, bad in (("id", 102), ("head_sha", "f" * 40), ("run_number", 2),
                         ("run_attempt", None), ("check_suite_id", None)):
            with self.subTest(key=key):
                self.assertRefused(self.guard(**{
                    "ci-run": {**workflow_run(), key: bad},
                }), "could not verify the latest check.yml attempt")
        self.assertRefused(self.guard(**{
            "ci-jobs": {"total_count": 2, "jobs": []},
        }), "could not enumerate")

    def test_pagination_does_not_hide_a_newer_run_or_required_job(self):
        newer = workflow_run(run_id=102, number=2, status="queued", conclusion=None)
        self.assertRefused(self.guard(**{
            "ci-runs": lambda path, query: {"total_count": 2, "workflow_runs": [
                workflow_run() if query["page"] == "1" else newer,
            ]}, "ci-run": newer,
        }), "check.yml latest run/attempt has no successful completed verdict")
        result = self.guard(**{
            "ci-jobs": lambda path, query: {"total_count": 2, "jobs": [
                workflow_job("ci-ok" if query["page"] == "1" else "worker check"),
            ]},
        })
        self.assertEqual(result[0], 0)

    def test_new_run_or_attempt_during_guard_read_is_refused(self):
        for rerun in (False, True):
            with self.subTest(rerun=rerun):
                reads = iter([workflow_run(), workflow_run(
                    run_id=CI_RUN_ID if rerun else 102, number=1 if rerun else 2,
                    attempt=2 if rerun else 1,
                )])
                if rerun:
                    responses = {"ci-run": lambda path, query: next(reads)}
                else:
                    lists = iter([green()["ci-runs"], {"total_count": 1, "workflow_runs": [workflow_run(
                        run_id=102, number=2,
                    )]}])
                    responses = {"ci-run": lambda path, query: next(reads),
                                 "ci-runs": lambda path, query: next(lists)}
                self.assertRefused(self.guard(**responses), "latest run/attempt changed during validation")

    def test_another_apps_green_verdict_cannot_authorize_production(self):
        untrusted = check_run("ci-ok", app_id=12345)
        for trusted in ([], [check_run("ci-ok", conclusion="failure")]):
            with self.subTest(trusted=trusted):
                self.assertRefused(
                    self.guard(**{"ci-ok": {"check_runs": [untrusted, *trusted]}}),
                    "`ci-ok` has no successful completed run",
                )

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

    def test_auto_approve_passes_without_reviewers_when_latest_staging_deploy_is_green(self):
        environment = {**green()["environment"], "protection_rules": [{"type": "branch_policy"}]}
        code, outputs, summary, calls, _ = self.guard(auto="true", environment=environment)
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"sha": SHA, "mode": "deploy", "version": "", "takeover": "false"})
        self.assertIn("Approval: automated", summary)
        self.assertIn("deploy-staging run `501`", summary)
        self.assertTrue(any(path.endswith("deploy-staging.yml/runs") and "status" not in q for path, q in calls))

    def test_auto_approve_refuses_unless_the_latest_staging_deploy_succeeded(self):
        environment = {**green()["environment"], "protection_rules": []}
        for runs, reason in (
            ([], "no verifiable latest run"),
            ([staging_run(conclusion="failure")], "not a completed success"),
            ([staging_run(), staging_run(run_id=502, number=6, conclusion="failure")], "not a completed success"),
            ([staging_run(), staging_run(run_id=502, number=6, status="in_progress", conclusion=None)],
             "not a completed success"),
            ([staging_run(sha="f" * 40)], "no verifiable latest run"),
        ):
            with self.subTest(runs=runs):
                result = self.guard(auto="true", environment=environment,
                                    **{"staging-all": {"total_count": len(runs), "workflow_runs": runs}})
                self.assertRefused(result, reason)

    def test_auto_approve_keeps_every_other_guard(self):
        environment = {**green()["environment"], "protection_rules": []}
        self.assertRefused(self.guard(auto="true", environment=environment,
                                      staging={"total_count": 0, "workflow_runs": []}),
                           "deploy-staging has no successful run")
        self.assertRefused(self.guard(auto="true", environment=environment, compare={"status": "behind"}),
                           "not an ancestor of origin/main")
        self.assertRefused(self.guard(auto="true", ref="refs/heads/feature"), "dispatch this workflow from main only")
        policy = {**environment, "deployment_branch_policy": None}
        self.assertRefused(self.guard(auto="true", environment=policy), "restrict deployments to main")
        self.assertRefused(self.guard(auto="true", branches={"branch_policies": [{"name": "*", "type": "branch"}]}),
                           "from main only")

    def test_reviewers_stay_required_unless_auto_approve_is_exactly_true(self):
        environment = {**green()["environment"], "protection_rules": []}
        for flag in ("", "false", "TRUE", "1", " true"):
            with self.subTest(flag=flag):
                self.assertRefused(self.guard(auto=flag, environment=environment), "no required reviewers")

    def test_api_failures_fail_closed(self):
        for key in ("compare", "ci-runs", "ci-run", "ci-jobs", "ci-ok", "worker check", "staging", "branches"):
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
    def record(self, mode="deploy", before="", after="", target="", takeover="false"):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            summary = Path(tmp) / "summary"
            github_env = Path(tmp) / "github_env"
            env = {
                "SHA": SHA, "MODE": mode, "TARGET_VERSION": target, "TAKEOVER": takeover,
                "BEFORE": before, "AFTER": after,
                "GITHUB_STEP_SUMMARY": str(summary), "GITHUB_ENV": str(github_env),
            }
            stdout, code = io.StringIO(), 0
            with patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(stdout):
                try:
                    exec(RECORD, {})
                except SystemExit as exit_:
                    code = exit_.code
            text = summary.read_text()
            self.assertNotIn("deployer@example.com", text + stdout.getvalue())
            exported = dict(
                line.split("=", 1) for line in
                (github_env.read_text().splitlines() if github_env.exists() else [])
            )
            return code, text, exported

    def test_deploy_records_sha_new_version_and_rollback_target(self):
        code, summary, exported = self.record(
            before=deployment((OLD_VERSION, 100)), after=f"⛅️ wrangler 4\n{deployment((VERSION, 100))}\n"
        )
        self.assertEqual(code, 0)
        self.assertIn(f"- SHA: `{SHA}`", summary)
        self.assertIn(f"now active: `{VERSION}` (100%)", summary)
        self.assertIn(f"Previously active: `{OLD_VERSION}` (100%)", summary)
        self.assertIn(f"`rollback={OLD_VERSION}`", summary)
        self.assertEqual(exported, {})

    def test_first_deploy_has_no_previous_version(self):
        code, summary, _ = self.record(before="", after=deployment((VERSION, 100)))
        self.assertEqual(code, 0)
        self.assertIn("Previously active: none", summary)
        self.assertNotIn("rollback=", summary)

    def test_deploy_that_changed_nothing_fails(self):
        same = deployment((OLD_VERSION, 100))
        self.assertEqual(self.record(before=same, after=same)[0], 1)

    def test_unreadable_new_version_fails_but_still_records(self):
        for after in ("", "X [ERROR] Authentication error", "{not json}", json.dumps({"versions": "?"})):
            with self.subTest(after=after):
                code, summary, _ = self.record(before=deployment((OLD_VERSION, 100)), after=after)
                self.assertEqual(code, 1)
                self.assertIn(f"`rollback={OLD_VERSION}`", summary)

    def test_rollback_must_leave_only_the_target_serving(self):
        before = deployment((OLD_VERSION, 100))
        self.assertEqual(self.record("rollback", before, deployment((VERSION, 100)), VERSION)[0], 0)
        self.assertEqual(self.record("rollback", before, before, VERSION)[0], 1)
        split = deployment((VERSION, 50), (OLD_VERSION, 50))
        self.assertEqual(self.record("rollback", before, split, VERSION)[0], 1)

    def test_takeover_exports_the_single_serving_version(self):
        code, _, exported = self.record(
            before=deployment((OLD_VERSION, 100)), after=deployment((VERSION, 100)),
            takeover="true",
        )
        self.assertEqual(code, 0)
        self.assertEqual(exported, {"NEW_VERSION": VERSION})

    def test_takeover_refuses_without_one_version_at_full_traffic(self):
        split = deployment((VERSION, 50), (OLD_VERSION, 50))
        for after in ("", split):
            with self.subTest(after=after[:40]):
                code, _, exported = self.record(
                    before=deployment((OLD_VERSION, 100)), after=after, takeover="true")
                self.assertEqual(code, 1)
                self.assertEqual(exported, {})


class ConfirmStepTests(unittest.TestCase):
    def confirm(self, reread="", new_version=VERSION):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            summary = Path(tmp) / "summary"
            env = {"REREAD": reread, "NEW_VERSION": new_version,
                   "GITHUB_STEP_SUMMARY": str(summary)}
            stdout, code = io.StringIO(), 0
            with patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(stdout):
                try:
                    exec(CONFIRM, {})
                except SystemExit as exit_:
                    code = exit_.code
            return code, summary.read_text() if summary.exists() else ""

    def test_matching_version_confirms(self):
        code, summary = self.confirm(reread=f"noise\n{deployment((VERSION, 100))}\n")
        self.assertEqual(code, 0)
        self.assertIn(f"Takeover confirmed: `{VERSION}` still serves 100%", summary)

    def test_anything_else_fails_closed(self):
        split = deployment((VERSION, 50), (OLD_VERSION, 50))
        for reread, new_version in (
            ("", VERSION), ("not json", VERSION), (deployment((OLD_VERSION, 100)), VERSION),
            (split, VERSION), (deployment((VERSION, 100)), ""), (deployment((VERSION, 100)), OLD_VERSION),
        ):
            with self.subTest(reread=reread[:30], new_version=new_version):
                code, _ = self.confirm(reread=reread, new_version=new_version)
                self.assertEqual(code, 1)


class ScriptedWorker:
    """Loopback server answering each path with its scripted (status, body) pairs.

    The last pair repeats. Status 0 closes the connection without an answer,
    which curl reports as 000.
    """

    def __init__(self, routes):
        self.routes, self.served, self.hits = routes, {}, []
        worker = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                worker.hits.append((self.path, self.headers.get("User-Agent")))
                answers = worker.routes.get(self.path, [(404, "")])
                index = worker.served.get(self.path, 0)
                worker.served[self.path] = index + 1
                status, body = answers[min(index, len(answers) - 1)]
                if status == 0:
                    return
                payload = body.encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.server.shutdown()
        self.server.server_close()

    def count(self, path):
        return self.served.get(path, 0)


def report(revision=SHA, build_id=BUILD_ID):
    components = [["process", "ready"], ["gateway", "ready"], ["database", "ready"], ["token_invalid", "ready"]]
    return json.dumps({"components": components, "jobs": {}, "build_revision": revision, "build_id": build_id})


def differences(before, after, path=""):
    if isinstance(before, dict) and isinstance(after, dict):
        for key in sorted(set(before) | set(after)):
            if key not in before or key not in after:
                yield f"{path}/{key}"
            else:
                yield from differences(before[key], after[key], f"{path}/{key}")
    elif isinstance(before, list) and isinstance(after, list) and len(before) == len(after):
        for index, (old, new) in enumerate(zip(before, after)):
            yield from differences(old, new, f"{path}[{index}]")
    elif before != after:
        yield path


class ShellStepTests(unittest.TestCase):
    def bash(self, script, cwd=ROOT, **env):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            bin_dir = Path(tmp) / "bin"
            bin_dir.mkdir()
            (bin_dir / "sleep").write_text("#!/bin/sh\nexit 0\n")
            (bin_dir / "sleep").chmod(0o755)
            step, summary = Path(tmp) / "step.sh", Path(tmp) / "summary"
            step.write_text(script)
            run_env = {
                "PATH": f"{bin_dir}:{os.environ['PATH']}", "RUNNER_TEMP": tmp,
                "GITHUB_STEP_SUMMARY": str(summary), **env,
            }
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-eo", "pipefail", str(step)],
                cwd=cwd, env=run_env, capture_output=True, text=True, timeout=120,
            )
            return result.returncode, summary.read_text() if summary.exists() else ""

    def gate(self, routes, mode="deploy", sha=SHA, run_id="700", attempt="1"):
        with ScriptedWorker(routes) as worker:
            code, summary = self.bash(GATE, cwd=WRANGLER_DIR, PRODUCTION_URL=worker.url,
                                      MODE=mode, SHA=sha,
                                      GITHUB_RUN_ID=run_id, GITHUB_RUN_ATTEMPT=attempt)
        return code, summary, worker

    def test_matching_revision_passes_ready_or_truthfully_parked(self):
        for status, state in ((200, "gateway ready"), (503, "gateway parked")):
            with self.subTest(status=status):
                code, summary, worker = self.gate(
                    {"/health": [(200, "{}")], "/readyz": [(status, report())]})
                self.assertEqual(code, 0, summary)
                self.assertIn(f"/readyz {status} ({state}", summary)
                self.assertEqual(worker.count("/readyz"), 1)
                self.assertEqual({agent for _, agent in worker.hits}, {AGENT})

    def test_health_is_polled_through_a_cold_start(self):
        code, summary, worker = self.gate(
            {"/health": [(502, ""), (503, ""), (200, "{}")], "/readyz": [(200, report())]})
        self.assertEqual(code, 0, summary)
        self.assertEqual(worker.count("/health"), 3)

    def test_readyz_must_serve_this_revision(self):
        cases = {
            "other revision": report(revision="f" * 40),
            "unstamped build": report(revision="unknown", build_id="unknown"),
            "stale build of this revision": report(build_id="999-1"),
            "unstamped build id on this revision": report(build_id="unknown"),
            "missing build_revision": json.dumps({"components": [], "build_id": BUILD_ID}),
            "missing build_id": json.dumps({"build_revision": SHA, "components": []}),
            "non-JSON body": "<html>502 Bad Gateway</html>",
            "JSON that is not an object": "[]",
        }
        for name, body in cases.items():
            for status in (200, 503):
                with self.subTest(case=name, status=status):
                    code, summary, worker = self.gate(
                        {"/health": [(200, "{}")], "/readyz": [(status, body)]})
                    self.assertEqual(code, 1)
                    self.assertIn("roll back", summary)
                    self.assertEqual(worker.count("/readyz"), 30)

    def test_readyz_from_the_previous_build_of_this_sha_keeps_polling(self):
        # A same-SHA redeploy: the old container still serves the SHA with its
        # own build id, so the gate must not pass on the stale answer. The
        # scripted stale answer repeats, like a container that never swaps.
        code, summary, worker = self.gate(
            {"/health": [(200, "{}")], "/readyz": [(200, report(build_id="999-1"))]})
        self.assertEqual(code, 1, summary)
        self.assertIn("not this run's build", summary)
        self.assertEqual(worker.count("/readyz"), 30)

    def test_rollback_ignores_the_build_id_of_the_older_build(self):
        code, summary, _ = self.gate(
            {"/health": [(200, "{}")], "/readyz": [(200, report(build_id="999-1"))]},
            mode="rollback")
        self.assertEqual(code, 0, summary)
        self.assertIn("gateway ready", summary)

    def test_readyz_that_moves_to_this_revision_passes(self):
        code, summary, worker = self.gate({
            "/health": [(200, "{}")],
            "/readyz": [(503, report(revision="a" * 40)), (200, report())],
        })
        self.assertEqual(code, 0, summary)
        self.assertEqual(worker.count("/readyz"), 2)

    def test_readyz_without_an_answer_fails(self):
        for status in (0, 404, 500):
            with self.subTest(status=status):
                code, summary, _ = self.gate({"/health": [(200, "{}")], "/readyz": [(status, "")]})
                self.assertEqual(code, 1)
                self.assertIn("roll back", summary)

    def test_health_that_never_recovers_fails_without_probing_readyz(self):
        code, summary, worker = self.gate({"/health": [(502, "")], "/readyz": [(200, report())]})
        self.assertEqual(code, 1)
        self.assertEqual(worker.count("/health"), 30)
        self.assertEqual(worker.count("/readyz"), 0)
        self.assertIn("never 200", summary)

    def test_rollback_passes_the_supplied_revision_or_a_pre_stamp_target_only(self):
        cases = (
            (report(), "gateway ready", 0),
            (report(revision="unknown", build_id="unknown"), "pre-stamp version", 0),
            (report(revision="a" * 40), "does not match", 1),
            (report(revision="unknown", build_id=BUILD_ID), "not stamped", 1),
        )
        for body, expected, exit_code in cases:
            with self.subTest(expected=expected):
                code, summary, _ = self.gate({"/health": [(200, "{}")], "/readyz": [(200, body)]},
                                             mode="rollback")
                self.assertEqual(code, exit_code, summary)
                self.assertIn(expected, summary)

    def test_production_url_must_be_https_and_not_staging(self):
        staging = "https://staging.example.workers.dev"
        for url, ok in (("", False), ("http://prod.example.workers.dev", False), (staging, False),
                        (staging + "/", False), ("https://prod.example.workers.dev", True)):
            with self.subTest(url=url):
                code, _ = self.bash(URL_CHECK, PRODUCTION_URL=url, STAGING_URL=staging)
                self.assertEqual(code == 0, ok)


class RenderTests(unittest.TestCase):
    def render(self, config=None, sha=SHA, run_id="700", attempt="1"):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            source = WRANGLER_TOML
            if config is not None:
                source = Path(tmp) / "wrangler.toml"
                source.write_text(config)
            out = Path(tmp) / "deploy.json"
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "render", "--config", str(source), "--out", str(out),
                 "--sha", sha, "--run-id", run_id, "--attempt", attempt],
                capture_output=True, text=True, timeout=60,
            )
            return result.returncode, json.loads(out.read_text()) if out.exists() else None

    def test_production_container_gets_the_guarded_identity_only(self):
        code, rendered = self.render(attempt="2")
        self.assertEqual(code, 0)
        self.assertEqual(rendered["env"]["production"]["containers"][0]["image_vars"],
                         {"BOT_BUILD_REVISION": SHA, "BOT_BUILD_ID": "700-2"})
        self.assertNotIn("image_vars", rendered["env"]["staging"]["containers"][0])
        self.assertNotIn("image_vars", rendered["containers"][0])

    def test_render_changes_only_paths_and_the_build_arguments(self):
        code, rendered = self.render()
        self.assertEqual(code, 0)
        original = tomllib.loads(WRANGLER_TOML.read_text())
        expected = {"/main", "/containers[0]/image", "/containers[0]/image_build_context",
                    "/env/production/containers[0]/image_vars"}
        for env in ("staging", "production"):
            expected |= {f"/env/{env}/containers[0]/image", f"/env/{env}/containers[0]/image_build_context"}
        self.assertEqual(set(differences(original, rendered)), expected)
        self.assertEqual(rendered["main"], str(WRANGLER_DIR / "src/index.ts"))
        self.assertEqual(rendered["env"]["production"]["containers"][0]["image"], str(ROOT / "Dockerfile"))

    def test_identity_it_cannot_prove_is_refused_without_output(self):
        for sha, run_id, attempt in ((SHA[:39], "700", "1"), (SHA.upper(), "700", "1"),
                                     (SHA, "run", "1"), (SHA, "700", "")):
            with self.subTest(sha=sha, run_id=run_id, attempt=attempt):
                code, rendered = self.render(sha=sha, run_id=run_id, attempt=attempt)
                self.assertEqual(code, 1)
                self.assertIsNone(rendered)

    def test_other_workers_and_container_shapes_are_refused(self):
        base = WRANGLER_TOML.read_text()
        cases = {
            "other Worker": base.replace('name = "two-bot-next"', 'name = "other"', 1),
            "two production containers": base + (
                '\n[[env.production.containers]]\nclass_name = "TwoBotContainer"\n'
                'image = "../Dockerfile"\nmax_instances = 1\n'),
            "unbounded instances": base.replace(
                "max_instances = 1\n\n[env.production.containers.constraints]",
                "max_instances = 2\n\n[env.production.containers.constraints]", 1),
        }
        for name, config in cases.items():
            with self.subTest(case=name):
                code, rendered = self.render(config=config)
                self.assertEqual(code, 1)
                self.assertIsNone(rendered)


class ReadyzVerdictTests(unittest.TestCase):
    def judge(self, status="200", body=None, mode="deploy", sha=SHA, build_id=BUILD_ID):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            path = Path(tmp) / "readyz.json"
            if body is not None:
                path.write_text(body)
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "readyz", "--status", status, "--body", str(path),
                 "--sha", sha, "--mode", mode, "--build-id", build_id],
                capture_output=True, text=True, timeout=60,
            )
            return result.returncode, result.stdout.strip()

    def test_matching_revision_passes_ready_and_truthfully_parked(self):
        code, message = self.judge("200", report())
        self.assertEqual((code, message.split(",")[0]), (0, "gateway ready"))
        code, message = self.judge("503", report())
        self.assertEqual((code, message.split(",")[0]), (0, "gateway parked; deploy is healthy"))

    def test_deploy_rejects_a_stale_build_of_the_same_sha(self):
        # The previous container still serves the SHA after a same-SHA
        # redeploy or a "Re-run failed jobs": its build id is not this run's.
        for status in ("200", "503"):
            for body in (report(build_id="999-1"), report(build_id="unknown")):
                with self.subTest(status=status, body=body):
                    code, message = self.judge(status, body)
                    self.assertEqual(code, 1)
                    self.assertIn("not this run's build", message)

    def test_every_other_observation_fails_with_its_reason(self):
        cases = (
            ("200", report(revision="f" * 40), "does not match"),
            ("503", report(revision="f" * 40), "does not match"),
            ("200", report(revision="unknown", build_id="unknown"), "not stamped"),
            ("200", json.dumps({"components": [], "build_id": BUILD_ID}), "no build_revision"),
            ("200", json.dumps({"build_revision": SHA, "build_id": None}), "no build_revision"),
            ("200", "<html>502 Bad Gateway</html>", "did not return JSON"),
            ("200", None, "did not return JSON"),
            ("200", "[]", "not an object"),
            ("500", report(), "answered 500"),
            ("000", report(), "answered 000"),
            ("302", report(), "answered 302"),
        )
        for status, body, reason in cases:
            with self.subTest(status=status, reason=reason):
                code, message = self.judge(status, body)
                self.assertEqual(code, 1)
                self.assertIn(reason, message)

    def test_rollback_passes_the_supplied_revision_or_a_pre_stamp_target(self):
        self.assertEqual(self.judge("200", report(), mode="rollback")[0], 0)
        code, message = self.judge("503", report(revision="unknown", build_id="unknown"), mode="rollback")
        self.assertEqual(code, 0)
        self.assertIn("pre-stamp version", message)
        self.assertEqual(self.judge("200", report(revision="f" * 40), mode="rollback")[0], 1)
        self.assertEqual(self.judge("200", report(revision="unknown", build_id=BUILD_ID), mode="rollback")[0], 1)
        # The rolled-back version was built by an older run: its build id is
        # not this run's, and the gate must still pass on the revision.
        self.assertEqual(self.judge("200", report(build_id="999-1"), mode="rollback")[0], 0)


if __name__ == "__main__":
    unittest.main()
