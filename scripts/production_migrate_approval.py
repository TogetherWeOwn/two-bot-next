#!/usr/bin/env python3
"""Require independent approval of the exact read-only production migration plan."""

from datetime import datetime, timedelta, timezone
import os
import sys

from ci.production_migrate_claim import DECIMAL, REPOSITORY, WORKFLOW_PATH, Refused, build_claim
from rollback_proof import ProofError, moment, run_identity
from staging_migrate_approval import MAX_REVIEW_AGE, load_document
from staging_migrate_reviewed import APP_BOT_ID, APP_ID
from staging_recovery_status import authenticated_plan, github
from staging_rollout import GateError, mapping, require, sequence

ARTIFACT = "production-migrate-manifest"
CHECK_NAME = "production-migration-approval"


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


def authorize(fresh, claim, env, now, api=github):
    require(env.get("GITHUB_REF_PROTECTED") == "true" and env.get("GITHUB_WORKFLOW_SHA") == env.get("GITHUB_SHA")
            and env.get("GITHUB_RUN_ATTEMPT") == "1", "production_approval_dispatch_untrusted")
    require(build_claim(fresh, env) == claim, "production_approval_claim_changed")
    actor = env.get("GITHUB_ACTOR_ID", "")
    require(DECIMAL.fullmatch(actor) and actor != str(APP_BOT_ID), "production_approval_requester_unverified")
    run = mapping(api(f"actions/runs/{claim['apply_run_id']}"))
    run_identity(run, WORKFLOW_PATH, "workflow_dispatch")
    require(run.get("id") == int(claim["apply_run_id"]) and run.get("run_attempt") == 1
            and run.get("head_sha") == claim["workflow_head_sha"]
            and mapping(run.get("actor")).get("id") == mapping(run.get("triggering_actor")).get("id") == int(actor),
            "production_approval_dispatch_untrusted")

    producing, artifact = authenticated_plan(claim["plan_run_id"], api, workflow=WORKFLOW_PATH, artifact_name=ARTIFACT)
    require(build_claim(producing, env) == claim and stable_plan(producing) == stable_plan(fresh)
            and bool(claim["expected_pending"] or claim["expected_store_pending"]), "production_migration_plan_changed")
    external_id = f"production-apply:{claim['plan_run_id']}:{artifact['archive_digest'].removeprefix('sha256:')}"
    sha = claim["workflow_head_sha"]
    checks = mapping(api(f"commits/{sha}/check-runs?check_name={CHECK_NAME}&filter=all&per_page=100"))
    rows = sequence(checks.get("check_runs"))
    require(checks.get("total_count") == len(rows) <= 100, "production_approval_check_listing_incomplete")
    matching = [row for row in rows if mapping(row).get("external_id") == external_id
                and mapping(row.get("app")).get("id") == APP_ID]
    require(len(matching) == 1, "independent_production_approval_missing_or_ambiguous")
    check = matching[0]
    require(check.get("name") == CHECK_NAME and check.get("head_sha") == sha
            and check.get("status") == "completed" and check.get("conclusion") == "success",
            "independent_production_approval_invalid")
    approved_at = moment(check.get("completed_at"))
    require(approved_at <= moment(run.get("created_at"))
            and timedelta() <= now - approved_at <= MAX_REVIEW_AGE, "independent_production_approval_stale")
    return artifact["artifact_id"]


def main(args, env):
    try:
        require(len(args) == 2 and env.get("GITHUB_REPOSITORY") == REPOSITORY, "invalid_production_approval_command")
        fresh, claim = (load_document(path) for path in args)
        artifact_id = authorize(fresh, claim, env, datetime.now(timezone.utc))
        print(f"Independent production migration approval verified for plan artifact {artifact_id}")
    except (GateError, ProofError, Refused) as error:
        print(f"production migration refused: {error}", file=sys.stderr)
        return 2
    except (KeyError, OSError, TypeError, ValueError, RecursionError):
        print("production migration approval evidence unavailable or malformed", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
