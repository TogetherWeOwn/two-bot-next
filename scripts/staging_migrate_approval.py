#!/usr/bin/env python3
"""Bind native staging Environment approval to one migration request."""

from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import sys
from urllib.request import Request

from ci.staging_migrate_claim import DECIMAL, MAX_MANIFEST_BYTES, SHA256, Refused, build_claim
from rollback_proof import ProofError, moment, run_identity
from script_intake import decode_json, no_redirect_opener, read_with_overflow
from staging_recovery_status import PLAN_WORKFLOW, authenticated_plan
from staging_rollout import GateError, mapping, require, sequence

REPO = "TogetherWeOwn/two-bot-next"
ENVIRONMENT = "staging-migrate-apply"
POLICY = f"environments/{ENVIRONMENT}"
APPROVE = "approve-staging-migration sha256:"
MAX_REVIEW_AGE = timedelta(hours=23, minutes=30)  # Leave the 30-minute apply job its full window.
MAX_RESPONSE = 128 * 1024


def digest_json(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def request_context(env):
    require(env.get("GITHUB_REPOSITORY") == REPO
            and env.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
            and env.get("GITHUB_REF") == "refs/heads/main"
            and env.get("GITHUB_REF_PROTECTED") == "true"
            and env.get("GITHUB_WORKFLOW_REF") == f"{REPO}/{PLAN_WORKFLOW}@refs/heads/main"
            and env.get("GITHUB_WORKFLOW_SHA") == env.get("GITHUB_SHA")
            and env.get("GITHUB_RUN_ATTEMPT") == "1"
            and DECIMAL.fullmatch(env.get("GITHUB_RUN_ID", "")), "invalid_approval_dispatch")
    return f"actions/runs/{env['GITHUB_RUN_ID']}"


def prepare(current, claim, env, api, fetch_plan=authenticated_plan):
    request_context(env)
    actor = env.get("GITHUB_ACTOR_ID", "")
    require(DECIMAL.fullmatch(actor), "approval_requester_unverified")
    _, allowed = environment_reviewers(api)
    require(allowed - {int(actor)}, "staging_independent_reviewer_missing")
    producing, artifact = fetch_plan(claim["plan_run_id"])
    require(build_claim(current, env) == build_claim(producing, env) == claim,
            "producing_plan_does_not_match_claim")
    for manifest in (current, producing):
        require(mapping(manifest.get("audit")).get("verify_findings") == [], "staging_role_findings")
    require(all(current[key] == producing[key] for key in ("ledger_before", "ledger_store_before")),
            "staging_ledger_changed_since_plan")
    return digest_json({"claim": claim, "producing_plan": artifact}), digest_json(producing), artifact


def identity(user, kinds=("User",)):
    user = mapping(user)
    uid = user.get("id")
    require(type(uid) is int and uid > 0 and user.get("type") in kinds, "invalid_github_user")
    return uid


def environment_reviewers(api):
    settings = mapping(api(POLICY))
    environment_id = settings.get("id")
    policy = mapping(settings.get("deployment_branch_policy"))
    branches = mapping(api(f"{POLICY}/deployment-branch-policies?per_page=2"))
    require(settings.get("name") == ENVIRONMENT and type(environment_id) is int and environment_id > 0
            and settings.get("can_admins_bypass") is False
            and policy.get("custom_branch_policies") is True and policy.get("protected_branches") is False
            and branches.get("total_count") == 1
            and [(mapping(row).get("type"), row.get("name")) for row in sequence(branches.get("branch_policies"))]
            == [("branch", "main")], "staging_approval_environment_unprotected")
    rules = [mapping(row) for row in sequence(settings.get("protection_rules"))]
    reviewer_rules = [rule for rule in rules if rule.get("type") == "required_reviewers"]
    require(sum(rule.get("type") == "branch_policy" for rule in rules) == 1
            and len(reviewer_rules) == 1 and reviewer_rules[0].get("prevent_self_review") is True,
            "staging_independent_reviewer_rule_missing")
    reviewers = [mapping(row) for row in sequence(reviewer_rules[0].get("reviewers"))]
    require(1 <= len(reviewers) <= 6 and all(row.get("type") == "User" for row in reviewers),
            "staging_reviewer_designation_invalid")
    return environment_id, {identity(row.get("reviewer")) for row in reviewers}


def verify(env, producing, api, now):
    route = request_context(env)
    digest = env.get("APPROVAL_DIGEST", "")
    require(SHA256.fullmatch(digest), "missing_approval_claim_digest")
    require(digest_json(producing) == env.get("APPROVAL_PLAN_SHA256"), "staging_approved_plan_changed")
    run = mapping(api(route))
    run_identity(run, PLAN_WORKFLOW, "workflow_dispatch")
    actor = identity(run.get("actor"), ("User", "Bot"))
    triggering_actor = identity(run.get("triggering_actor"), ("User", "Bot"))
    require(run.get("id") == int(env["GITHUB_RUN_ID"]) and run.get("run_attempt") == 1
            and run.get("head_sha") == env.get("GITHUB_SHA") and actor == triggering_actor
            and str(actor) == env.get("GITHUB_ACTOR_ID"), "approval_requester_unverified")
    require(timedelta() <= now - moment(run.get("created_at")) <= MAX_REVIEW_AGE, "approval_request_expired")
    environment_id, allowed = environment_reviewers(api)

    decisions = []
    for entry in sequence(api(f"{route}/approvals")):
        entry = mapping(entry)
        for item in sequence(entry.get("environments")):
            item = mapping(item)
            if item.get("id") == environment_id or item.get("name") == ENVIRONMENT:
                require(item.get("id") == environment_id and item.get("name") == ENVIRONMENT,
                        "staging_approval_environment_changed")
                decisions.append(entry)
    require(len(decisions) == 1 and decisions[0].get("state") == "approved"
            and decisions[0].get("comment") == APPROVE + digest, "staging_approval_missing_or_changed")
    reviewer = identity(decisions[0].get("user"))
    require(reviewer in allowed and reviewer != actor, "staging_approval_reviewer_unauthorized")
    return reviewer


def api_get(path):
    request = Request(f"https://api.github.com/repos/{REPO}/{path}", headers={
        "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
        "Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2026-03-10",
    })
    with no_redirect_opener(use_proxy=False).open(request, timeout=10) as response:
        data = read_with_overflow(response, MAX_RESPONSE)
    require(len(data) <= MAX_RESPONSE, "approval_response_too_large")
    return decode_json(data)


def load_document(path):
    with Path(path).open("rb") as reader:
        data = read_with_overflow(reader, MAX_MANIFEST_BYTES)
    require(len(data) <= MAX_MANIFEST_BYTES, "approval_document_too_large")
    return decode_json(data)


def main(args, env):
    try:
        if len(args) == 3 and args[0] == "prepare":
            current, claim = [load_document(path) for path in args[1:]]
            digest, plan_digest, artifact = prepare(current, claim, env, api_get)
            with Path(env["GITHUB_OUTPUT"]).open("a") as output:
                print(f"digest={digest}", file=output)
                print(f"plan_sha256={plan_digest}", file=output)
            with Path(env["GITHUB_STEP_SUMMARY"]).open("a") as summary:
                print(f"Review the apply claim and producing plan artifact `{artifact['artifact_id']}` "
                      f"(`{artifact['archive_digest']}`). Enter `{APPROVE}{digest}` in the Environment approval.",
                      file=summary)
        elif len(args) == 2 and args[0] == "verify":
            reviewer = verify(env, load_document(args[1]), api_get, datetime.now(timezone.utc))
            print(f"Verified staging approval by GitHub user ID {reviewer} for claim {env['APPROVAL_DIGEST']}")
        else:
            raise Refused("invalid approval command")
    except (GateError, ProofError, Refused) as error:
        print(f"staging migration approval refused: {error}", file=sys.stderr)
        return 2
    except (OSError, ValueError, KeyError, RecursionError):
        print("staging migration approval evidence unavailable or malformed", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
