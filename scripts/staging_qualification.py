"""Authorize a reviewed staging source using protected Git and Actions evidence."""

import os
from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import urlencode

from build_identity import REVIEWED_STAGING_TITLE, staging_source
from rollback_proof import REPO, ProofError, run_identity
from staging_bootstrap import read_json
from staging_recovery_status import MAX_PLAN, github, matches, verified_plan
from staging_rollout import WRANGLER, GateError, decode, mapping, require, save, sequence
from worker_container_rollback import SHA, VERSION_ID

WORKFLOW = ".github/workflows/deploy-staging.yml"
PLAN = re.compile(r"([1-9][0-9]{0,19})@(sha256:[a-f0-9]{64})")
PAIR = re.compile(VERSION_ID.pattern + r"@sha256:[0-9a-f]{64}")
CHECKS = {"check.yml": ("ci-ok", "worker check"), "supply-chain.yml": ("pr-lint", "gitleaks")}
# The error-rate gate imports these Worker modules directly.
TRUSTED_PATHS = (".github/workflows", ".github/scripts", "scripts", "wrangler/scripts",
                 "wrangler/src/alert-rules.ts", "wrangler/src/metrics-scrape.ts")
# This runner is imported only by its offline Worker test.
OFFLINE_ACCEPTANCE = ":(top,exclude)wrangler/scripts/staging-voice-move-acceptance.mjs"
INCOMPATIBLE_INPUTS = ("recover_prior_sha", "release_fence", "inject_gateway_failure",
                       "inject_error_rate_spike", "inject_voice_stall",
                       "inject_unpaired_disconnect", "verify_automod_preview")


def check_git(*args):
    subprocess.run(["git", *args], check=True, timeout=25, stderr=subprocess.DEVNULL)


def collect(api, path, key, **query):
    rows = []
    for page in range(1, 11):
        suffix = urlencode(dict(query, per_page=100, page=page))
        result = mapping(api(f"{path}?{suffix}"))
        total = result.get("total_count")
        batch = sequence(result.get(key))
        require(type(total) is int and 0 <= total <= 1000, "qualification_inventory_unavailable")
        rows.extend(map(mapping, batch))
        if len(rows) == total:
            return rows
        if not batch or len(rows) > total:
            break
    raise GateError("qualification_inventory_unavailable")


def positive(value):
    return type(value) is int and value > 0


def successful(row):
    return row.get("status") == "completed" and row.get("conclusion") == "success"


def check_evidence(source, api):
    evidence = {}
    for workflow, checks in CHECKS.items():
        path = f"actions/workflows/{workflow}/runs"
        runs = collect(api, path, "workflow_runs", head_sha=source, branch="main", event="push")
        require(runs and all(run.get("head_sha") == source and positive(run.get("id"))
                             and positive(run.get("run_number")) for run in runs), "candidate_ci_unproved")
        latest = max(runs, key=lambda run: (run["run_number"], run["id"]))
        run_id = latest["id"]
        run_path = f"actions/runs/{run_id}"
        current = mapping(api(run_path))
        run_identity(current, f".github/workflows/{workflow}", "push")
        attempt, suite = current.get("run_attempt"), current.get("check_suite_id")
        require(all(current.get(key) == latest[key] for key in ("id", "head_sha", "run_number"))
                and positive(attempt) and positive(suite) and successful(current), "candidate_ci_unproved")
        jobs = collect(api, f"{run_path}/attempts/{attempt}/jobs", "jobs")
        for name in checks:
            selected = [job for job in jobs if job.get("name") == name]
            require(len(selected) == 1, "candidate_check_job_unproved")
            job = selected[0]
            require(job.get("run_id") == run_id and job.get("run_attempt") == attempt
                    and job.get("head_sha") == source and positive(job.get("id"))
                    and successful(job), "candidate_check_job_unproved")
            url = job.get("check_run_url")
            require(isinstance(url, str)
                    and re.fullmatch(rf"https://api\.github\.com/repos/{REPO}/check-runs/[1-9][0-9]{{0,19}}", url),
                    "candidate_check_job_unproved")
            params = urlencode({"check_name": name, "app_id": 15368, "filter": "latest", "per_page": 100})
            runs = sequence(mapping(api(f"commits/{source}/check-runs?{params}")).get("check_runs"))
            bound = [check for check in map(mapping, runs) if check.get("url") == url]
            require(len(bound) == 1, "candidate_check_unproved")
            check = bound[0]
            require(check.get("head_sha") == source and check.get("name") == name
                    and mapping(check.get("app")).get("id") == 15368
                    and mapping(check.get("check_suite")).get("id") == suite and successful(check),
                    "candidate_check_unproved")
        evidence[workflow] = {"run_id": run_id, "attempt": attempt}
    return evidence


def authorize(env, inputs, fresh_plan, lockfile, api=github, git=check_git):
    source, workflow, tip = (env.get(key) for key in ("SOURCE_SHA", "GITHUB_SHA", "FETCHED_MAIN_SHA"))
    plan = PLAN.fullmatch(env.get("REVIEWED_PLAN", ""))
    pair = env.get("RECOVERY_PAIR")
    require(plan and matches(PAIR, pair) and all(matches(SHA, sha) for sha in (source, workflow, tip)),
            "reviewed_inputs_invalid")
    require(env.get("GITHUB_REPOSITORY") == REPO and env.get("GITHUB_REPOSITORY_ID") == "1396224242"
            and env.get("GITHUB_EVENT_NAME") == "workflow_dispatch" and env.get("GITHUB_RUN_ATTEMPT") == "1"
            and env.get("GITHUB_REF") == "refs/heads/main" and env.get("GITHUB_REF_PROTECTED") == "true"
            and env.get("GITHUB_WORKFLOW_REF") == f"{REPO}/{WORKFLOW}@refs/heads/main"
            and env.get("GITHUB_WORKFLOW_SHA") == workflow, "untrusted_reviewed_dispatch")
    require(env.get("RECOVERY_MODE") == inputs.get("recovery_mode") == "verified"
            and inputs.get("sha") == source and inputs.get("reviewed_plan") == plan[0]
            and inputs.get("recovery_pair") == pair and env.get("VOICE_SYNTHETIC_REQUIRED") != "false"
            and all(inputs.get(key, "false") == "false" for key in INCOMPATIBLE_INPUTS),
            "reviewed_source_requires_clean_verified_recovery")
    packages = mapping(lockfile.get("packages"))
    require(mapping(packages.get("node_modules/wrangler")).get("version") == WRANGLER,
            "reviewed_source_wrangler_incompatible")
    # The run keeps the latest deployment definition even if unrelated files
    # advanced after dispatch. A changed control file requires a new dispatch.
    git("merge-base", "--is-ancestor", workflow, tip)
    git("merge-base", "--is-ancestor", source, tip)
    git("diff", "--quiet", workflow, tip, "--", *(f":(top){path}" for path in TRUSTED_PATHS),
        OFFLINE_ACCEPTANCE)
    run_id = env.get("GITHUB_RUN_ID", "")
    require(re.fullmatch(r"[1-9][0-9]{0,19}", run_id), "untrusted_reviewed_dispatch")
    current = mapping(api(f"actions/runs/{run_id}"))
    run_identity(current, WORKFLOW, "workflow_dispatch")
    require(current.get("id") == int(run_id) and current.get("head_sha") == workflow
            and current.get("run_attempt") == 1 and positive(current.get("run_number"))
            and current.get("display_title") == REVIEWED_STAGING_TITLE + source, "untrusted_reviewed_dispatch")
    latest = sequence(mapping(api("actions/workflows/deploy-staging.yml/runs?branch=main&status=success&per_page=1"))
                      .get("workflow_runs"))
    # The live stopped pair is independently re-attested immediately before
    # the write. Run history also prevents moving behind a successful staging
    # deployment, including when its production certification was skipped.
    require(len(latest) == 1, "staging_history_unproved")
    prior = mapping(latest[0])
    run_identity(prior, WORKFLOW, prior.get("event"))
    prior_source = staging_source(prior)
    require(prior.get("event") in ("push", "workflow_dispatch") and successful(prior)
            and matches(SHA, prior_source) and positive(prior.get("run_number"))
            and prior["run_number"] < current["run_number"], "reviewed_source_superseded")
    git("merge-base", "--is-ancestor", prior_source, source)
    approved = verified_plan(plan[1], api)
    require(approved["archive_digest"] == plan[2] and approved["source_sha"] == source
            and all(approved[key] == value for key, value in fresh_plan.items())
            and set(approved) - set(fresh_plan) == {"run_id", "artifact_id", "archive_digest"},
            "reviewed_plan_mismatch")
    checks = check_evidence(source, api)
    return {"source_sha": source, "workflow_sha": workflow, "checked_main_sha": tip,
            "run_id": int(run_id), "attempt": 1, "checks": checks, "plan": approved,
            "recovery_pair": pair}


def main(env):
    try:
        event = mapping(decode(Path(env["GITHUB_EVENT_PATH"]).read_bytes()))
        lockfile = read_json(Path("package-lock.json"), MAX_PLAN)
        evidence = authorize(env, mapping(event.get("inputs")), mapping(decode(env["FRESH_PLAN"])), lockfile)
        save(os.path.join(env["ROLLOUT_DIR"], "reviewed-staging-source.json"), evidence)
        with open(env["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            print(f"source_sha={evidence['source_sha']}", file=output)
        return 0
    except (GateError, ProofError) as error:
        print(f"Reviewed staging source refused: {error}", file=sys.stderr)
    except (KeyError, OSError, TypeError, ValueError, subprocess.SubprocessError):
        print("Reviewed staging source refused: invalid or unavailable evidence or changed workflow/history",
              file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main(os.environ))
