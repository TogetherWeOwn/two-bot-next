"""Pin deploy-production.yml's guards and run its inline steps offline.

No GitHub or Cloudflare credentials: `gh` is faked through subprocess and
sleep through PATH. The readyz gate talks to a loopback HTTP server that
scripts each answer. Nothing here dispatches the workflow.
"""

import contextlib
import datetime
import importlib
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
import urllib.request
import zipfile
import zlib
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
DEPLOY_ID = "2026.10.21.1+0123456"
AGENT = "two-bot-next-production-rollout/1.0"
VERSION = "0b6e5e3c-1f2a-4b3c-8d4e-5f6a7b8c9d0e"
OLD_VERSION = "9f8e7d6c-5b4a-4c3d-9e2f-1a0b9c8d7e6f"
GUARD_STEP = "Refuse unverified SHA or unprotected Environment"
RENDER_STEP = "Render the production deploy config with build identity"
GATE_STEP = "Gate on stable /health + /readyz gateway-ready"
ROLLBACK_STEP = "Roll back production Worker version and container image"
AUTO_ROLLBACK_STEP = "Auto-roll back Worker version and container image"
SNAPSHOT_STEP = "Snapshot the pre-deploy rollback state"
IMAGE = "registry.cloudflare.com/acct/two-bot-next-twobotcontainer-production@sha256:" + "c" * 64
WRANGLER_ACTION = "cloudflare/wrangler-action@"
# R1 short-SHA fixtures (TOG-28737 acceptance; builder TOG-28741 must not edit
# these). SHORT_UNAMBIGUOUS is a 12-char prefix of SHA that resolves uniquely;
# AMBIGUOUS_PREFIX is a 7-char prefix shared with COLLIDING_SHA and must refuse.
# Builder contract: 7-39 hex prefixes resolve via
# `GET repos/{repo}/commits/{short}` (gh api); success continues with the full
# 40-char SHA, failure refuses with a message containing both "40-character hex"
# and ACCEPTED_FORMAT below (e.g. "sha must be a full 40-character hex commit
# SHA or an unambiguous 7-40 character hex prefix").
SHORT_UNAMBIGUOUS = SHA[:12]
AMBIGUOUS_PREFIX = SHA[:7]
COLLIDING_SHA = SHA[:7] + "f" * 33
ACCEPTED_FORMAT = "unambiguous 7-40 character hex prefix"


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
RECHECK_STEP = "Re-read the promotion hold and the live revision"
RECHECK = inline_script(RECHECK_STEP)
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


def staging_run(run_id=501, number=5, attempt=2, status="completed", conclusion="success", sha=SHA):
    # Real deploy-staging workflow_runs always carry run_attempt (int >= 1);
    # the latest run on a rerun reports attempt 2 while run_number stays
    # fixed, and the guard forwards it verbatim as staging_attempt.
    return {"id": run_id, "run_number": number, "run_attempt": attempt,
            "head_sha": sha, "status": status, "conclusion": conclusion}


SNAPSHOT_RUN_ID = 9100
SNAPSHOT_ARTIFACT_ID = 7700


def snapshot_receipt(version=VERSION, image=IMAGE, sha=SHA, complete=True):
    """The pre-deploy rollback snapshot a production deploy uploads."""
    return {"has_previous": True, "worker": "two-bot-next-production", "application": "app",
            "serving_version": version, "serving_sha": sha, "provenance_complete": complete,
            "serving_image": image}


def snapshot_zip(receipt, name="production-rollback-snapshot.json"):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        archive.writestr(name, json.dumps(receipt))
    return buffer.getvalue()


def corrupt_deflate_zip(receipt, name="production-rollback-snapshot.json"):
    """A snapshot zip whose member is corrupt deflate data.

    Unlike b"not a zip" this parses as a zip and fails inside the
    decompressor: 0xFF as the first stream byte sets the reserved block
    type, so archive.read raises zlib.error, not BadZipFile.
    """
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(name, json.dumps(receipt))
    blob = bytearray(buffer.getvalue())
    data = 30 + len(name.encode("utf-8"))  # local file header, no extra field
    blob[data] = 0xFF
    return bytes(blob)


def snapshot_artifact(artifact_id=SNAPSHOT_ARTIFACT_ID, run_id=SNAPSHOT_RUN_ID, age_days=1,
                      branch="main", head_repo=1, expired=False):
    created = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=age_days)
    return {"id": artifact_id, "name": "production-rollback-snapshot", "expired": expired,
            "created_at": created.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "workflow_run": {"id": run_id, "repository_id": 1, "head_repository_id": head_repo,
                             "head_branch": branch, "head_sha": "e" * 40}}


def snapshot_origin_run(run_id=SNAPSHOT_RUN_ID, path=".github/workflows/deploy-production.yml",
                        branch="main", event="workflow_run", repo=REPO):
    return {"id": run_id, "path": path, "head_branch": branch, "event": event,
            "repository": {"full_name": REPO}, "head_repository": {"full_name": repo}}


def snapshot_responses(receipt=None, artifact=None, run=None):
    """Guard responses holding one rollback snapshot artifact."""
    artifact = artifact or snapshot_artifact()
    return {"artifacts": {"total_count": 1, "artifacts": [artifact]},
            f"artifact-zip-{artifact['id']}": snapshot_zip(receipt or snapshot_receipt()),
            "snapshot-run": run or snapshot_origin_run()}


def failed_call():
    return subprocess.CalledProcessError(1, ["gh", "api"], "gh: Not Found (HTTP 404)")


def fake_readyz_opener(testcase, api, calls):
    """A loopback /readyz opener shared by the guard and recheck harnesses."""
    def fake_urlopen(request, timeout):
        testcase.assertIsInstance(request, urllib.request.Request)
        testcase.assertEqual(request.get_header("User-agent"), AGENT)
        calls.append(("production-readyz", {"url": request.full_url, "timeout": timeout}))
        response = api["production-readyz"]

        class ReadyResponse:
            status = response.get("status", 200)

            def __enter__(self):
                return self

            def __exit__(self, *_):
                return False

            def read(self):
                return json.dumps(response).encode()

        return ReadyResponse()
    return fake_urlopen


class StaticGuardTests(unittest.TestCase):
    def test_automatic_triggers_promote_green_staging_and_sha_defaults_to_last_staged(self):
        triggers = children(TOP["on"][1:], 2)
        self.assertEqual(list(triggers), ["workflow_run", "schedule", "workflow_dispatch"])
        # workflow_run: a completed deploy-staging run on main only. Anything
        # else (another workflow, another branch, queued/cancelled) never
        # reaches the guard.
        run_trigger = "\n".join(triggers["workflow_run"])
        # Both staging and CI completions notify: whichever finishes last on
        # the same SHA promotes (10-20 race: staging green 23 min before CI).
        self.assertIn("workflows: [deploy-staging, check]", run_trigger)
        self.assertIn("types: [completed]", run_trigger)
        self.assertIn("branches: [main]", run_trigger)
        # schedule: the daily backstop for a soaked commit the event missed.
        self.assertRegex("\n".join(triggers["schedule"]), r"(?m)^\s*-\s*cron:\s*\"\S+ \S+ \* \* \*\"")
        inputs = children(children(triggers["workflow_dispatch"][1:], 4)["inputs"][1:], 6)
        sha_input = children(inputs["sha"][1:], 8)
        self.assertEqual(value(sha_input["required"]), "false")
        self.assertEqual(value(sha_input["default"]), '""')
        self.assertEqual(value(children(inputs["rollback"][1:], 8)["required"]), "false")
        self.assertEqual(value(children(inputs["image"][1:], 8)["required"]), "false")
        self.assertEqual(value(children(inputs["image"][1:], 8)["default"]), '""')
        # A deploy always takes over (10-20 13:31Z: a manual deploy without
        # takeover left production fenced). The takeover flag defaults on;
        # only the explicit, opt-in cutover_hold keeps the fence held.
        takeover = children(inputs["takeover"][1:], 8)
        self.assertEqual(value(takeover["required"]), "false")
        self.assertEqual(value(takeover["type"]), "boolean")
        self.assertEqual(value(takeover["default"]), "true")
        hold = children(inputs["cutover_hold"][1:], 8)
        self.assertEqual(value(hold["required"]), "false")
        self.assertEqual(value(hold["type"]), "boolean")
        self.assertEqual(value(hold["default"]), "false")
        self.assertIn("fence stays held", value(hold["description"]))
        # allow_unstaged re-serves the live SHA when staging history aged out.
        unstaged = children(inputs["allow_unstaged"][1:], 8)
        self.assertEqual(value(unstaged["required"]), "false")
        self.assertEqual(value(unstaged["type"]), "boolean")
        self.assertEqual(value(unstaged["default"]), "false")
        # Unknown inputs are rejected by GitHub before a production run.
        self.assertEqual(set(inputs), {"sha", "rollback", "image", "allow_unstaged", "takeover", "cutover_hold"})
        self.assertNotIn("inject_error_rate_spike", "\n".join(LINES))
        self.assertNotIn("INJECT_ERROR_RATE_SPIKE", "\n".join(LINES))
        self.assertNotIn("inject_gateway_failure", "\n".join(LINES))
        self.assertNotIn("--rehearsal-firing", "\n".join(LINES))
        text = "\n".join(TOP["on"])
        for trigger in ("push", "pull_request", "pull_request_target", "workflow_call"):
            self.assertNotRegex(text, rf"(?m)^\s*{trigger}:", trigger)

    def test_wrangler_and_secrets_only_run_in_the_production_environment(self):
        production = children(JOBS["production"][1:], 4)
        self.assertEqual(value(production["environment"]), "production")
        # Drift must block the deploy: the production job waits for the guard,
        # the prod schema gate and the prod roles gate. A rollback serves no
        # new schema and waives the ledger check inside the reviewed gate
        # script, never here.
        self.assertEqual(value(production["needs"]), "[guard, soak, recheck, schema-gate, roles-gate]")
        for gate in ("schema-gate", "roles-gate"):
            condition = value(children(JOBS[gate][1:], 4)["if"])
            self.assertEqual(condition, "needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true'"
                                        " && needs.guard.outputs.skip == ''")
        production_if = value(children(JOBS["production"][1:], 4)["if"])
        self.assertIn("needs.guard.outputs.mode == 'rollback'", production_if)
        self.assertIn("needs.schema-gate.result == 'success' && needs.roles-gate.result == 'success'", production_if)
        guard = children(JOBS["guard"][1:], 4)
        self.assertNotIn("environment", guard)
        self.assertNotIn("secrets.", "\n".join(JOBS["guard"]))
        wrangler = [s for s in steps(JOBS["production"]) if WRANGLER_ACTION in "\n".join(s)]
        # Deploy, the before/after version reads, and the post-takeover
        # re-read. Rollback mode restores Worker + image through the reviewed
        # script, not `wrangler rollback`, so the image rolls back too.
        self.assertEqual(len(wrangler), 4)
        for step in wrangler:
            self.assertIn("          environment: production", step)

    def test_concurrency_queues_instead_of_cancelling(self):
        # Job-level concurrency on production only: the guard, gates, soak
        # and recheck run outside the lane, so an automatic run can never
        # delay an operator rollback behind a sleeping promotion. The lane
        # keeps the default single pending slot (a third arrival cancels the
        # queued run), so the recheck skips an automatic run while an operator
        # dispatch is active instead of entering the lane behind it. Running
        # jobs are never cancelled (cancel-in-progress is false).
        self.assertNotIn("concurrency", TOP)
        conc = children(JOBS["production"][1:], 4).get("concurrency", [])
        self.assertIn("group: deploy-production", "\n".join(conc))
        self.assertIn("cancel-in-progress: false", "\n".join(conc))
        for job in ("guard", "schema-gate", "roles-gate", "soak", "recheck"):
            self.assertNotIn("concurrency", children(JOBS[job][1:], 4))

    def test_every_job_uses_the_shared_runner_routing(self):
        # scripts/test-runner-routing.py pins the full expression (TOG-12339).
        self.assertEqual(list(JOBS), ["guard", "schema-gate", "roles-gate", "soak", "recheck",
                                      "production"])
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
            if name == "production":
                # Checkout plus the deploy-ID lifecycle: count today's
                # production Deployments for N, then record this one; plus
                # read-only runs access to fetch the exact immutable staging
                # artifact by ID for same-SHA promotion.
                self.assertEqual(granted, {"contents": "read", "actions": "read", "deployments": "write"})
                continue
            self.assertEqual(set(granted.values()), {"read"}, name)
        self.assertEqual(
            set(children(children(JOBS["production"][1:], 4)["permissions"][1:], 6)),
            {"contents", "actions", "deployments"},
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
                                       "IMAGE: ${{ inputs.image }}",
                                       "ALLOW_UNSTAGED: ${{ inputs.allow_unstaged }}",
                                       "TAKEOVER: ${{ inputs.takeover }}",
                                       "CUTOVER_HOLD: ${{ inputs.cutover_hold }}"])
        production = "\n".join(JOBS["production"])
        # No production step reads any raw input or fault switch.
        self.assertEqual([line for line in production.splitlines() if "inputs." in line], [])
        self.assertNotIn("INJECT_ERROR_RATE_SPIKE", production)
        self.assertNotIn("--rehearsal-firing", production)
        self.assertIn("ref: ${{ needs.guard.outputs.sha }}", production)
        self.assertIn("persist-credentials: false", production)
        self.assertIn('git merge-base --is-ancestor "$SHA" origin/main', production)
        self.assertIn("command: deploy --config ${{ env.PRODUCTION_DEPLOY_CONFIG }} --env production --message ${{ needs.guard.outputs.sha }}", production)
        # No `wrangler rollback`: a Worker-only rollback leaves the new image
        # serving. Rollback mode restores both through the reviewed script.
        self.assertNotIn("wrangler rollback", production)
        self.assertNotIn("command: rollback", production)
        rollback = inline_script(ROLLBACK_STEP)
        self.assertIn("worker_container_rollback.py rollback", rollback)
        self.assertIn('--target-version "$TARGET_VERSION"', rollback)
        self.assertIn('--target-image "$TARGET_IMAGE"', rollback)
        self.assertIn("--variant production", rollback)
        conditions = {
            step[0]: [line.strip() for line in step if line.strip().startswith("if:")]
            for step in steps(JOBS["production"])
        }
        self.assertEqual(conditions["      - name: Deploy to production"], ["if: env.MODE == 'deploy'"])
        self.assertEqual(conditions[f"      - name: {ROLLBACK_STEP}"], ["if: env.MODE == 'rollback'"])

    def test_snapshot_and_auto_rollback_share_the_reviewed_script(self):
        snapshot = inline_script(SNAPSHOT_STEP)
        self.assertIn("worker_container_rollback.py snapshot", snapshot)
        self.assertIn("--variant production", snapshot)
        auto = inline_script(AUTO_ROLLBACK_STEP)
        self.assertIn("worker_container_rollback.py rollback", auto)
        self.assertIn("--snapshot", auto)
        self.assertIn("--variant production", auto)
        # The snapshot restores the previous build, whose /readyz reports
        # the previous revision: gating it against the failed SHA could
        # never pass, so the snapshot rollback carries no --expected-sha.
        self.assertNotIn("--expected-sha", auto)
        # A manual rollback names the restored version's commit as the SHA,
        # so its gate still checks the revision.
        self.assertIn('--expected-sha "$SHA"', inline_script(ROLLBACK_STEP))
        conditions = {
            step[0]: [line.strip() for line in step if line.strip().startswith("if:")]
            for step in steps(JOBS["production"])
        }
        # Auto-rollback only after a takeover deploy actually replaced
        # something: the run still fails red, but service is restored first.
        # The record step runs after it, so bookkeeping can never gate
        # recovery (pinned by test_workflows.py).
        self.assertEqual(
            conditions[f"      - name: {AUTO_ROLLBACK_STEP}"],
            ["if: failure() && env.MODE == 'deploy' && env.TAKEOVER == 'true' "
             "&& steps.deploy.outcome == 'success' && steps.snapshot.outcome == 'success'"],
        )
        production = "\n".join(JOBS["production"])
        self.assertIn("production-rollback-snapshot.json", production)
        self.assertIn("production-auto-rollback-evidence.json", production)

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
        # The only github.sha references are the tooling checkout refs: build
        # identity (deploy message, config, gate) still uses the guarded SHA.
        for line in production.splitlines():
            if "github.sha" in line:
                self.assertIn("ref:", line)
        self.assertIn('--sha "$SHA" --run-id "$GITHUB_RUN_ID" --attempt "$GITHUB_RUN_ATTEMPT" \\\n'
                      '  --deploy-id "$DEPLOY_ID"', RENDER)
        self.assertIn('echo "PRODUCTION_DEPLOY_CONFIG=$config" >> "$GITHUB_ENV"', RENDER)
        names = [step[0].strip() for step in steps(JOBS["production"])]
        self.assertLess(names.index(f"- name: {RENDER_STEP}"), names.index("- name: Deploy to production"))

    def test_readyz_gate_judges_the_guarded_sha_and_mode_without_secrets(self):
        self.assertIn('production_deploy.py readyz --status "$code" --body "$readyz" --sha "$SHA" '
                      '--mode "$MODE" --build-id "$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT" '
                      '"${deploy_id_arg[@]}" 2>&1',
                      GATE)
        # The flag is conditional: real runs judge the computed ID only when
        # the deployed Dockerfile can stamp it; callers without an exported
        # ID, or a guarded SHA from before stamping, judge the legacy shape.
        self.assertIn("deploy_id_arg=()", GATE)
        self.assertIn('if [ -n "${DEPLOY_ID:-}" ] && grep -q \'^ARG BOT_DEPLOY_ID\' '
                      '../Dockerfile; then', GATE)
        self.assertIn('deploy_id_arg=(--deploy-id "$DEPLOY_ID")', GATE)
        for text in (RENDER, GATE):
            self.assertNotIn("secrets.", text)
            self.assertNotIn("token", text.lower())

    def test_gate_prints_remote_bodies_only_as_escaped_excerpts(self):
        for name in ("health", "readyz"):
            self.assertNotIn(f'cat "${name}"', GATE)
        # /readyz bodies reach the log only through the bounded escaped
        # excerpt; /health bodies never reach it at all (status code only).
        self.assertIn('production_deploy.py excerpt --body "$readyz"', GATE)
        self.assertNotIn('excerpt --body "$health"', GATE)

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
        for name in (preflight, status, reread, confirm):
            conditions = [line.strip() for line in by_name[name] if line.strip().startswith("if:")]
            self.assertEqual(conditions, ["if: env.TAKEOVER == 'true'"], name)
        # The mutating takeover POST runs in deploy mode only: rollback mode
        # already fenced, restored and handed ownership over in its own step.
        conditions = [line.strip() for line in by_name[takeover]
                      if line.strip().startswith("if:")]
        self.assertEqual(conditions, ["if: env.TAKEOVER == 'true' && env.MODE == 'deploy'"], takeover)
        text = "\n".join(sum((by_name[name] for name in (preflight, status, takeover)), []))
        # Production bindings only: the control token comes from the
        # production Environment secret, the URL from its variable.
        self.assertIn("OWNERSHIP_CONTROL_TOKEN: ${{ secrets.PRODUCTION_OWNERSHIP_CONTROL_TOKEN }}", text)
        self.assertIn("PRODUCTION_WORKER_URL: ${{ vars.PRODUCTION_WORKER_URL }}", text)
        self.assertIn("STAGING_WORKER_URL: ${{ vars.STAGING_WORKER_URL }}", text)
        self.assertNotIn("STAGING_OWNERSHIP_CONTROL_TOKEN", text)
        self.assertIn("node ../tooling/wrangler/scripts/production-ownership-control.mjs preflight", text)
        self.assertIn("node ../tooling/wrangler/scripts/production-ownership-control.mjs status", text)
        self.assertIn('node ../tooling/wrangler/scripts/production-ownership-control.mjs takeover "$epoch"', text)
        # The takeover posts exactly the P2 epoch with an explicit release and
        # a run-derived audit actor naming the guarded SHA, never GITHUB_SHA.
        self.assertIn('OWNERSHIP_RELEASE_FENCE: "true"', text)
        self.assertIn("OWNERSHIP_ACTOR: github-actions:${{ github.run_id }}:${{ needs.guard.outputs.sha }}", text)
        self.assertIn("OWNERSHIP_EXPECTED_DEPLOYMENT: ${{ env.NEW_VERSION }}", text)
        production = "\n".join(JOBS["production"])
        self.assertNotIn("inputs.takeover", production)
        self.assertIn("TAKEOVER: ${{ needs.guard.outputs.takeover }}", production)

    def test_skipped_runs_deploy_nothing(self):
        # held, already-live and non-green automatic runs stay green while
        # deploying nothing: every downstream job requires skip == ''.
        for gate in ("schema-gate", "roles-gate"):
            condition = value(children(JOBS[gate][1:], 4)["if"])
            self.assertIn("needs.guard.outputs.skip == ''", condition)
        production_if = value(children(JOBS["production"][1:], 4)["if"])
        self.assertIn("needs.guard.outputs.skip == ''", production_if)

    def test_soak_runs_outside_the_production_lane_and_recomputes_the_remainder(self):
        # The soak sleeps in its own job with no concurrency group, so a
        # sleeping promotion never holds the deploy-production lane behind
        # which a rollback would queue. The remainder is recomputed here
        # from the staging-green timestamp, not carried from guard time.
        soak_job = "\n".join(JOBS["soak"])
        self.assertIn("needs: guard", soak_job)
        condition = value(children(JOBS["soak"][1:], 4)["if"])
        for needle in ("needs.guard.outputs.skip == ''",
                       "needs.guard.outputs.mode == 'deploy'",
                       "needs.guard.outputs.allow_unstaged != 'true'",
                       "needs.guard.outputs.soak_seconds != '0'"):
            self.assertIn(needle, condition)
        self.assertNotIn("concurrency", children(JOBS["soak"][1:], 4))
        self.assertIn("GREEN_AT: ${{ needs.guard.outputs.green_at }}", soak_job)
        soak = inline_script("Wait out the remainder of the 30-minute post-staging soak")
        self.assertIn("SOAK_SECONDS - int(time.time() - green_at)", soak)
        self.assertIn("time.sleep(remaining)", soak)
        # An unreadable timestamp soaks the full window, never zero.
        self.assertIn("remaining = SOAK_SECONDS", soak)
        # Never interpolate the expression into the script text.
        self.assertNotIn("${{", soak)
        production = "\n".join(JOBS["production"])
        self.assertNotIn("Soak past the staging-green time", production)
        self.assertNotIn("needs.guard.outputs.soak_seconds", production)

    def test_recheck_rereads_hold_and_live_before_the_deploy(self):
        # The guard's hold/live verdict goes stale while gates build and the
        # soak sleeps: the recheck job gates production on a fresh read. It
        # waits for the soak AND both gates, so the read happens after the
        # last long build; a hold flipped during the gate builds still skips.
        recheck_job = "\n".join(JOBS["recheck"])
        self.assertIn("needs: [guard, soak, schema-gate, roles-gate]", recheck_job)
        condition = value(children(JOBS["recheck"][1:], 4)["if"])
        for needle in ("!cancelled()",
                       "needs.guard.result == 'success'",
                       "needs.guard.outputs.skip == ''",
                       "needs.guard.outputs.mode == 'deploy'",
                       "needs.guard.outputs.auto == 'true'",
                       "(needs.soak.result == 'success' || needs.soak.result == 'skipped')",
                       "needs.schema-gate.result == 'success'",
                       "needs.roles-gate.result == 'success'"):
            self.assertIn(needle, condition)
        self.assertIn("HOLD_UNTIL: ${{ vars.PRODUCTION_AUTO_PROMOTE_HOLD_UNTIL }}", recheck_job)
        self.assertIn("SHA: ${{ needs.guard.outputs.sha }}", recheck_job)
        # Entering the single-pending lane behind a recovery dispatch would
        # cancel the queued run, so the recheck skips while a dispatch is
        # active — and when the runs cannot be listed, so a blind run never
        # risks the lane.
        granted = set(children(children(JOBS["recheck"][1:], 4)["permissions"][1:], 6))
        self.assertEqual(granted, {"contents", "actions"})
        recheck = inline_script("Re-read the promotion hold and the live revision")
        for needle in ('f"held until {held.isoformat()}"', 'skip = "live"',
                       'skip = "older-than-live"',
                       'skip = "operator-dispatch-active"',
                       'skip = "dispatch-check-unreadable"',
                       "actions/workflows/deploy-production.yml/runs",
                       '"event=workflow_dispatch"',
                       '("queued", "waiting", "in_progress"'):
            self.assertIn(needle, recheck)
        self.assertNotIn("${{", recheck)
        production_if = value(children(JOBS["production"][1:], 4)["if"])
        # A failed soak fails closed into no deploy, a cancelled run never
        # deploys, and a skipped recheck counts only for manual runs: an
        # automatic run always needs a fresh recheck pass with no skip.
        self.assertIn("!cancelled()", production_if)
        self.assertIn("(needs.soak.result == 'success' || needs.soak.result == 'skipped')",
                      production_if)
        self.assertIn("needs.guard.outputs.auto != 'true' && needs.recheck.result == 'skipped'",
                      production_if)
        self.assertIn("needs.recheck.result == 'success' && needs.recheck.outputs.skip == ''",
                      production_if)

    def test_production_error_rate_gate_uses_reviewed_tooling_and_production_bindings(self):
        production = "\n".join(JOBS["production"])
        # Reviewed tooling from the workflow commit, never the guarded SHA.
        self.assertIn("node ../tooling/wrangler/scripts/promotion-error-gate.mjs", production)
        self.assertIn("METRICS_SCRAPE_TOKEN: ${{ secrets.PRODUCTION_METRICS_SCRAPE_TOKEN }}", production)
        self.assertIn("PRODUCTION_URL: ${{ vars.PRODUCTION_WORKER_URL }}", production)
        self.assertNotIn("STAGING_METRICS_SCRAPE_TOKEN", production)
        by_name = {step[0].strip(): step for step in steps(JOBS["production"])}
        gate = by_name["- name: Gate promotion on REST 5xx error rate"]
        self.assertIn("id: error_rate_gate", "\n".join(gate))
        # After a rollback restore there is nothing to roll back to: the
        # gate must not fail the recovery run red.
        conditions = [line.strip() for line in gate if line.strip().startswith("if:")]
        self.assertEqual(conditions, ["if: env.MODE == 'deploy'"], gate)

    def test_workflow_run_context_reaches_the_guard(self):
        # Behaviour tests inject env directly; pin the real workflow wiring
        # on the guard step so missing or misbound event fields cannot pass.
        guard_step = next(step for step in steps(JOBS["guard"])
                          if step[0].strip() == f"- name: {GUARD_STEP}")
        env = children(children(guard_step[1:], 8)["env"][1:], 10)
        for name, field in (("WORKFLOW_RUN_SHA", "head_sha"),
                            ("WORKFLOW_RUN_CONCLUSION", "conclusion"),
                            ("WORKFLOW_RUN_UPDATED_AT", "updated_at"),
                            ("WORKFLOW_RUN_PATH", "path"),
                            ("WORKFLOW_RUN_EVENT", "event")):
            with self.subTest(name=name):
                self.assertIn(name, env)
                self.assertEqual(value(env[name]), "${{ github.event.workflow_run." + field + " }}")

    def test_drill_switch_is_absent_and_production_gate_is_live_only(self):
        workflow = "\n".join(LINES)
        self.assertNotIn("inject_error_rate_spike", workflow)
        self.assertNotIn("INJECT_ERROR_RATE_SPIKE", workflow)
        self.assertNotIn("--rehearsal-firing", workflow)
        self.assertNotIn('os.environ.get("INJECT_', GUARD)
        run = inline_script("Gate promotion on REST 5xx error rate")
        for needle in ('--metrics-url "$PRODUCTION_URL"', "--evidence",
                       '--sha "$SHA"', '--user-agent "two-bot-next-production-rollout/1.0"'):
            self.assertIn(needle, run)
        for needle in ("--rehearsal-firing", "INJECT_ERROR_RATE_SPIKE", "rehearsal"):
            self.assertNotIn(needle, run)
        # The evidence filename appears exactly twice: the --evidence write
        # path and the read-back print, which must name the same file.
        self.assertEqual(run.count("production-error-rate-gate.json"), 2)
        self.assertNotIn("${{", run)


class GuardBehaviourTests(unittest.TestCase):
    def guard(self, sha=SHA, rollback="", image=None, takeover="false", allow_unstaged="false",
              ref="refs/heads/main", auto="", resolved_sha=None, event="workflow_dispatch",
              run_sha="", run_conclusion="", run_updated="", hold="", spike="",
              run_path="", run_event="", cutover_hold="", **responses):
        if image is None:
            image = IMAGE if rollback else ""
        api = {**green(), "staging-latest": {"total_count": 1, "workflow_runs": [staging_run()]},
               "production-readyz": {"status": 200, "build_revision": SHA},
               # Older-than-live compare (compare/{sha}...{live}): the default
               # live build is behind the target, so automatic runs proceed.
               "compare-live": {"status": "behind"},
               # Rollback snapshots: none by default, so a reviewer-less
               # rollback refuses unless a test supplies a verified receipt.
               "artifacts": {"total_count": 0, "artifacts": []},
               "snapshot-run": snapshot_origin_run(), **responses}
        calls = []
        resolved = sha.strip().lower() or (SHA if not rollback else "")
        # R1 short-SHA fixture routing: a 7-39 hex prefix of SHA (other than the
        # colliding AMBIGUOUS_PREFIX) resolves to the full SHA; the ambiguous
        # prefix and any other short hex fail resolution. Downstream gates must
        # use the full SHA after a successful resolution.
        is_short = bool(re.fullmatch(r"[0-9a-f]{7,39}", resolved or ""))
        is_unambiguous = is_short and resolved == SHA[:len(resolved)] and resolved != AMBIGUOUS_PREFIX
        downstream = SHA if is_unambiguous else resolved

        def fake_check_output(args, **kwargs):
            self.assertEqual(args[:4], ["gh", "api", "--method", "GET"])
            self.assertTrue(all(flag == "-f" for flag in args[5::2]), args)
            path, query = args[4], dict(arg.split("=", 1) for arg in args[6::2])
            calls.append((path, query))
            if is_short and path == f"repos/{REPO}/commits/{resolved}":
                # Short-SHA resolution via the commits API (builder contract).
                if resolved_sha is not None:
                    # A branch or tag named with a hex string resolves to a
                    # commit the operator did not type: return the injected
                    # SHA so the prefix-match check is exercised.
                    return json.dumps({"sha": resolved_sha})
                if is_unambiguous:
                    return json.dumps({"sha": SHA})
                raise failed_call()
            if path == f"repos/{REPO}/compare/{downstream}...main":
                key = "compare"
            elif (path.startswith(f"repos/{REPO}/compare/{downstream}...")
                    and re.fullmatch(r"[0-9a-f]{40}", path.split("...")[-1])):
                # Older-than-live check: whether the live revision already
                # contains the guarded SHA.
                key = "compare-live"
            elif path == f"repos/{REPO}/commits/{downstream}/check-runs":
                self.assertEqual(query["app_id"], "15368")
                self.assertEqual(query["filter"], "latest")
                key = query["check_name"]
            elif path == f"repos/{REPO}/actions/workflows/check.yml/runs":
                self.assertEqual(query["head_sha"], downstream)
                self.assertNotIn("status", query)
                self.assertNotIn("branch", query)
                key = "ci-runs"
            elif path == f"repos/{REPO}/actions/runs/{SNAPSHOT_RUN_ID}":
                key = "snapshot-run"
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
                    {"head_sha": downstream, "branch": "main"},
                )
                key = "staging-all"
            elif path == f"repos/{REPO}/actions/workflows/deploy-staging.yml/runs":
                self.assertEqual(
                    {k: query[k] for k in ("head_sha", "branch", "status")},
                    {"head_sha": downstream, "branch": "main", "status": "success"},
                )
                key = "staging"
            elif path == f"repos/{REPO}/environments/production":
                key = "environment"
            elif path == f"repos/{REPO}/environments/production/deployment-branch-policies":
                key = "branches"
            elif path == f"repos/{REPO}/actions/artifacts":
                # Paginated like every other collection: the name filter is
                # pinned, the page cursor passes through to the fixture.
                self.assertEqual(query["name"], "production-rollback-snapshot")
                self.assertEqual(query["per_page"], "100")
                key = "artifacts"
            elif re.fullmatch(rf"repos/{REPO}/actions/artifacts/[0-9]+/zip", path):
                key = "artifact-zip-" + path.split("/")[-2]
            else:
                self.fail(f"unexpected API call {path}")
            result = api[key]
            if callable(result):
                result = result(path, query)
            if isinstance(result, Exception):
                raise result
            if isinstance(result, bytes):
                return result
            if path.endswith("/check-runs") and isinstance(result, dict):
                # Match the GitHub API's name/App filters, but never filter by
                # status: a pending latest run must not disappear behind green.
                self.assertNotIn("status", query)
                result = {"check_runs": [
                    run for run in result.get("check_runs", [])
                    if run.get("name") == key and str(run.get("app", {}).get("id")) == query["app_id"]
                ]}
            return result if isinstance(result, str) else json.dumps(result)

        fake_urlopen = fake_readyz_opener(self, api, calls)

        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            output, summary = Path(tmp) / "output", Path(tmp) / "summary"
            env = {
                "GITHUB_REPOSITORY": REPO, "GITHUB_REF": ref, "SHA": sha, "ROLLBACK": rollback,
                "IMAGE": image, "TAKEOVER": takeover, "ALLOW_UNSTAGED": allow_unstaged,
                "EVENT_NAME": event,
                "WORKFLOW_RUN_SHA": run_sha, "WORKFLOW_RUN_CONCLUSION": run_conclusion,
                "WORKFLOW_RUN_UPDATED_AT": run_updated, "HOLD_UNTIL": hold,
                "WORKFLOW_RUN_PATH": run_path, "WORKFLOW_RUN_EVENT": run_event,
                "INJECT_ERROR_RATE_SPIKE": spike, "CUTOVER_HOLD": cutover_hold,
                "PRODUCTION_WORKER_URL": "https://production.example.test",
                "AUTO_APPROVE": auto,
                "GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(summary),
            }
            stdout, code = io.StringIO(), 0
            with patch.dict(os.environ, env, clear=True), \
                    patch("subprocess.check_output", side_effect=fake_check_output), \
                    patch("urllib.request.urlopen", side_effect=fake_urlopen), \
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

    def recheck(self, sha=SHA, hold="", **responses):
        api = {"production-readyz": {"status": 200, "build_revision": "f" * 40},
               "compare-live": {"status": "behind"},
               "dispatch-runs": {"workflow_runs": []}, **responses}
        calls = []

        def fake_check_output(args, **kwargs):
            self.assertEqual(args[:4], ["gh", "api", "--method", "GET"])
            path, query = args[4], dict(arg.split("=", 1) for arg in args[6::2])
            calls.append((path, query))
            if (path.startswith(f"repos/{REPO}/compare/{sha.lower()}...")
                    and re.fullmatch(r"[0-9a-f]{40}", path.split("...")[-1])):
                result = api["compare-live"]
            elif path == f"repos/{REPO}/actions/workflows/deploy-production.yml/runs":
                self.assertEqual(query.get("event"), "workflow_dispatch")
                result = api["dispatch-runs"]
            else:
                self.fail(f"unexpected API call {path}")
            if isinstance(result, Exception):
                raise result
            return result if isinstance(result, str) else json.dumps(result)

        fake_urlopen = fake_readyz_opener(self, api, calls)
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            output, summary = Path(tmp) / "output", Path(tmp) / "summary"
            env = {
                "GITHUB_REPOSITORY": REPO, "SHA": sha, "HOLD_UNTIL": hold,
                "PRODUCTION_WORKER_URL": "https://production.example.test",
                "GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(summary),
            }
            stdout, code = io.StringIO(), 0
            with patch.dict(os.environ, env, clear=True), \
                    patch("subprocess.check_output", side_effect=fake_check_output), \
                    patch("urllib.request.urlopen", side_effect=fake_urlopen), \
                    contextlib.redirect_stdout(stdout):
                try:
                    exec(RECHECK, {})
                except SystemExit as exit_:
                    code = exit_.code
            outputs = dict(
                line.split("=", 1) for line in (output.read_text().splitlines() if output.exists() else [])
            )
            return code, outputs, summary.read_text() if summary.exists() else "", calls, stdout.getvalue()

    def test_green_main_sha_passes_and_reports(self):
        code, outputs, summary, calls, _ = self.guard()
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"sha": SHA, "mode": "deploy", "version": "", "restore_image": "",
                                   "takeover": "true", "allow_unstaged": "false",
                                   "skip": "", "soak_seconds": "0",
                                   "auto": "false", "green_at": "",
                                   "staging_run_id": "501", "staging_attempt": "2"})
        self.assertIn("Takeover: on", summary)
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
        # R1: 7-39 hex prefixes are valid short SHAs resolved via the commits
        # API (see the short-SHA fixtures below); SHA[:39] is no longer
        # malformed. Shorter than 7, longer than 40, or non-hex refuses before
        # any API call with the exact accepted format.
        for sha in ("main", SHA[:6], "abc", SHA + "0", "g" * 40, "g" * 7,
                    "$(id)", f"{SHA};id", f"{SHA[:20]} {SHA[20:]}"):
            with self.subTest(sha=sha):
                result = self.guard(sha=sha)
                self.assertRefused(result, "40-character hex")
                self.assertRefused(result, ACCEPTED_FORMAT)
                self.assertEqual(result[3], [])

    def test_unambiguous_short_sha_is_resolved_and_accepted(self):
        # R1 acceptance (TOG-28737; builder TOG-28741 must not edit): given a
        # 12-char unambiguous prefix of a green main commit, when the guard
        # parses it, then it resolves via the commits API and passes with the
        # full 40-char SHA in outputs. Covers deploy and rollback modes.
        for rollback in ("", VERSION):
            with self.subTest(mode="rollback" if rollback else "deploy"):
                code, outputs, summary, calls, _ = self.guard(
                    sha=SHORT_UNAMBIGUOUS, rollback=rollback)
                self.assertEqual(code, 0)
                self.assertEqual(outputs["sha"], SHA)
                self.assertEqual(outputs["mode"], "rollback" if rollback else "deploy")
                self.assertIn(SHA, summary)
                # Resolution first, then every downstream gate uses the full SHA.
                self.assertEqual(calls[0][0], f"repos/{REPO}/commits/{SHORT_UNAMBIGUOUS}")
                self.assertIn((f"repos/{REPO}/compare/{SHA}...main", {"per_page": "1"}), calls)
                for path, query in calls:
                    if "head_sha" in query:
                        self.assertEqual(query["head_sha"], SHA)

    def test_ambiguous_short_sha_is_refused_with_accepted_format(self):
        # R1 acceptance (TOG-28737; builder TOG-28741 must not edit): given a
        # 7-char prefix colliding with another commit (fixture: AMBIGUOUS_PREFIX
        # matches SHA and COLLIDING_SHA), when the guard parses it, then it is
        # refused with the exact accepted format and gates nothing on the
        # collision.
        self.assertEqual(AMBIGUOUS_PREFIX, SHA[:7])
        self.assertEqual(COLLIDING_SHA[:7], SHA[:7])
        self.assertNotEqual(COLLIDING_SHA, SHA)
        result = self.guard(sha=AMBIGUOUS_PREFIX)
        self.assertRefused(result, "40-character hex")
        self.assertRefused(result, ACCEPTED_FORMAT)
        code, outputs, _, calls, _ = result
        self.assertEqual(code, 1)
        self.assertEqual(outputs, {})
        # Only the resolution attempt; no compare/CI/staging gates on ambiguity.
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][0], f"repos/{REPO}/commits/{AMBIGUOUS_PREFIX}")

    def test_short_sha_resolving_to_a_different_commit_is_refused(self):
        # The commits endpoint resolves any ref, including a branch or tag
        # named with a hex string: given a typed prefix whose resolution
        # returns a full SHA that does not start with it, when the guard
        # parses it, then it refuses with the exact accepted format and runs
        # no downstream gate on the foreign commit. Mutant: deleting
        # `not full.startswith(short)` from the guard fails this test.
        foreign = "f" * 40
        self.assertTrue(re.fullmatch(r"[0-9a-f]{40}", foreign))
        self.assertFalse(foreign.startswith(SHORT_UNAMBIGUOUS))
        result = self.guard(sha=SHORT_UNAMBIGUOUS, resolved_sha=foreign)
        self.assertRefused(result, "40-character hex")
        self.assertRefused(result, ACCEPTED_FORMAT)
        code, outputs, _, calls, _ = result
        self.assertEqual(code, 1)
        self.assertEqual(outputs, {})
        # Only the resolution attempt; no compare/CI/staging gates on the
        # foreign commit.
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][0], f"repos/{REPO}/commits/{SHORT_UNAMBIGUOUS}")

    def test_rollback_needs_a_version_uuid(self):
        for bad in ("1234", "latest", "../v", f"{VERSION} --force", VERSION[:-1]):
            with self.subTest(rollback=bad):
                result = self.guard(rollback=bad)
                self.assertRefused(result, "Worker version ID")
                self.assertEqual(result[3], [])
        code, outputs, summary, _, _ = self.guard(rollback=VERSION.upper())
        self.assertEqual(code, 0)
        # Recovery never depends on staging history: both source outputs stay empty.
        self.assertEqual(outputs, {"sha": SHA, "mode": "rollback", "version": VERSION,
                                   "restore_image": IMAGE, "takeover": "false",
                                   "allow_unstaged": "false", "skip": "", "soak_seconds": "0",
                                   "auto": "false", "green_at": "",
                                   "staging_run_id": "", "staging_attempt": ""})
        self.assertIn(VERSION, summary)

    def test_rollback_needs_the_previous_container_image(self):
        # The digest is a content hash, safe as an input; without it the
        # rollback would repeat the incident (Worker back, image still bad).
        for image in ("", "latest", IMAGE[:-1], IMAGE + "0", IMAGE.replace("@sha256:", ":x@sha256:")):
            with self.subTest(image=image[:40]):
                result = self.guard(rollback=VERSION, image=image)
                self.assertRefused(result, "previous container image")
                self.assertEqual(result[3], [])
        result = self.guard(image=IMAGE)
        self.assertRefused(result, "rollback-only input")
        self.assertEqual(result[3], [])

    def test_rollback_never_depends_on_staging_history(self):
        # Aged-out staging history blocked the 10-15 rollback. A rollback
        # needs the version that ran in prod plus takeover, never staging.
        code, outputs, summary, calls, _ = self.guard(
            rollback=VERSION, takeover="true",
            staging={"total_count": 0, "workflow_runs": []})
        self.assertEqual(code, 0)
        self.assertEqual(outputs["mode"], "rollback")
        self.assertIn("never blocks recovery", summary)
        self.assertFalse(any("deploy-staging" in path for path, _ in calls))

    def test_live_sha_waiver_requires_live_readyz_revision(self):
        result = self.guard(
            takeover="true", allow_unstaged="true",
            **{"production-readyz": {"build_revision": "f" * 40}},
        )
        self.assertRefused(result, "live production build revision does not match requested SHA")

    def test_live_sha_takeover_redeploy_waives_staging_history(self):
        # The owner's first-response cure for a stale bot: re-serve the live
        # SHA with takeover when staging history aged out.
        code, outputs, summary, calls, _ = self.guard(
            takeover="true", allow_unstaged="true",
            staging={"total_count": 0, "workflow_runs": []})
        self.assertEqual(code, 0)
        # The live-SHA waiver re-serves the live build without staging history.
        self.assertEqual(outputs, {"sha": SHA, "mode": "deploy", "version": "", "restore_image": "",
                                   "takeover": "true", "allow_unstaged": "true",
                                   "skip": "", "soak_seconds": "0",
                                   "auto": "false", "green_at": "",
                                   "staging_run_id": "", "staging_attempt": ""})
        self.assertIn("operator confirms the SHA is live", summary)
        self.assertIn(("production-readyz", {"url": "https://production.example.test/readyz", "timeout": 10}), calls)
        self.assertFalse(any("deploy-staging" in path for path, _ in calls))

    def test_live_sha_waiver_accepts_parked_readyz_only_with_matching_revision(self):
        result = self.guard(
            takeover="true", allow_unstaged="true",
            **{"production-readyz": {"status": 503, "build_revision": SHA}},
        )
        self.assertEqual(result[0], 0)

    def test_live_sha_waiver_refuses_unexpected_readyz_status(self):
        result = self.guard(
            takeover="true", allow_unstaged="true",
            **{"production-readyz": {"status": 500, "build_revision": SHA}},
        )
        self.assertRefused(result, "unexpected status 500")

    def test_allow_unstaged_needs_takeover_and_deploy_mode(self):
        result = self.guard(allow_unstaged="true", cutover_hold="true")
        self.assertRefused(result, "needs takeover true")
        self.assertEqual(result[3], [])
        result = self.guard(rollback=VERSION, allow_unstaged="true")
        self.assertRefused(result, "never a rollback")
        self.assertEqual(result[3], [])
        for flag in ("", "yes", "True", "1"):
            with self.subTest(flag=flag):
                result = self.guard(allow_unstaged=flag)
                self.assertRefused(result, "allow_unstaged must be true or false")
                self.assertEqual(result[3], [])

    def test_takeover_flag_is_validated_before_any_api_call(self):
        for flag in ("", "yes", "True", "1", " true", "takeover"):
            with self.subTest(flag=flag):
                result = self.guard(takeover=flag)
                self.assertRefused(result, "takeover must be true or false")
                self.assertEqual(result[3], [])
        code, outputs, summary, _, _ = self.guard(takeover="true")
        self.assertEqual(code, 0)
        self.assertEqual(outputs["takeover"], "true")
        self.assertIn("Takeover: on", summary)

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
        self.assertEqual(outputs, {"sha": SHA, "mode": "deploy", "version": "", "restore_image": "",
                                   "takeover": "true", "allow_unstaged": "false",
                                   "skip": "", "soak_seconds": "0",
                                   "auto": "false", "green_at": "",
                                   "staging_run_id": "501", "staging_attempt": "2"})
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

    def test_ordinary_reviewers_forward_the_latest_staging_run_and_attempt(self):
        # The source guard runs on every ordinary promotion too: the newest
        # eligible run (by run_number, then id) is forwarded verbatim, and a
        # rerun's incremented attempt is never flattened to empty or 1.
        older = staging_run(run_id=500, number=4, attempt=1)
        runs = {"total_count": 2, "workflow_runs": [older, staging_run()]}
        code, outputs, _, _, _ = self.guard(**{"staging-all": runs})
        self.assertEqual(code, 0)
        self.assertEqual(outputs["staging_run_id"], "501")
        self.assertEqual(outputs["staging_attempt"], "2")
        # Removed drill inputs cannot change the forwarded source either.
        for spike in ("true", "false"):
            with self.subTest(spike=spike):
                code, outputs, _, _, _ = self.guard(
                    spike=spike, **{"staging-all": runs})
                self.assertEqual(code, 0)
                self.assertEqual(outputs["staging_run_id"], "501")
                self.assertEqual(outputs["staging_attempt"], "2")

    def test_ordinary_reviewers_refuse_unless_the_latest_staging_run_is_green(self):
        # An older success cannot hide a newer failed, still-running, or
        # current-attempt run on the reviewer path either.
        cases = {
            "newer failed run": [staging_run(), staging_run(run_id=502, number=6, conclusion="failure")],
            "newer in-progress run": [staging_run(), staging_run(run_id=502, number=6,
                                                                 status="in_progress", conclusion=None)],
            "current attempt still running": [staging_run(status="in_progress", conclusion=None)],
        }
        for name, run_list in cases.items():
            with self.subTest(name=name):
                result = self.guard(
                    **{"staging-all": {"total_count": len(run_list), "workflow_runs": run_list}})
                self.assertRefused(result, "not a completed success")

    def test_ordinary_reviewers_exclude_verified_proof_runs(self):
        # A history holding only the dispatch-only rollback proof certifies
        # nothing; beside a real green run the proof is skipped and the green
        # run (501/2) is forwarded, with reviewers still required throughout.
        proof_row = {"id": 500, "run_number": 4, "head_sha": SHA,
                     "status": "completed", "conclusion": "failure",
                     "display_title": "staging rollback proof"}
        proof_current = {**proof_row, "run_attempt": 1, "event": "workflow_dispatch",
                         "path": ".github/workflows/deploy-staging.yml",
                         "head_branch": "main", "repository": {"full_name": REPO},
                         "head_repository": {"full_name": REPO}}
        proof_jobs = {"total_count": 1, "jobs": [{
            "run_id": 500, "run_attempt": 1, "head_sha": SHA,
            "status": "completed", "conclusion": "failure",
            "steps": [
                {"name": "Snapshot the pre-deploy rollback state", "conclusion": "success"},
                {"name": "Deploy to staging", "conclusion": "success"},
                {"name": "Verify exact new rollout and serving Worker/container readiness",
                 "conclusion": "success"},
                {"name": "Gate promotion on REST 5xx error rate", "conclusion": "failure"},
                {"name": "Auto-roll back Worker version and container image", "conclusion": "success"},
                {"name": "Upload the rollout evidence", "conclusion": "success"},
            ]}]}

        def ci_run(path, query):
            if path == f"repos/{REPO}/actions/runs/500":
                return proof_current
            return workflow_run()

        def ci_jobs(path, query):
            if path == f"repos/{REPO}/actions/runs/500/attempts/1/jobs":
                return proof_jobs
            return {"total_count": 2,
                    "jobs": [workflow_job(name) for name in ("ci-ok", "worker check")]}

        responses = {"ci-run": ci_run, "ci-jobs": ci_jobs}
        result = self.guard(
            **responses,
            **{"staging-all": {"total_count": 1, "workflow_runs": [proof_row]}})
        self.assertRefused(result, "no certification run")
        code, outputs, _, _, _ = self.guard(
            **responses,
            **{"staging-all": {"total_count": 2, "workflow_runs": [proof_row, staging_run()]}})
        self.assertEqual(code, 0)
        self.assertEqual(outputs["staging_run_id"], "501")
        self.assertEqual(outputs["staging_attempt"], "2")

    def test_reviewers_stay_required_unless_auto_approve_is_exactly_true(self):
        environment = {**green()["environment"], "protection_rules": []}
        for flag in ("", "false", "TRUE", "1", " true"):
            with self.subTest(flag=flag):
                self.assertRefused(self.guard(auto=flag, environment=environment), "no required reviewers")

    def test_staging_waivers_still_need_human_reviewers_with_auto_approve(self):
        # Automated approval vouches through staging history; a live-SHA
        # redeploy waives that history, so auto-approve cannot replace the
        # human Environment reviewers there. A rollback without a verified
        # production snapshot of its target refuses the same way.
        environment = {**green()["environment"], "protection_rules": []}
        result = self.guard(auto="true", environment=environment, rollback=VERSION, takeover="true")
        self.assertRefused(result, "no required reviewers")
        self.assertRefused(result, "matches no verified rollback snapshot")
        result = self.guard(auto="true", environment=environment, takeover="true", allow_unstaged="true")
        self.assertRefused(result, "no required reviewers")

    # Rule 3 (10-20 13:31Z): recovery never waits for a human. With
    # automated approval and zero reviewers, a rollback whose whole target is
    # a verified production rollback snapshot runs.
    def test_reviewerless_rollback_runs_from_a_verified_production_snapshot(self):
        environment = {**green()["environment"], "protection_rules": [{"type": "branch_policy"}]}
        code, outputs, summary, calls, _ = self.guard(
            auto="true", environment=environment, rollback=VERSION, takeover="true",
            **snapshot_responses())
        self.assertEqual(code, 0)
        self.assertEqual(outputs["mode"], "rollback")
        self.assertEqual(outputs["version"], VERSION)
        self.assertEqual(outputs["restore_image"], IMAGE)
        self.assertIn(f"verified rollback snapshot of deploy-production run `{SNAPSHOT_RUN_ID}`", summary)
        self.assertIn(f"repos/{REPO}/actions/artifacts/{SNAPSHOT_ARTIFACT_ID}/zip", [path for path, _ in calls])
        # The SHA still passed ancestry and the full CI verdict first.
        paths = [path for path, _ in calls]
        self.assertIn(f"repos/{REPO}/compare/{SHA}...main", paths)
        self.assertTrue(any(path.endswith("/check-runs") for path in paths))

    def test_reviewerless_rollback_refuses_any_unverified_snapshot(self):
        environment = {**green()["environment"], "protection_rules": []}
        other = "f" * 40
        cases = {
            "other version": snapshot_responses(receipt=snapshot_receipt(version=OLD_VERSION)),
            "other image": snapshot_responses(receipt=snapshot_receipt(image=IMAGE.replace("c" * 64, "d" * 64))),
            "other sha": snapshot_responses(receipt=snapshot_receipt(sha=other)),
            "incomplete provenance": snapshot_responses(receipt=snapshot_receipt(complete=False)),
            "no previous": snapshot_responses(receipt={**snapshot_receipt(), "has_previous": False}),
            "too old": snapshot_responses(artifact=snapshot_artifact(age_days=15)),
            "expired": snapshot_responses(artifact=snapshot_artifact(expired=True)),
            "not main": snapshot_responses(artifact=snapshot_artifact(branch="feature")),
            "fork run": snapshot_responses(artifact=snapshot_artifact(head_repo=2)),
            "other workflow": snapshot_responses(run=snapshot_origin_run(path=".github/workflows/check.yml")),
            "pull request run": snapshot_responses(run=snapshot_origin_run(event="pull_request")),
            "run off main": snapshot_responses(run=snapshot_origin_run(branch="feature")),
            "fork head repo": snapshot_responses(run=snapshot_origin_run(repo="evil/two-bot-next")),
            "unreadable zip": {**snapshot_responses(), f"artifact-zip-{SNAPSHOT_ARTIFACT_ID}": failed_call()},
            "not a zip": {**snapshot_responses(), f"artifact-zip-{SNAPSHOT_ARTIFACT_ID}": b"not a zip"},
            "wrong file": {**snapshot_responses(),
                           f"artifact-zip-{SNAPSHOT_ARTIFACT_ID}": snapshot_zip(snapshot_receipt(), name="x.json")},
        }
        for label, responses in cases.items():
            with self.subTest(label):
                result = self.guard(auto="true", environment=environment, rollback=VERSION,
                                    takeover="true", **responses)
                self.assertRefused(result, "matches no verified rollback snapshot")
        # Without automated approval a snapshot never replaces reviewers.
        result = self.guard(environment=environment, rollback=VERSION, **snapshot_responses())
        self.assertRefused(result, "no required reviewers")
        # The SHA/ancestry/CI checks still apply before any snapshot read.
        result = self.guard(auto="true", environment=environment, rollback=VERSION,
                            compare={"status": "diverged"}, **snapshot_responses())
        self.assertRefused(result, "not an ancestor of origin/main")
        self.assertFalse(any("artifacts" in path for path, _ in result[3]))

    def test_reviewerless_rollback_picks_the_matching_snapshot_among_recent_ones(self):
        environment = {**green()["environment"], "protection_rules": []}
        newer = snapshot_artifact(artifact_id=7701, run_id=9101, age_days=0)
        responses = {
            **snapshot_responses(),
            "artifacts": {"total_count": 2, "artifacts": [newer, snapshot_artifact()]},
            "artifact-zip-7701": snapshot_zip(snapshot_receipt(version=OLD_VERSION)),
        }
        code, _, summary, _, _ = self.guard(auto="true", environment=environment, rollback=VERSION,
                                            takeover="true", **responses)
        self.assertEqual(code, 0)
        self.assertIn(f"deploy-production run `{SNAPSHOT_RUN_ID}`", summary)

    def test_reviewerless_rollback_skips_a_corrupt_deflate_snapshot(self):
        # One corrupt-deflate snapshot must not abort the search: the newer
        # corrupt zip is skipped and the older match still approves.
        environment = {**green()["environment"], "protection_rules": []}
        newer = snapshot_artifact(artifact_id=7701, run_id=9101, age_days=0)
        responses = {
            **snapshot_responses(),
            "artifacts": {"total_count": 2, "artifacts": [newer, snapshot_artifact()]},
            "artifact-zip-7701": corrupt_deflate_zip(snapshot_receipt()),
        }
        # The decoy must fail inside the decompressor, not zip parsing, or
        # this test would pass on the pre-fix BadZipFile-only handler.
        with self.assertRaises(zlib.error):
            with zipfile.ZipFile(io.BytesIO(responses["artifact-zip-7701"])) as archive:
                archive.read("production-rollback-snapshot.json")
        code, _, summary, _, _ = self.guard(auto="true", environment=environment, rollback=VERSION,
                                            takeover="true", **responses)
        self.assertEqual(code, 0)
        self.assertIn(f"deploy-production run `{SNAPSHOT_RUN_ID}`", summary)

    def test_reviewerless_rollback_finds_a_match_past_the_first_page(self):
        # A busy artifact history must not hide the snapshot: page 1 holds
        # only newer non-matching snapshots, the match sits on page 2.
        environment = {**green()["environment"], "protection_rules": []}
        decoys = [snapshot_artifact(artifact_id=7800 + i, run_id=9200 + i, age_days=0)
                  for i in range(3)]
        pages = {"1": {"total_count": 4, "artifacts": decoys},
                 "2": {"total_count": 4, "artifacts": [snapshot_artifact()]}}
        responses = {**snapshot_responses(),
                     "artifacts": lambda path, query: pages[query["page"]]}
        for decoy in decoys:
            responses[f"artifact-zip-{decoy['id']}"] = snapshot_zip(
                snapshot_receipt(version=OLD_VERSION))
        code, _, summary, calls, _ = self.guard(
            auto="true", environment=environment, rollback=VERSION,
            takeover="true", **responses)
        self.assertEqual(code, 0)
        self.assertIn(f"deploy-production run `{SNAPSHOT_RUN_ID}`", summary)
        artifact_pages = [query["page"] for path, query in calls
                          if path == f"repos/{REPO}/actions/artifacts"]
        self.assertEqual(artifact_pages, ["1", "2"])

    def test_reviewerless_rollback_checks_every_fresh_snapshot(self):
        # No newest-N cap: 35 newer non-matching snapshots must not hide an
        # older match in the same 14-day window.
        environment = {**green()["environment"], "protection_rules": []}
        decoys = [snapshot_artifact(artifact_id=7800 + i, run_id=9200 + i, age_days=0)
                  for i in range(35)]
        responses = {**snapshot_responses(),
                     "artifacts": {"total_count": 36, "artifacts": [*decoys, snapshot_artifact()]}}
        for decoy in decoys:
            responses[f"artifact-zip-{decoy['id']}"] = snapshot_zip(
                snapshot_receipt(version=OLD_VERSION))
        code, _, summary, _, _ = self.guard(
            auto="true", environment=environment, rollback=VERSION,
            takeover="true", **responses)
        self.assertEqual(code, 0)
        self.assertIn(f"deploy-production run `{SNAPSHOT_RUN_ID}`", summary)

    def test_reviewerless_rollback_refuses_an_unlistable_snapshot_history(self):
        # Failing closed stays a refusal, but the summary distinguishes an
        # unreadable history from an unmatched target.
        environment = {**green()["environment"], "protection_rules": []}
        result = self.guard(auto="true", environment=environment, rollback=VERSION,
                            takeover="true", artifacts={"message": "nope"})
        self.assertRefused(result, "could not enumerate the production rollback snapshots")

    def test_reviewers_present_never_read_snapshots(self):
        code, _, _, calls, _ = self.guard(rollback=VERSION, takeover="true")
        self.assertEqual(code, 0)
        self.assertFalse(any("artifacts" in path for path, _ in calls))

    def test_unexpected_trigger_is_refused(self):
        result = self.guard(event="push")
        self.assertRefused(result, "unexpected trigger")
        self.assertEqual(result[3], [])

    def test_automatic_run_refuses_rollback_inputs(self):
        # Recovery is always an explicit operator dispatch, never automatic.
        result = self.guard(event="schedule", rollback=VERSION, image=IMAGE)
        self.assertRefused(result, "rollback inputs never apply to an automatic run")
        self.assertEqual(result[3], [])

    def test_workflow_run_without_success_skips_green(self):
        # A red staging completion promotes nothing: green, no API storm.
        for conclusion in ("failure", "cancelled", ""):
            with self.subTest(conclusion=conclusion):
                code, outputs, summary, calls, _ = self.guard(
                    event="workflow_run", sha="", run_sha=SHA, run_conclusion=conclusion)
                self.assertEqual(code, 0)
                self.assertEqual(outputs["skip"], "staging-not-green")
                self.assertEqual(outputs["sha"], SHA)
                self.assertEqual(calls, [])
                self.assertIn("nothing to promote", summary)

    def test_workflow_run_green_promotes_latest_not_its_sha_with_takeover_and_soak(self):
        # Automatic promotion is the empty-sha dispatch with takeover, plus
        # the remaining soak past the staging-green time. A green completion
        # promotes the LATEST successful staging deploy, never the triggering
        # run's SHA: the trigger here names an older run, which must not
        # downgrade production.
        old_run_sha = "e" * 40
        now = datetime.datetime.now(datetime.timezone.utc)
        green_at = (now - datetime.timedelta(seconds=60)).strftime("%Y-%m-%dT%H:%M:%SZ")
        code, outputs, summary, calls, _ = self.guard(
            event="workflow_run", sha="", run_sha=old_run_sha, run_conclusion="success",
            **{"production-readyz": {"status": 200, "build_revision": "f" * 40},
               "staging": {"total_count": 1, "workflow_runs": [
                   {"conclusion": "success", "updated_at": green_at}]}})
        self.assertEqual(code, 0)
        self.assertEqual(outputs["sha"], SHA)
        self.assertNotEqual(outputs["sha"], old_run_sha)
        self.assertEqual(outputs["takeover"], "true")
        self.assertEqual(outputs["allow_unstaged"], "false")
        self.assertEqual(outputs["skip"], "")
        self.assertEqual(outputs["auto"], "true")
        self.assertTrue(outputs["green_at"], outputs)
        self.assertTrue(1700 <= int(outputs["soak_seconds"]) <= 1800, outputs)
        self.assertIn("automatic", summary)
        self.assertIn("Soak:", summary)
        # The target came from the latest-successful selection, not the trigger.
        self.assertEqual(calls[0][0], f"repos/{REPO}/actions/workflows/deploy-staging.yml/runs")

    def test_staging_completion_before_ci_skips_green_ci_pending(self):
        # 10-20 race: deploy-staging finished ~23 min before check.yml on the
        # same SHA. The staging-triggered run must skip green, not refuse;
        # the CI completion promotes.
        for status in ("queued", "in_progress", "waiting"):
            with self.subTest(status=status):
                pending = workflow_run(status=status, conclusion=None)
                code, outputs, summary, _, stdout = self.guard(
                    event="workflow_run", sha="", run_sha=SHA, run_conclusion="success",
                    run_path=".github/workflows/deploy-staging.yml", run_event="push",
                    **{"ci-runs": {"total_count": 1, "workflow_runs": [pending]}, "ci-run": pending})
                self.assertEqual(code, 0, stdout)
                self.assertEqual(outputs["skip"], "ci-pending")
                self.assertEqual(outputs["sha"], SHA)
                self.assertIn("still running", summary)

    def test_completed_red_ci_still_refuses_automatic_run(self):
        red = workflow_run(conclusion="failure")
        self.assertRefused(self.guard(
            event="workflow_run", sha="", run_sha=SHA, run_conclusion="success",
            **{"ci-runs": {"total_count": 1, "workflow_runs": [red]}, "ci-run": red}),
            "no successful completed verdict")

    def test_dispatch_with_pending_ci_still_refuses(self):
        pending = workflow_run(status="in_progress", conclusion=None)
        self.assertRefused(self.guard(
            **{"ci-runs": {"total_count": 1, "workflow_runs": [pending]}, "ci-run": pending}),
            "no successful completed verdict")

    def test_ci_completion_on_latest_staged_sha_promotes(self):
        # CI finished last: the check-triggered run promotes the latest staged
        # SHA through every gate (ci-ok, worker check, staging, soak).
        code, outputs, summary, calls, stdout = self.guard(
            event="workflow_run", sha="", run_sha=SHA, run_conclusion="success",
            run_path=".github/workflows/check.yml", run_event="push",
            **{"production-readyz": {"status": 200, "build_revision": "f" * 40}})
        self.assertEqual(code, 0, stdout)
        self.assertEqual(outputs["sha"], SHA)
        self.assertEqual(outputs["skip"], "")
        self.assertEqual(outputs["auto"], "true")
        self.assertEqual(outputs["takeover"], "true")
        self.assertEqual(calls[0][0], f"repos/{REPO}/actions/workflows/deploy-staging.yml/runs")
        self.assertTrue(any(path.endswith("/check-runs") for path, _ in calls))

    def test_ci_completion_before_staging_skips_green_staging_pending(self):
        # CI finished first (or on a superseded commit): its SHA is not the
        # latest staged one, so it skips green and the staging completion
        # promotes. No gate reads beyond the staging lookup.
        ci_sha = "d" * 40
        code, outputs, summary, calls, stdout = self.guard(
            event="workflow_run", sha="", run_sha=ci_sha, run_conclusion="success",
            run_path=".github/workflows/check.yml", run_event="push")
        self.assertEqual(code, 0, stdout)
        self.assertEqual(outputs["skip"], "staging-pending")
        self.assertEqual(outputs["sha"], ci_sha)
        self.assertEqual([path for path, _ in calls],
                         [f"repos/{REPO}/actions/workflows/deploy-staging.yml/runs"])
        self.assertIn("nothing to promote", summary)

    def test_ci_completion_red_or_not_push_skips_green(self):
        for conclusion, event, reason in (("failure", "push", "ci-not-green"),
                                          ("cancelled", "push", "ci-not-green"),
                                          ("success", "pull_request", "ci-not-push"),
                                          ("success", "schedule", "ci-not-push")):
            with self.subTest(conclusion=conclusion, event=event):
                code, outputs, _, calls, _ = self.guard(
                    event="workflow_run", sha="", run_sha=SHA, run_conclusion=conclusion,
                    run_path=".github/workflows/check.yml", run_event=event)
                self.assertEqual(code, 0)
                self.assertEqual(outputs["skip"], reason)
                self.assertEqual(calls, [])

    def test_target_older_than_live_skips_green(self):
        # The live build already contains the target: deploying it would be
        # an unrequested downgrade with newer migrations under older code.
        # Intentional downgrades stay an explicit operator rollback.
        live = "b" * 40
        code, outputs, summary, _, _ = self.guard(
            event="schedule", sha="",
            **{"production-readyz": {"status": 200, "build_revision": live},
               "compare-live": {"status": "ahead"}})
        self.assertEqual(code, 0)
        self.assertEqual(outputs["skip"], "older-than-live")
        self.assertIn("nothing is deployed", summary)

    def test_unknown_live_revision_falls_through_to_gates(self):
        # The older-than-live check is non-refusing: an unreadable live
        # revision, or a failed compare read, still deploys under the guard's
        # gates instead of refusing a deployable build.
        code, outputs, _, _, _ = self.guard(
            event="schedule", sha="",
            **{"production-readyz": {"status": 500, "build_revision": SHA},
               "compare-live": failed_call(),
               "staging": {"total_count": 1, "workflow_runs": [{"conclusion": "success"}]}})
        self.assertEqual(code, 0)
        self.assertEqual(outputs["skip"], "")
        code, outputs, _, calls, _ = self.guard(
            event="schedule", sha="",
            **{"production-readyz": {"status": 200, "build_revision": "c" * 40},
               "compare-live": failed_call(),
               "staging": {"total_count": 1, "workflow_runs": [{"conclusion": "success"}]}})
        self.assertEqual(code, 0)
        self.assertEqual(outputs["skip"], "")
        self.assertTrue(any(path.startswith(f"repos/{REPO}/compare/") for path, _ in calls))

    def test_schedule_after_rollback_stays_held_when_hold_set(self):
        # After a manual rollback the next automatic run re-selects the
        # staged build it just rolled back: the operator holds promotion
        # until the incident is resolved, or the backstop redeploys it.
        code, outputs, _, _, _ = self.guard(
            event="schedule", sha="", hold="2999-01-01T00:00:00Z",
            **{"production-readyz": {"status": 200, "build_revision": "a" * 40},
               "compare-live": {"status": "behind"}})
        self.assertEqual(code, 0)
        self.assertTrue(outputs["skip"].startswith("held until"), outputs)

    # The removed environment variable cannot arm anything, even if inherited
    # from a stale runner. Input absence itself is pinned against raw YAML.
    def test_removed_drill_environment_cannot_change_guard_decisions(self):
        for kwargs in ({"takeover": "true"}, {"takeover": "false"}, {"cutover_hold": "true"},
                       {"event": "schedule", "sha": ""}):
            baseline = self.guard(**kwargs)
            for spike in ("true", "false", "yes"):
                with self.subTest(kwargs=kwargs, spike=spike):
                    result = self.guard(spike=spike, **kwargs)
                    self.assertEqual(result[0], baseline[0])
                    self.assertEqual(result[1], baseline[1])
                    self.assertEqual(result[3], baseline[3])

    def test_removed_drill_input_has_no_guard_or_gate_reader(self):
        workflow = "\n".join(LINES)
        self.assertNotIn("inject_error_rate_spike", workflow.lower())
        self.assertNotIn("--rehearsal-firing", workflow)
        self.assertNotIn("inject_gateway_failure", workflow)
        self.assertNotIn("spike =", GUARD)
        self.assertNotIn("INJECT_ERROR_RATE_SPIKE", GUARD)

    # Rule 2 (10-20 13:31Z): a manual deploy can never leave production
    # fenced. Deploy mode takes over by default; only cutover_hold skips it.
    def test_manual_deploy_always_takes_over_unless_cutover_hold(self):
        for takeover in ("false", "true"):
            with self.subTest(takeover=takeover):
                code, outputs, summary, _, stdout = self.guard(takeover=takeover)
                self.assertEqual(code, 0)
                self.assertEqual(outputs["takeover"], "true")
                self.assertIn("Takeover: on", summary)
                self.assertNotIn("fence stays held", summary.lower())
                if takeover == "false":
                    self.assertIn("takeover=false no longer skips the deploy takeover", stdout)
        for takeover in ("false", "true"):
            with self.subTest(cutover_hold=True, takeover=takeover):
                code, outputs, summary, _, _ = self.guard(takeover=takeover, cutover_hold="true")
                self.assertEqual(code, 0)
                self.assertEqual(outputs["takeover"], "false")
                self.assertIn("SKIPPED by cutover_hold", summary)
                self.assertIn("fence STAYS HELD", summary)

    def test_cutover_hold_is_deploy_only_and_validated(self):
        self.assertRefused(self.guard(rollback=VERSION, takeover="true", cutover_hold="true"),
                           "cutover_hold applies to a deploy, never a rollback")
        self.assertRefused(self.guard(event="schedule", sha="", cutover_hold="true"),
                           "cutover_hold never applies to an automatic run")
        for flag in ("yes", "True", "1"):
            with self.subTest(flag=flag):
                result = self.guard(cutover_hold=flag)
                self.assertRefused(result, "cutover_hold must be true or false")
                self.assertEqual(result[3], [])
        # Rollback mode keeps its own takeover flag (read/confirm ownership).
        code, outputs, _, _, _ = self.guard(rollback=VERSION, takeover="false")
        self.assertEqual(code, 0)
        self.assertEqual(outputs["takeover"], "false")

    def test_schedule_promotes_the_latest_staged_commit(self):
        code, outputs, _, calls, stdout = self.guard(event="schedule", sha="")
        self.assertEqual(code, 0)
        self.assertEqual(outputs["sha"], SHA)
        self.assertEqual(outputs["takeover"], "true")
        self.assertIn("Promoting the latest successful staging deploy", stdout)
        self.assertEqual(calls[0][0], f"repos/{REPO}/actions/workflows/deploy-staging.yml/runs")

    def test_stale_staging_needs_no_soak(self):
        now = datetime.datetime.now(datetime.timezone.utc)
        green_at = (now - datetime.timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%SZ")
        _, outputs, _, _, _ = self.guard(
            event="schedule", sha="",
            **{"production-readyz": {"status": 200, "build_revision": "f" * 40},
               "staging": {"total_count": 1, "workflow_runs": [
                   {"conclusion": "success", "updated_at": green_at}]}})
        self.assertEqual(outputs["skip"], "")
        self.assertEqual(outputs["soak_seconds"], "0")

    def test_unreadable_staging_time_soaks_the_full_window(self):
        # A missing timestamp must not skip the soak: fail closed.
        _, outputs, _, _, _ = self.guard(
            event="schedule", sha="",
            **{"production-readyz": {"status": 200, "build_revision": "f" * 40}})
        self.assertEqual(outputs["skip"], "")
        self.assertEqual(outputs["soak_seconds"], "1800")

    def test_live_target_skips_green(self):
        # The default readyz fixture already serves the SHA: an automatic
        # run then deploys nothing.
        code, outputs, summary, _, _ = self.guard(event="schedule", sha="")
        self.assertEqual(code, 0)
        self.assertEqual(outputs["skip"], "live")
        self.assertIn("nothing is deployed", summary)

    def test_hold_skips_until_expiry_then_proceeds(self):
        code, outputs, summary, _, _ = self.guard(event="schedule", sha="", hold="2999-01-01T00:00:00Z")
        self.assertEqual(code, 0)
        self.assertTrue(outputs["skip"].startswith("held until"), outputs)
        self.assertIn("nothing is deployed", summary)
        code, outputs, _, _, _ = self.guard(
            event="schedule", sha="", hold="2020-01-01T00:00:00Z",
            **{"production-readyz": {"status": 200, "build_revision": "f" * 40},
               "staging": {"total_count": 1, "workflow_runs": [{"conclusion": "success"}]}})
        self.assertEqual(code, 0)
        self.assertEqual(outputs["skip"], "")

    def test_hold_without_a_zone_or_shape_is_refused(self):
        for hold in ("tomorrow", "2026-10-20T07:42:00", "2026-13-01T00:00:00Z"):
            with self.subTest(hold=hold):
                self.assertRefused(self.guard(event="schedule", sha="", hold=hold), "promotion hold")

    def test_manual_dispatch_ignores_the_hold_and_never_soaks(self):
        # An explicit operator dispatch is intent: hold and soak do not gate it.
        code, outputs, _, _, _ = self.guard(hold="2999-01-01T00:00:00Z")
        self.assertEqual(code, 0)
        self.assertEqual(outputs["skip"], "")
        self.assertEqual(outputs["soak_seconds"], "0")
        self.assertEqual(outputs["takeover"], "true")

    def test_recheck_passes_without_hold_and_fresh_live(self):
        code, outputs, summary, _, _ = self.recheck()
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"skip": ""})
        self.assertIn("Pre-deploy recheck passed", summary)

    def test_recheck_skips_when_the_hold_flips_after_the_guard(self):
        # The incident starts while the gates build or the soak sleeps: the
        # operator sets the hold, and the recheck stops the promotion green
        # instead of deploying through it.
        code, outputs, summary, _, _ = self.recheck(hold="2999-01-01T00:00:00Z")
        self.assertEqual(code, 0)
        self.assertTrue(outputs["skip"].startswith("held until"), outputs)
        self.assertIn("nothing is deployed", summary)
        code, outputs, _, _, _ = self.recheck(hold="2020-01-01T00:00:00Z")
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"skip": ""})

    def test_recheck_skips_live_and_older_than_live(self):
        code, outputs, _, _, _ = self.recheck(
            **{"production-readyz": {"status": 200, "build_revision": SHA}})
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"skip": "live"})
        code, outputs, _, _, _ = self.recheck(
            **{"production-readyz": {"status": 200, "build_revision": "b" * 40},
               "compare-live": {"status": "identical"}})
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"skip": "older-than-live"})

    def test_recheck_falls_through_on_unreadable_live(self):
        code, outputs, _, _, _ = self.recheck(
            **{"production-readyz": {"status": 500, "build_revision": SHA},
               "compare-live": failed_call()})
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"skip": ""})

    def test_recheck_skips_while_an_operator_dispatch_is_active(self):
        # Entering the single-pending lane behind a recovery dispatch would
        # cancel the queued run: an active dispatch stops the promotion green.
        # Completed dispatch history does not block it.
        for status in ("queued", "waiting", "in_progress"):
            with self.subTest(status=status):
                code, outputs, _, _, _ = self.recheck(
                    **{"dispatch-runs": {"workflow_runs": [
                        {"status": "completed", "conclusion": "success"},
                        {"status": status, "conclusion": None}]}})
                self.assertEqual(code, 0)
                self.assertEqual(outputs, {"skip": "operator-dispatch-active"})
        code, outputs, _, _, _ = self.recheck(
            **{"dispatch-runs": {"workflow_runs": [
                {"status": "completed", "conclusion": "success"}]}})
        self.assertEqual(code, 0)
        self.assertEqual(outputs, {"skip": ""})

    def test_recheck_skips_when_dispatch_runs_are_unreadable(self):
        # A blind run never risks the lane: when the dispatch list cannot be
        # read, the promotion skips green and the backstop retries it.
        for unreadable in (failed_call(), {"workflow_runs": [7]},
                           {"workflow_runs": None}, {"total_count": 0}):
            with self.subTest(unreadable=unreadable):
                code, outputs, _, _, _ = self.recheck(
                    **{"dispatch-runs": unreadable})
                self.assertEqual(code, 0)
                self.assertEqual(outputs, {"skip": "dispatch-check-unreadable"})

    def test_recheck_refuses_an_unparseable_hold(self):
        for hold in ("tomorrow", "2026-10-20T07:42:00"):
            with self.subTest(hold=hold):
                code, outputs, _, _, stdout = self.recheck(hold=hold)
                self.assertEqual(code, 1)
                self.assertEqual(outputs, {})
                self.assertIn("promotion hold", stdout)

    def test_reviewed_tooling_runs_from_the_workflow_commit(self):
        # A rollback targets an older SHA that predates these scripts, so
        # the guarded checkout may not contain them; reviewed tooling runs
        # from a second checkout of the workflow's own commit.
        production = "\n".join(JOBS["production"])
        self.assertIn("path: tooling", production)
        self.assertIn("ref: ${{ github.sha }}", production)
        self.assertIn("../tooling/scripts/worker_container_rollback.py", production)
        self.assertIn("../tooling/scripts/production_deploy.py", production)
        self.assertIn("../tooling/wrangler/scripts/production-ownership-control.mjs", production)
        self.assertIn("../tooling/wrangler/scripts/promotion-error-gate.mjs", production)
        self.assertIn("tooling/scripts/ci/production_schema_gate.py",
                      "\n".join(JOBS["schema-gate"]))
        roles_gate = "\n".join(JOBS["roles-gate"])
        self.assertIn("path: tooling", roles_gate)
        self.assertIn("ref: ${{ github.sha }}", roles_gate)
        self.assertIn("tooling/scripts/ci/roles_gate.py", roles_gate)

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


def report(revision=SHA, build_id=BUILD_ID, deploy_id=DEPLOY_ID):
    components = [["process", "ready"], ["gateway", "ready"], ["database", "ready"], ["token_invalid", "ready"]]
    body = {"components": components, "jobs": {}, "build_revision": revision, "build_id": build_id}
    if deploy_id is not None:
        body["deploy_id"] = deploy_id
    return json.dumps(body)


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
    def bash(self, script, cwd=ROOT, curl_script=None, **env):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            bin_dir = Path(tmp) / "bin"
            bin_dir.mkdir()
            if curl_script is not None:
                (bin_dir / "curl").write_text(curl_script)
                (bin_dir / "curl").chmod(0o755)
            (bin_dir / "sleep").write_text("#!/bin/sh\nexit 0\n")
            (bin_dir / "sleep").chmod(0o755)
            step, summary = Path(tmp) / "step.sh", Path(tmp) / "summary"
            step.write_text(script)
            run_env = {
                "PATH": f"{bin_dir}:{os.environ['PATH']}", "RUNNER_TEMP": tmp,
                "GITHUB_STEP_SUMMARY": str(summary), **env,
            }
            # The workflow runs reviewed tooling from a second checkout at
            # <workspace>/tooling (ref github.sha) while steps run in
            # <workspace>/wrangler, so inline scripts address ../tooling/....
            # Offline there is a single checkout; link it as ./tooling for the
            # run so the same relative paths resolve to the reviewed scripts.
            # (A staged link must live in the real checkout: kernels resolve
            # `..` physically, so a linked cwd would escape a temp layout.)
            tooling = ROOT / "tooling"
            staged = False
            if cwd == WRANGLER_DIR:
                if tooling.is_symlink() and os.readlink(tooling) == str(ROOT):
                    pass
                elif not tooling.exists():
                    tooling.symlink_to(ROOT, target_is_directory=True)
                    staged = True
            try:
                result = subprocess.run(
                    ["bash", "--noprofile", "--norc", "-eo", "pipefail", str(step)],
                    cwd=cwd, env=run_env, capture_output=True, text=True, timeout=120,
                )
            finally:
                if staged:
                    tooling.unlink()
            return (result.returncode, summary.read_text() if summary.exists() else "",
                    result.stdout + result.stderr)

    def gate(self, routes, mode="deploy", sha=SHA, run_id="700", attempt="1",
               deploy_id=DEPLOY_ID):
        # deploy_id=None leaves the variable unset, like a caller from before
        # the compute step existed; the gate then judges the legacy shape.
        extra = {} if deploy_id is None else {"DEPLOY_ID": deploy_id}
        with ScriptedWorker(routes) as worker:
            code, summary, _ = self.bash(GATE, cwd=WRANGLER_DIR, PRODUCTION_URL=worker.url,
                                         MODE=mode, SHA=sha,
                                         GITHUB_RUN_ID=run_id, GITHUB_RUN_ATTEMPT=attempt,
                                         **extra)
        return code, summary, worker

    def test_matching_revision_passes_only_when_gateway_ready_and_stable(self):
        code, summary, worker = self.gate(
            {"/health": [(200, "{}")], "/readyz": [(200, report())]})
        self.assertEqual(code, 0, summary)
        self.assertIn("stable 18/18 polls", summary)
        # One first pass plus 18 consecutive stability polls on each path.
        self.assertEqual(worker.count("/readyz"), 1 + 18)
        self.assertEqual(worker.count("/health"), 1 + 18)
        self.assertEqual({agent for _, agent in worker.hits}, {AGENT})

    def test_curl_size_failure_with_200_and_valid_prefix_never_passes(self):
        # curl can report HTTP 200 and leave a valid JSON prefix when its
        # body cap aborts the transfer. The exit status must still fail closed.
        curl_script = f"#!{sys.executable}\n" + textwrap.dedent('''\
            import os, pathlib, sys
            args = sys.argv[1:]
            pathlib.Path(args[args.index("-o") + 1]).write_text(os.environ["PREFIX"])
            print("200", end="")
            sys.exit(63 if args[-1].endswith(os.environ["FAILED_PATH"]) else 0)
            ''')
        for endpoint in ("/health", "/readyz"):
            for mode in ("deploy", "rollback"):
                with self.subTest(endpoint=endpoint, mode=mode):
                    code, summary, _ = self.bash(
                        GATE, cwd=WRANGLER_DIR, curl_script=curl_script,
                        PRODUCTION_URL="https://fixture.invalid", MODE=mode, SHA=SHA,
                        GITHUB_RUN_ID="700", GITHUB_RUN_ATTEMPT="1", DEPLOY_ID=DEPLOY_ID,
                        PREFIX=report(), FAILED_PATH=endpoint,
                    )
                    self.assertEqual(code, 1, summary)
                    self.assertIn("Gate failed: no fully-passing answer", summary)
                    self.assertNotIn("stable 18/18 polls", summary)

    def test_parked_gateway_never_passes(self):
        # The 10-15 incident: a crash-looping build answered 503 parked and
        # the gate certified it. Parked is now a failure that rolls back.
        parked = json.dumps({"components": [["process", "ready"], ["gateway", "down"],
                                             ["database", "ready"], ["token_invalid", "ready"]],
                             "jobs": {}, "build_revision": SHA, "build_id": BUILD_ID,
                             "deploy_id": DEPLOY_ID})
        for status in (200, 503):
            with self.subTest(status=status):
                code, summary, worker = self.gate(
                    {"/health": [(200, "{}")], "/readyz": [(status, parked)]})
                self.assertEqual(code, 1)
                self.assertIn("roll back", summary)
                # Phase 1 only: no stability window without a first pass.
                self.assertEqual(worker.count("/readyz"), 30)

    def test_stability_window_resets_on_a_miss(self):
        missing = [(200, report())] * 2 + [(503, report())] + [(200, report())] * 30
        code, summary, worker = self.gate(
            {"/health": [(200, "{}")], "/readyz": missing})
        self.assertEqual(code, 0, summary)
        self.assertIn("stable 18/18 polls", summary)
        # 2 passes, a miss resetting the streak, then 18 consecutive.
        self.assertEqual(worker.count("/readyz"), 2 + 1 + 18)

    def test_health_is_polled_through_a_cold_start(self):
        code, summary, worker = self.gate(
            {"/health": [(502, ""), (503, ""), (200, "{}")], "/readyz": [(200, report())]})
        self.assertEqual(code, 0, summary)
        self.assertEqual(worker.count("/health"), 3 + 18)

    def test_readyz_must_serve_this_revision(self):
        cases = {
            "other revision": report(revision="f" * 40),
            "unstamped build": report(revision="unknown", build_id="unknown"),
            "stale build of this revision": report(build_id="999-1"),
            "unstamped build id on this revision": report(build_id="unknown"),
            "stale deploy id of this revision": report(deploy_id=f"2026.10.20.9+{SHA[:7]}"),
            "missing deploy_id": report(deploy_id=None),
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

    def test_rollback_ignores_the_deploy_id_of_the_older_build(self):
        # The restored build reports its original stamped ID (or none), never
        # this rollback's event ID; the gate judges the revision only.
        for body in (report(deploy_id=f"2026.10.20.9+{SHA[:7]}"), report(deploy_id=None)):
            with self.subTest(body=body):
                code, summary, _ = self.gate(
                    {"/health": [(200, "{}")], "/readyz": [(200, body)]},
                    mode="rollback")
                self.assertEqual(code, 0, summary)
                self.assertIn("gateway ready", summary)

    def test_gate_without_an_exported_id_judges_the_legacy_shape(self):
        # Stamping-transition compat: without DEPLOY_ID the gate omits the
        # flag and passes a report with no deploy_id key, exactly as before.
        code, summary, _ = self.gate(
            {"/health": [(200, "{}")], "/readyz": [(200, report(deploy_id=None))]},
            deploy_id=None)
        self.assertEqual(code, 0, summary)
        self.assertIn("stable 18/18 polls", summary)

    def test_gate_judges_the_legacy_shape_for_a_pre_stamping_dockerfile(self):
        # A guarded SHA from before stamping has no BOT_DEPLOY_ID ARG in its
        # Dockerfile and its binary reports no deploy_id; the gate judges the
        # legacy revision/build-ID shape instead of failing every poll. The
        # gate runs in a sandbox workspace so the fixture Dockerfile (not the
        # checkout's) decides; reviewed tooling still resolves via a link.
        for dockerfile, expected in (
            ("FROM example.invalid/base\n", 0),
            ("FROM example.invalid/base\nARG BOT_DEPLOY_ID=unknown\n", 1),
        ):
            with self.subTest(expected=expected):
                with tempfile.TemporaryDirectory(
                        dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
                    ws = Path(tmp) / "ws"
                    (ws / "wrangler").mkdir(parents=True)
                    (ws / "Dockerfile").write_text(dockerfile)
                    (ws / "tooling").symlink_to(ROOT, target_is_directory=True)
                    with ScriptedWorker(
                            {"/health": [(200, "{}")],
                             "/readyz": [(200, report(deploy_id=None))]}) as worker:
                        code, summary, _ = self.bash(
                            GATE, cwd=ws / "wrangler", PRODUCTION_URL=worker.url,
                            MODE="deploy", SHA=SHA,
                            GITHUB_RUN_ID="700", GITHUB_RUN_ATTEMPT="1",
                            DEPLOY_ID=DEPLOY_ID)
                self.assertEqual(code, expected, summary)
                self.assertIn("stable 18/18 polls" if expected == 0 else "roll back",
                              summary)

    def test_readyz_that_moves_to_this_revision_passes(self):
        code, summary, worker = self.gate({
            "/health": [(200, "{}")],
            "/readyz": [(503, report(revision="a" * 40)), (200, report())],
        })
        self.assertEqual(code, 0, summary)
        # A miss, a first pass, then the 18-poll stability window.
        self.assertEqual(worker.count("/readyz"), 2 + 18)

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
        self.assertIn("no fully-passing answer", summary)

    def test_hostile_readyz_body_cannot_forge_workflow_commands_in_the_log(self):
        # A non-JSON 200 whose lines look like runner commands: the step log is
        # stdout+stderr, and no line of it may start a workflow command.
        body = "ok\n::error::forged by remote\n::add-mask::remote-value\n" + "B" * 4096
        with ScriptedWorker({"/health": [(200, "{}")], "/readyz": [(200, body)]}) as worker:
            code, summary, log = self.bash(GATE, cwd=WRANGLER_DIR, PRODUCTION_URL=worker.url,
                                           MODE="deploy", SHA=SHA,
                                           GITHUB_RUN_ID="700", GITHUB_RUN_ATTEMPT="1",
                                           DEPLOY_ID=DEPLOY_ID)
        self.assertEqual(code, 1)
        self.assertIn("roll back", summary)
        self.assertEqual([line for line in log.splitlines() if line.startswith("::")], [], log[:400])

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

    def test_rollback_pre_stamp_still_needs_gateway_ready(self):
        # The workflow gate polls this script: 18 parked pre-stamp answers
        # must never count as a stable pass on a pre-stamping rollback.
        parked_pre_stamp = json.dumps({"components": [["process", "ready"], ["gateway", "down"],
                                                       ["database", "ready"], ["token_invalid", "ready"]],
                                       "jobs": {}, "build_revision": "unknown", "build_id": "unknown"})
        pre_stamp = report(revision="unknown", build_id="unknown")
        for status, body in ((503, pre_stamp), (200, parked_pre_stamp), (503, parked_pre_stamp)):
            with self.subTest(status=status):
                code, summary, _ = self.gate({"/health": [(200, "{}")], "/readyz": [(status, body)]},
                                             mode="rollback")
                self.assertEqual(code, 1, summary)
                self.assertIn("roll back", summary)

    def test_production_url_must_be_https_and_not_staging(self):
        staging = "https://staging.example.workers.dev"
        for url, ok in (("", False), ("http://prod.example.workers.dev", False), (staging, False),
                        (staging + "/", False), ("https://prod.example.workers.dev", True)):
            with self.subTest(url=url):
                code, _, _ = self.bash(URL_CHECK, PRODUCTION_URL=url, STAGING_URL=staging)
                self.assertEqual(code == 0, ok)


class RenderTests(unittest.TestCase):
    def render(self, config=None, sha=SHA, run_id="700", attempt="1", deploy_id=DEPLOY_ID):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            source = WRANGLER_TOML
            if config is not None:
                source = Path(tmp) / "wrangler.toml"
                source.write_text(config)
            out = Path(tmp) / "deploy.json"
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "render", "--config", str(source), "--out", str(out),
                 "--sha", sha, "--run-id", run_id, "--attempt", attempt,
                 "--deploy-id", deploy_id],
                capture_output=True, text=True, timeout=60,
            )
            return result.returncode, json.loads(out.read_text()) if out.exists() else None

    def test_production_container_gets_the_guarded_identity_only(self):
        code, rendered = self.render(attempt="2")
        self.assertEqual(code, 0)
        self.assertEqual(rendered["env"]["production"]["containers"][0]["image_vars"],
                         {"BOT_BUILD_REVISION": SHA, "BOT_BUILD_ID": "700-2",
                          "BOT_DEPLOY_ID": DEPLOY_ID})
        self.assertEqual(rendered["env"]["production"]["vars"]["CONTAINER_RELEASE"], SHA)
        self.assertNotIn("image_vars", rendered["env"]["staging"]["containers"][0])
        self.assertNotIn("image_vars", rendered["containers"][0])

    def test_render_changes_only_paths_and_the_build_arguments(self):
        code, rendered = self.render()
        self.assertEqual(code, 0)
        original = tomllib.loads(WRANGLER_TOML.read_text())
        expected = {"/main", "/containers[0]/image", "/containers[0]/image_build_context",
                    "/env/production/containers[0]/image_vars",
                    "/env/production/vars/CONTAINER_RELEASE"}
        for env in ("staging", "production"):
            expected |= {f"/env/{env}/containers[0]/image", f"/env/{env}/containers[0]/image_build_context"}
        self.assertEqual(set(differences(original, rendered)), expected)
        self.assertEqual(rendered["main"], str(WRANGLER_DIR / "src/index.ts"))
        self.assertEqual(rendered["env"]["production"]["containers"][0]["image"], str(ROOT / "Dockerfile"))

    def test_identity_it_cannot_prove_is_refused_without_output(self):
        for sha, run_id, attempt, deploy_id in ((SHA[:39], "700", "1", DEPLOY_ID),
                                                (SHA.upper(), "700", "1", DEPLOY_ID),
                                                (SHA, "run", "1", DEPLOY_ID),
                                                (SHA, "700", "", DEPLOY_ID),
                                                (SHA, "700", "1", "bad-id"),
                                                (SHA, "700", "1", f"2026.10.21.1+{'f' * 7}")):
            with self.subTest(sha=sha, run_id=run_id, attempt=attempt, deploy_id=deploy_id):
                code, rendered = self.render(sha=sha, run_id=run_id, attempt=attempt,
                                             deploy_id=deploy_id)
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
    def judge(self, status="200", body=None, mode="deploy", sha=SHA, build_id=BUILD_ID,
              deploy_id=DEPLOY_ID):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            path = Path(tmp) / "readyz.json"
            if body is not None:
                path.write_text(body)
            command = [sys.executable, str(SCRIPT), "readyz", "--status", status, "--body", str(path),
                       "--sha", sha, "--mode", mode, "--build-id", build_id]
            if deploy_id is not None:
                command += ["--deploy-id", deploy_id]
            result = subprocess.run(command, capture_output=True, text=True, timeout=60)
            return result.returncode, result.stdout.strip()

    def test_matching_revision_passes_only_when_gateway_ready(self):
        code, message = self.judge("200", report())
        self.assertEqual((code, message.split(",")[0]), (0, "gateway ready"))
        # A parked gateway is never a pass: the gate fails and rolls back.
        parked = json.dumps({"components": [["process", "ready"], ["gateway", "down"],
                                             ["database", "ready"], ["token_invalid", "ready"]],
                             "jobs": {}, "build_revision": SHA, "build_id": BUILD_ID,
                             "deploy_id": DEPLOY_ID})
        for status in ("200", "503"):
            code, message = self.judge(status, parked)
            self.assertEqual(code, 1)
            self.assertIn("gateway not ready", message)
        code, message = self.judge("503", report())
        self.assertEqual(code, 1)
        self.assertIn("gateway not ready", message)

    def test_deploy_rejects_a_stale_build_of_the_same_sha(self):
        # The previous container still serves the SHA after a same-SHA
        # redeploy or a "Re-run failed jobs": its build id is not this run's.
        for status in ("200", "503"):
            for body in (report(build_id="999-1"), report(build_id="unknown")):
                with self.subTest(status=status, body=body):
                    code, message = self.judge(status, body)
                    self.assertEqual(code, 1)
                    self.assertIn("not this run's build", message)

    def test_deploy_rejects_a_stale_or_missing_deploy_id(self):
        # Same previous-container shape, one field over: the stamped deploy
        # ID must be this run's, and a missing key is never a pass.
        stale = f"2026.10.20.9+{SHA[:7]}"
        for status in ("200", "503"):
            for body, reason in ((report(deploy_id=stale), "not this run's deploy"),
                                 (report(deploy_id="unknown"), "not this run's deploy"),
                                 (report(deploy_id=None), "has no deploy_id")):
                with self.subTest(status=status, reason=reason):
                    code, message = self.judge(status, body)
                    self.assertEqual(code, 1)
                    self.assertIn(reason, message)

    def test_deploy_refuses_a_hostile_deploy_id_shape_without_echo(self):
        hostile = "2026.10.21.1+abc1234\n::error::forged by remote"
        code, message = self.judge("200", report(deploy_id=hostile))
        self.assertEqual(code, 1)
        self.assertEqual(message, "/readyz deploy_id has an unexpected shape")

    def test_a_malformed_expectation_refuses_before_grading(self):
        for deploy_id, reason in (("bad-id", "canonical deploy ID"),
                                  (f"2026.10.21.1+{'f' * 7}", "names a different commit")):
            with self.subTest(deploy_id=deploy_id):
                code, message = self.judge("200", report(), deploy_id=deploy_id)
                self.assertEqual(code, 1)
                self.assertIn(reason, message)

    def test_absent_deploy_id_judges_the_legacy_shape(self):
        # Stamping-transition compat: callers without an expectation (the
        # protected body-cap suite) see the pre-stamp verdicts, including a
        # pass on a report with no deploy_id key at all.
        code, message = self.judge("200", report(deploy_id=None), deploy_id=None)
        self.assertEqual(code, 0)
        self.assertIn("gateway ready", message)
        code, _ = self.judge("200", report(revision="f" * 40), deploy_id=None)
        self.assertEqual(code, 1)

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
        # A pre-stamp rollback still needs a live 200 with gateway ready:
        # parked answers never certify a build from before stamping.
        code, message = self.judge("200", report(revision="unknown", build_id="unknown"), mode="rollback")
        self.assertEqual(code, 0)
        self.assertIn("pre-stamp version", message)
        parked_pre_stamp = json.dumps({"components": [["process", "ready"], ["gateway", "down"],
                                                       ["database", "ready"], ["token_invalid", "ready"]],
                                       "jobs": {}, "build_revision": "unknown", "build_id": "unknown"})
        for status, body in (("503", report(revision="unknown", build_id="unknown")),
                             ("200", parked_pre_stamp),
                             ("503", parked_pre_stamp)):
            with self.subTest(status=status):
                code, message = self.judge(status, body, mode="rollback")
                self.assertEqual(code, 1, message)
                self.assertIn("gateway not ready", message)
        self.assertEqual(self.judge("200", report(revision="f" * 40), mode="rollback")[0], 1)
        self.assertEqual(self.judge("200", report(revision="unknown", build_id=BUILD_ID), mode="rollback")[0], 1)
        # The rolled-back version was built by an older run: its build id is
        # not this run's, and the gate must still pass on the revision.
        self.assertEqual(self.judge("200", report(build_id="999-1"), mode="rollback")[0], 0)
        # Same for the deploy ID: the restored build carries its original
        # stamped ID (or none at all), never this rollback's event ID.
        self.assertEqual(
            self.judge("200", report(deploy_id=f"2026.10.20.9+{SHA[:7]}"),
                       mode="rollback")[0], 0)
        self.assertEqual(self.judge("200", report(deploy_id=None), mode="rollback")[0], 0)

    def test_duplicate_build_revision_keys_are_refused_before_grading(self):
        # Last-wins would accept the second (correct) revision with exit 0;
        # the gate must refuse the ambiguity with a fixed single-line reason.
        wrong = "f" * 40
        bodies = (
            '{"build_revision": "%s", "build_revision": "%s", "build_id": "%s"}'
            % (wrong, SHA, BUILD_ID),
            '{"build_revision": "%s", "build_revision": "%s", "build_id": "%s"}'
            % (SHA, wrong, BUILD_ID),
            '{"build_revision": "%s", "build_id": "%s", "build_id": "%s"}'
            % (SHA, BUILD_ID, BUILD_ID),
        )
        for body in bodies:
            with self.subTest(body=body[:40]):
                for mode in ("deploy", "rollback"):
                    code, message = self.judge("200", body, mode=mode)
                    self.assertEqual(code, 1, message)
                    self.assertIn("duplicate", message)
                    self.assertNotIn("\n", message)

    def test_excessive_nesting_is_refused_with_a_bounded_message(self):
        # Iterative depth refusal: valid identity buried under deep junk
        # must not certify, and the refusal echoes no payload bytes.
        nested = "[" * 500 + "]" * 500
        body = ('{"build_revision": "%s", "build_id": "%s", "payload": %s}'
                % (SHA, BUILD_ID, nested))
        for mode in ("deploy", "rollback"):
            code, message = self.judge("200", body, mode=mode)
            self.assertEqual(code, 1, message)
            self.assertIn("deeply nested", message)
            self.assertNotIn("\n", message)
            self.assertNotIn("payload", message)

    def test_decoder_overflow_is_refused_without_a_traceback(self):
        # Deep enough to overflow the C JSON decoder on older CPython, but
        # below the body cap: the lexical pre-scan refuses it on every version.
        body = "[" * 10_000
        for mode in ("deploy", "rollback"):
            code, message = self.judge("200", body, mode=mode)
            self.assertEqual(code, 1, message)
            self.assertIn("deeply nested", message)
            self.assertNotIn("\n", message)
            self.assertNotIn("Traceback", message)

    def test_hostile_remote_identity_never_reaches_diagnostics(self):
        # Runtime-built marker: it cannot match a literal in the script, so
        # its absence proves the remote value never reached the output.
        marker = "sg%s%s%d" % ("ni", "p", id(object()))
        evils = [
            {"revision": SHA, "build_id": "700-1\nEXTRA LINE " + marker},
            {"revision": SHA, "build_id": "700-1|" + marker},
            {"revision": SHA, "build_id": "700-1\x00" + marker},
            {"revision": SHA, "build_id": "700-1\x1b" + marker},
            {"revision": SHA, "build_id": "1" * 40 + "-" + "1" * 40 + marker},
            {"revision": SHA, "build_id": "ghp_" + marker + "A" * 16},
            {"revision": SHA, "build_id": "xoxb-" + marker},
            {"revision": "ab|cd\nef" + marker, "build_id": BUILD_ID},
            {"revision": "ghp_" + marker + "A" * 16, "build_id": BUILD_ID},
            {"revision": "A" * 36 + marker[:4], "build_id": BUILD_ID},
            {"revision": SHA[:39] + "|", "build_id": BUILD_ID},
            {"revision": SHA, "build_id": "run-1"},
        ]
        for evil in evils:
            with self.subTest(revision=evil["revision"][:12], build_id=evil["build_id"][:12]):
                body = report(revision=evil["revision"], build_id=evil["build_id"])
                for mode in ("deploy", "rollback"):
                    code, message = self.judge("200", body, mode=mode)
                    self.assertEqual(code, 1, message)
                    self.assertIn("unexpected shape", message)
                    self.assertNotIn("\n", message)
                    self.assertNotIn(marker, message)
                    self.assertNotIn("|", message.split("unexpected shape")[0].split(":")[-1]
                                     if "unexpected shape" in message else message)

    def test_raw_bodies_print_as_one_bounded_escaped_line(self):
        hostile = '{"x": "ok\n::error::forged\n::add-mask::secret\r\x1b' + "A" * 5000
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            path = Path(tmp) / "readyz.json"
            path.write_text(hostile)
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "excerpt", "--body", str(path)],
                capture_output=True, text=True, timeout=60,
            )
            missing = subprocess.run(
                [sys.executable, str(SCRIPT), "excerpt", "--body", str(Path(tmp) / "absent.json")],
                capture_output=True, text=True, timeout=60,
            )
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), 1)
        self.assertFalse(lines[0].startswith("::"))
        self.assertIn("truncated", lines[0])
        self.assertLess(len(lines[0]), 2 * 2048 + 64)
        self.assertEqual((missing.returncode, missing.stderr), (0, ""))
        self.assertIn("no body", missing.stdout)
        self.assertNotIn("Traceback", missing.stdout)

    def test_valid_deploy_rollback_and_unstamped_controls_still_pass(self):
        # Shape-valid identity keeps the exact-sha/same-run, rollback and
        # pre-stamp semantics unchanged after the hardening.
        code, message = self.judge("200", report())
        self.assertEqual(code, 0)
        self.assertIn("gateway ready", message)
        self.assertIn(BUILD_ID, message)
        code, _ = self.judge("200", report(build_id="999-1"), mode="rollback")
        self.assertEqual(code, 0)
        code, message = self.judge(
            "200", report(revision="unknown", build_id="unknown"), mode="rollback")
        self.assertEqual(code, 0)
        self.assertIn("pre-stamp version", message)


# This explicit suite is CI's production-deploy entrypoint; keep the immutable
# acceptance cases in it as well as ordinary unittest discovery.
from test_immutable_bot_promotion import (
    AdmittedArtifactReadinessTests,
    AdmittedArtifactRenderTests,
    ImmutablePromotionIntakeTests,
    ImmutablePromotionProducerTests,
    ImmutablePromotionWorkflowTests,
)


# Run the native adapter suites from CI's existing production-deploy entrypoint.
# Missing suites must fail closed, not silently reduce acceptance coverage.
PROMOTION_SUITE_MODULES = (
    "test_bot_promotion_command_adapter",
    "test_bot_promotion_copy_adapter",
    "test_production_promotion_runtime_adapters",
    "test_promotion_integration_acceptance",
)


def load_tests(loader, tests, pattern):
    for name in PROMOTION_SUITE_MODULES:
        tests.addTests(loader.loadTestsFromModule(importlib.import_module(name)))
    return tests


if __name__ == "__main__":
    unittest.main()
