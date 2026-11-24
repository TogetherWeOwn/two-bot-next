#!/usr/bin/env python3
"""Require independent approval of the exact read-only production migration plan."""

from datetime import datetime, timedelta, timezone
import os
from pathlib import Path
import re
import sys
from urllib.parse import urlencode

from ci.production_migrate_claim import DECIMAL, REPOSITORY, SHA40, SHA256, WORKFLOW_PATH, Refused, build_claim
from rollback_proof import ProofError, moment, run_identity
from staging_migrate_approval import MAX_REVIEW_AGE, api_get, digest_json, load_document
from staging_migrate_reviewed import APP_BOT_ID, APP_ID
from staging_recovery_status import authenticated_plan, github
from staging_rollout import GateError, mapping, require, sequence

ARTIFACT = "production-migrate-manifest"
CHECK_NAME = "production-migration-approval"
ENVIRONMENT = "production-migrate-apply"
POLICY = f"environments/{ENVIRONMENT}"
EXTERNAL_ID = re.compile(r"production-apply:[1-9][0-9]{0,19}:[0-9a-f]{64}")


def stable_plan(plan):
    audit = mapping(plan.get("audit"))
    sequence(audit.get("verify_findings"))
    require(type(plan.get("role_verified_connections")) is int and plan["role_verified_connections"] > 0,
            "production_plan_role_unverified")
    # build_claim already verifies the effective pending lists. The caller's
    # optional hint and the number of verified connections can vary between runs.
    result = {key: value for key, value in plan.items() if key not in ("expected_pending", "role_verified_connections")}
    result["audit"] = {key: value for key, value in audit.items() if key != "connection_usage"}
    return result


def verify_environment(api):
    settings = mapping(api(POLICY))
    policy = mapping(settings.get("deployment_branch_policy"))
    branches = mapping(api(f"{POLICY}/deployment-branch-policies?per_page=2"))
    rules = sequence(settings.get("protection_rules"))
    require(settings.get("name") == ENVIRONMENT and settings.get("can_admins_bypass") is False
            and policy.get("custom_branch_policies") is True and policy.get("protected_branches") is False
            and sum(mapping(rule).get("type") == "branch_policy" for rule in rules) == 1
            and branches.get("total_count") == 1
            and [(mapping(row).get("type"), row.get("name")) for row in sequence(branches.get("branch_policies"))]
            == [("branch", "main")], "production_migration_environment_unprotected")


def dispatch(env, api):
    sha, run_id, actor = (env.get(key, "") for key in ("GITHUB_SHA", "GITHUB_RUN_ID", "GITHUB_ACTOR_ID"))
    require(env.get("GITHUB_REPOSITORY") == REPOSITORY and env.get("GITHUB_REPOSITORY_ID") == "1396224242"
            and env.get("GITHUB_EVENT_NAME") == "workflow_dispatch" and env.get("GITHUB_REF") == "refs/heads/main"
            and env.get("GITHUB_REF_PROTECTED") == "true" and env.get("GITHUB_WORKFLOW_SHA") == sha
            and env.get("GITHUB_WORKFLOW_REF") == f"{REPOSITORY}/{WORKFLOW_PATH}@refs/heads/main"
            and env.get("GITHUB_RUN_ATTEMPT") == "1" and SHA40.fullmatch(sha) and DECIMAL.fullmatch(run_id)
            and DECIMAL.fullmatch(actor) and actor != str(APP_BOT_ID), "production_approval_dispatch_untrusted")
    run = mapping(api(f"actions/runs/{run_id}"))
    run_identity(run, WORKFLOW_PATH, "workflow_dispatch")
    require(run.get("id") == int(run_id) and run.get("run_attempt") == 1 and run.get("head_sha") == sha
            and mapping(run.get("actor")).get("id") == mapping(run.get("triggering_actor")).get("id") == int(actor),
            "production_approval_dispatch_untrusted")
    return run


def verify_check(api, run, external_id, now):
    require(EXTERNAL_ID.fullmatch(external_id), "independent_production_approval_invalid")
    sha = run["head_sha"]
    checks = mapping(api(f"commits/{sha}/check-runs?check_name={CHECK_NAME}&filter=all&per_page=30"))
    rows = sequence(checks.get("check_runs"))
    require(checks.get("total_count") == len(rows) <= 30, "production_approval_check_listing_incomplete")
    matching = [row for row in rows if mapping(row).get("external_id") == external_id
                and mapping(row.get("app")).get("id") == APP_ID]
    require(len(matching) == 1, "independent_production_approval_missing_or_ambiguous")
    check = matching[0]
    require(check.get("name") == CHECK_NAME and check.get("head_sha") == sha
            and check.get("status") == "completed" and check.get("conclusion") == "success"
            and type(check.get("id")) is int and DECIMAL.fullmatch(str(check["id"])),
            "independent_production_approval_invalid")
    approved_at = moment(check.get("completed_at"))
    require(approved_at <= moment(run.get("created_at"))
            and timedelta() <= now - approved_at <= MAX_REVIEW_AGE, "independent_production_approval_stale")
    return check["id"], approved_at


def require_unused_plan(api, producing_id, applying_id, since):
    # GitHub supplies the plan run creation time. A replacement App check cannot
    # move this boundary forward to hide a previous apply attempt.
    timestamp = since.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    query = urlencode({"event": "workflow_dispatch", "branch": "main", "created": f">={timestamp}", "per_page": 30})
    listing = mapping(api(f"actions/workflows/production-migrate.yml/runs?{query}"))
    rows = sequence(listing.get("workflow_runs"))
    require(listing.get("total_count") == len(rows) <= 30, "production_apply_history_incomplete")
    for row in rows:
        run_id = mapping(row).get("id")
        require(type(run_id) is int and DECIMAL.fullmatch(str(run_id)), "production_apply_history_invalid")
        if run_id in (producing_id, applying_id):
            continue
        jobs = mapping(api(f"actions/runs/{run_id}/attempts/1/jobs?per_page=4"))
        entries = sequence(jobs.get("jobs"))
        require(jobs.get("total_count") == len(entries) <= 4, "production_apply_history_incomplete")
        require(not any(mapping(job).get("name") == "production-migrate (apply)" and job.get("started_at")
                        and job.get("conclusion") != "skipped" for job in entries),
                "production_plan_superseded_by_apply_attempt")


def authorize(fresh, claim, env, now, api=github):
    run = dispatch(env, api)
    verify_environment(api)
    require(build_claim(fresh, env) == claim, "production_approval_claim_changed")
    require(claim["expected_pending"] or claim["expected_store_pending"], "production_migration_plan_empty")
    producing, artifact = authenticated_plan(claim["plan_run_id"], api, workflow=WORKFLOW_PATH, artifact_name=ARTIFACT)
    require(build_claim(producing, env) == claim and stable_plan(producing) == stable_plan(fresh),
            "production_migration_plan_changed")
    external_id = f"production-apply:{claim['plan_run_id']}:{artifact['archive_digest'].removeprefix('sha256:')}"
    check_id, approved_at = verify_check(api, run, external_id, now)
    plan_run = mapping(api(f"actions/runs/{claim['plan_run_id']}"))
    created = moment(plan_run.get("created_at"))
    require(created <= approved_at, "independent_production_approval_stale")
    require_unused_plan(api, artifact["run_id"], run["id"], created)
    atomic_release = claim["expected_pending"] == ["475", "479", "480", "481", "482"] and claim["expected_store_pending"] == ["478"]
    return {"external_id": external_id, "check_id": check_id, "plan_sha256": digest_json(producing),
            "atomic_release": "true" if atomic_release else "false"}


def recheck(producing, env, now, api=api_get):
    run = dispatch(env, api)
    verify_environment(api)
    approved_digest = env.get("APPROVAL_PLAN_SHA256", "")
    require(SHA256.fullmatch(approved_digest) and digest_json(producing) == approved_digest,
            "production_approval_plan_changed")
    check_id, _ = verify_check(api, run, env.get("APPROVAL_EXTERNAL_ID", ""), now)
    require(str(check_id) == env.get("APPROVAL_CHECK_ID"), "independent_production_approval_changed")


def main(args, env):
    try:
        now = datetime.now(timezone.utc)
        if len(args) == 3 and args[0] == "authorize":
            fresh, claim = (load_document(path) for path in args[1:])
            approval = authorize(fresh, claim, env, now)
            with Path(env["GITHUB_OUTPUT"]).open("a") as output:
                for key, value in approval.items():
                    print(f"{key}={value}", file=output)
        elif len(args) == 2 and args[0] == "recheck":
            recheck(load_document(args[1]), env, now)
        else:
            raise GateError("invalid_production_approval_command")
    except (GateError, ProofError, Refused) as error:
        print(f"production migration refused: {error}", file=sys.stderr)
        return 2
    except (AttributeError, KeyError, OSError, TypeError, ValueError, RecursionError):
        print("production migration approval evidence unavailable or malformed", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
