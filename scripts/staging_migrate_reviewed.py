#!/usr/bin/env python3
"""Consume a staging migration request introduced by a protected squash merge."""

from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import sys

from ci.staging_migrate_claim import DECIMAL, SHA40, Refused, versions
from rollback_proof import ProofError, moment, run_identity
from script_intake import decode_json
from staging_migrate_approval import MAX_REVIEW_AGE, load_document
from staging_recovery_status import authenticated_plan, github
from staging_rollout import GateError, mapping, require

REPO = "TogetherWeOwn/two-bot-next"
WORKFLOW = "staging-migrate-reviewed.yml"
REQUEST = re.compile(r"\.github/staging-apply/[a-f0-9]{32}\.json")
RECOVERY = re.compile(r"staging-recovery-capture/([1-9][0-9]{0,19})/artifacts/([1-9][0-9]{0,19})")
DIGEST = re.compile(r"sha256:[a-f0-9]{64}")


def git(*args):
    return subprocess.check_output(["git", *args], stderr=subprocess.DEVNULL, timeout=25).strip()


def parse_request(raw):
    require(len(raw) <= 1024, "request_too_large")
    value = mapping(decode_json(raw))
    require(set(value) == {"plan_run_id", "plan_archive_digest", "recovery_archive_digest"}
            and DECIMAL.fullmatch(value["plan_run_id"])
            and all(DIGEST.fullmatch(value[key]) for key in ("plan_archive_digest", "recovery_archive_digest")),
            "invalid_staging_request")
    return value


def context(env, event, api):
    sha, before = env.get("GITHUB_SHA", ""), event.get("before", "")
    require(env.get("GITHUB_REPOSITORY") == REPO and env.get("GITHUB_EVENT_NAME") == "push"
            and env.get("GITHUB_REF_PROTECTED") == "true" and env.get("GITHUB_REF") == event.get("ref") == "refs/heads/main"
            and env.get("GITHUB_WORKFLOW_REF") == f"{REPO}/.github/workflows/{WORKFLOW}@refs/heads/main"
            and SHA40.fullmatch(sha) and SHA40.fullmatch(before) and event.get("after") == sha
            and env.get("GITHUB_WORKFLOW_SHA") == sha and env.get("GITHUB_RUN_ATTEMPT") == "1"
            and DECIMAL.fullmatch(env.get("GITHUB_RUN_ID", ""))
            and all(event.get(key) is False for key in ("forced", "created", "deleted")), "invalid_request_delivery")
    runs = api(f"actions/workflows/{WORKFLOW}/runs?event=push&head_sha={sha}&branch=main&per_page=2")
    require(runs["total_count"] == 1 and [run["id"] for run in runs["workflow_runs"]] == [int(env["GITHUB_RUN_ID"])],
            "request_delivery_not_unique")
    return sha, before


def still_fresh(merged_at, now):
    age = now - moment(merged_at)
    require(age.days >= 0 and age <= MAX_REVIEW_AGE, "reviewed_request_expired")


def authorize(env, event, api, now, run_git=git, fetch_plan=authenticated_plan):
    sha, before = context(env, event, api)
    require(run_git("show", "-s", "--format=%P", sha) == before.encode(), "request_not_single_commit")
    change = run_git("diff", "--no-renames", "--name-status", before, sha).decode().split("\t")
    require(len(change) == 2 and change[0] == "A" and REQUEST.fullmatch(change[1]), "request_must_be_only_change")
    path = change[1]
    require(not run_git("log", "-1", "--format=%H", before, "--", path), "request_already_introduced")
    require(run_git("ls-tree", sha, "--", path).startswith(b"100644 blob ")
            and int(run_git("cat-file", "-s", f"{sha}:{path}")) <= 1024, "invalid_request_blob")

    pulls = api(f"commits/{sha}/pulls?per_page=2")
    require(isinstance(pulls, list) and len(pulls) == 1, "request_merge_unproved")
    pull = pulls[0]
    head = pull["head"]["sha"]
    number = pull["number"]
    require(pull["merge_commit_sha"] == sha and pull["state"] == "closed" and pull["base"]["ref"] == "main"
            and all(pull[key]["repo"]["full_name"] == REPO for key in ("base", "head"))
            and SHA40.fullmatch(head) and type(number) is int and number > 0, "request_merge_unproved")
    still_fresh(pull["merged_at"], now)
    checks = api(f"commits/{head}/check-runs?check_name=ci-ok&per_page=10")["check_runs"]
    require(any(check["app"]["id"] == 15368 and check["name"] == "ci-ok" and check["head_sha"] == head
                and check["conclusion"] == "success" for check in checks), "request_head_ci_unproved")
    run_git("fetch", "--no-tags", "origin", f"refs/pull/{number}/head")
    require(run_git("rev-parse", "FETCH_HEAD") == head.encode()
            and run_git("rev-parse", f"{head}:{path}") == run_git("rev-parse", f"{sha}:{path}"), "reviewed_request_changed")

    request = parse_request(run_git("show", f"{sha}:{path}"))
    plan, artifact = fetch_plan(request["plan_run_id"])
    recovery = RECOVERY.fullmatch(plan["recovery_evidence_ref"])
    require(artifact["archive_digest"] == request["plan_archive_digest"] and recovery
            and plan["mode"] == "plan" and plan["migration_target"] == "staging" and plan["role"] == "two_bot_migrator_ro"
            and SHA40.fullmatch(plan["source_sha"]) and (plan["pending_before"] or plan["pending_store_before"]),
            "reviewed_staging_plan_unproved")
    run_git("merge-base", "--is-ancestor", plan["source_sha"], before)
    capture = api(f"actions/runs/{recovery[1]}")
    run_identity(capture, ".github/workflows/staging-recovery-capture.yml", "workflow_dispatch")
    receipt = api(f"actions/artifacts/{recovery[2]}")
    require(capture["id"] == int(recovery[1]) and capture["run_attempt"] == 1 and capture["conclusion"] == "success"
            and receipt["name"] == "staging-recovery-capture" and receipt["expired"] is False
            and receipt["workflow_run"]["id"] == int(recovery[1])
            and receipt["digest"] == request["recovery_archive_digest"], "reviewed_recovery_unproved")
    return {"plan": plan, "plan_run_id": request["plan_run_id"], "merged_at": pull["merged_at"], "request_sha": sha}


def compare_plans(approved, fresh):
    def stable(plan):
        audit = mapping(plan["audit"])
        require(isinstance(audit.get("verify_findings"), list) and plan["role_verified_connections"] > 0,
                "staging_plan_audit_missing")
        return {**plan, "audit": {k: v for k, v in audit.items() if k != "connection_usage"}}

    require(stable(approved) == stable(fresh), "staging_plan_changed")


def run(mode, claim, env):
    plan, target = claim["plan"], claim["plan"]["target"]
    require(claim["request_sha"] == env["GITHUB_SHA"] and env["GITHUB_RUN_ATTEMPT"] == "1", "request_delivery_changed")
    still_fresh(claim["merged_at"], datetime.now(timezone.utc))
    flags = ["--source-sha", plan["source_sha"], "--staging-host", target["host"], "--staging-database", target["database"],
             "--staging-branch-id", target["branch_id"], "--recovery-evidence-ref", plan["recovery_evidence_ref"],
             "--acl-plan-ref", plan["acl_plan_ref"]]
    expected = plan["expected_pending"] if mode == "plan" else plan["pending_before"]
    if expected is not None:
        flags += ["--expected-pending", ",".join(map(str, versions(expected)))]
    if mode == "apply":
        compare_plans(plan, load_document("fresh.json"))
        Path("producing.json").write_text(json.dumps(plan))
        flags += ["--expected-store-pending", ",".join(map(str, versions(plan["pending_store_before"]))),
                  "--plan-manifest-sha256", plan["plan_manifest_sha256"],
                  "--store-plan-manifest-sha256", plan["store_plan_manifest_sha256"],
                  "--plan-run-id", claim["plan_run_id"], "--plan-manifest-path", "producing.json"]
    subprocess.run(["source/target/debug/staging-migrate", "--target", "staging", f"--{mode}", *flags], check=True)


def main(mode, env):
    try:
        if mode == "authorize":
            claim = authorize(env, load_document(env["GITHUB_EVENT_PATH"]), github, datetime.now(timezone.utc))
            Path("approved.json").write_text(json.dumps(claim))
            with Path(env["GITHUB_OUTPUT"]).open("a") as output:
                print(f"source_sha={claim['plan']['source_sha']}", file=output)
        elif mode == "verify":
            context(env, load_document(env["GITHUB_EVENT_PATH"]), github)
            claim = load_document("approved.json")
            still_fresh(claim["merged_at"], datetime.now(timezone.utc))
            compare_plans(claim["plan"], load_document("fresh.json"))
        elif mode in ("plan", "apply"):
            run(mode, load_document("approved.json"), env)
        else:
            raise GateError("invalid_reviewed_migration_command")
    except (GateError, ProofError, Refused) as error:
        print(f"reviewed staging migration refused: {error}", file=sys.stderr)
        return 2
    except (KeyError, OSError, ValueError, TypeError, subprocess.SubprocessError):
        print("reviewed staging migration refused or failed", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], os.environ))
