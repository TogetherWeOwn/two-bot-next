#!/usr/bin/env python3
"""Run or watch the daily staging-only automatic rollback proof.

`run --dry-run` prints the fixed dispatch contract without any network calls.
`run` dispatches deploy-staging on main exactly once, using GitHub REST
2026-03-10's returned workflow_run_id (never a search for the latest run).
The child MUST fail its error-rate gate and successfully restore both snapshot
members. A green child is a failure of the drill. `watch` is read-only except
for one Discord finding through the existing synthetic-alerts binding.
"""

import argparse
from datetime import datetime, timedelta, timezone
import importlib
import io
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import sys
import time
import zipfile

REPO = "TogetherWeOwn/two-bot-next"
WORKFLOW = ".github/workflows/deploy-staging.yml"
PROOF_WORKFLOW = ".github/workflows/staging-rollback-drill.yml"
ARTIFACT = "staging-rollback-proof"
MAX_BYTES = 2 << 20
GH_TIMEOUT = 45
REHEARSAL_TITLE = "staging rollback proof"
MAX_WAIT = 70 * 60
MAX_AGE = timedelta(hours=30)
DISPATCH = {"ref": "main", "inputs": {"inject_error_rate_spike": True}}
SHA = re.compile(r"[0-9a-f]{40}")
VERSION = re.compile(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
# Actions exposes step names, not YAML step ids. Keep these names pinned.
REQUIRED_STEPS = {
    "Snapshot the pre-deploy rollback state": "success",
    "Deploy to staging": "success",
    "Verify exact new rollout and serving Worker/container readiness": "success",
    "Gate promotion on REST 5xx error rate": "failure",
    "Auto-roll back Worker version and container image": "success",
    "Upload the rollout evidence": "success",
}


class ProofError(Exception):
    """Fixed-vocabulary error, never a remote response or subprocess stderr."""


def require(condition, code):
    if not condition:
        raise ProofError(code)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate_json_key")
        result[key] = value
    return result


def gh(path, *, body=None, binary=False):
    args = ["gh", "api", "--method", "POST" if body is not None else "GET",
            "-H", "X-GitHub-Api-Version: 2026-03-10", path]
    if body is not None:
        args += ["--input", "-"]
    payload = json.dumps(body).encode() if body is not None else b""
    # Dispatch bodies are tiny; never block writing a pipe-full of stdin before
    # starting the readers. Drain BOTH output pipes, bounding bytes as received.
    require(len(payload) <= 4096, "github_request_too_large")
    output = bytearray()
    try:
        with subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE) as child:
            try:
                deadline = time.monotonic() + GH_TIMEOUT
                child.stdin.write(payload)
                child.stdin.close()
                with selectors.DefaultSelector() as selector:
                    selector.register(child.stdout, selectors.EVENT_READ, "stdout")
                    selector.register(child.stderr, selectors.EVENT_READ, "stderr")
                    sizes = {"stdout": 0, "stderr": 0}
                    while selector.get_map():
                        remaining = deadline - time.monotonic()
                        require(remaining > 0, "github_transport_failure")
                        for key, _ in selector.select(remaining):
                            stream = key.data
                            chunk = os.read(key.fd, min(65536, MAX_BYTES - sizes[stream] + 1))
                            if not chunk:
                                selector.unregister(key.fileobj)
                                continue
                            sizes[stream] += len(chunk)
                            require(sizes[stream] <= MAX_BYTES, "github_response_too_large")
                            if stream == "stdout":
                                output.extend(chunk)
                child.wait(timeout=max(0.001, deadline - time.monotonic()))
                require(child.returncode == 0, "github_request_failed_no_retry")
            finally:
                if child.poll() is None:
                    child.kill()
                child.wait()
    except (OSError, subprocess.TimeoutExpired):
        raise ProofError("github_transport_failure") from None
    if binary:
        return bytes(output)
    try:
        return json.loads(output, object_pairs_hook=unique_object)
    except (ValueError, RecursionError):
        raise ProofError("github_invalid_json") from None


def moment(raw):
    try:
        value = datetime.fromisoformat(raw.replace("Z", "+00:00"))
        require(value.tzinfo is not None, "timestamp_missing_timezone")
        return value
    except (AttributeError, TypeError, ValueError):
        raise ProofError("invalid_timestamp") from None


def run_identity(run, workflow, event):
    require(isinstance(run, dict), "invalid_run")
    require(run.get("path", "").split("@")[0] == workflow
            and run.get("event") == event and run.get("head_branch") == "main"
            and (run.get("repository") or {}).get("full_name") == REPO
            and (run.get("head_repository") or {}).get("full_name") == REPO
            and SHA.fullmatch(run.get("head_sha") or ""), "run_provenance_mismatch")


def artifact_files(run_id, name):
    listing = gh(f"repos/{REPO}/actions/runs/{run_id}/artifacts?per_page=100")
    require(listing.get("total_count", 101) <= 100, "artifact_listing_truncated")
    matches = [row for row in listing.get("artifacts", []) if row.get("name") == name]
    require(len(matches) == 1, "artifact_missing_or_ambiguous")
    artifact = matches[0]
    require(not artifact.get("expired") and type(artifact.get("id")) is int
            and 0 < artifact.get("size_in_bytes", 0) <= MAX_BYTES, "artifact_invalid")
    archive = gh(f"repos/{REPO}/actions/artifacts/{artifact['id']}/zip", binary=True)
    try:
        with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
            items = bundle.infolist()
            require(len(items) <= 30 and sum(item.file_size for item in items) <= MAX_BYTES,
                    "artifact_expansion_limit")
            # Never extract paths. Only unique basenames of bounded JSON files
            # are decoded, and no raw artifact data is printed or re-uploaded.
            files = {}
            for item in items:
                name = Path(item.filename).name
                if name.endswith(".json"):
                    require(name not in files, "artifact_duplicate_filename")
                    files[name] = json.loads(bundle.read(item), object_pairs_hook=unique_object)
            return files
    except (ValueError, OSError, zipfile.BadZipFile, RuntimeError, RecursionError):
        raise ProofError("artifact_invalid_archive") from None


def validate_evidence(files):
    snapshot = files.get("staging-rollback-snapshot.json") or {}
    restore = files.get("auto-rollback-evidence.json") or {}
    error_gate = files.get("error-rate-gate.json") or {}
    require(snapshot.get("worker") == "two-bot-next-staging" and snapshot.get("has_previous") is True,
            "staging_snapshot_missing")
    require(restore.get("variant") == "staging" and restore.get("worker") == "two-bot-next-staging",
            "restore_not_staging")
    version = snapshot.get("serving_version")
    image = snapshot.get("serving_image") or ""
    digest = image.rsplit("@", 1)[-1]
    require(isinstance(version, str) and VERSION.fullmatch(version) and DIGEST.fullmatch(digest),
            "invalid_snapshot_members")
    require(restore.get("restored_version") == version
            and restore.get("restored_image_digest") == digest, "restored_snapshot_mismatch")
    require(restore.get("takeover") is True and restore.get("gate") == "stable-200-gateway-ready"
            and not restore.get("failure"), "restore_not_stable")
    require(error_gate.get("mode") == "rehearsal" and error_gate.get("verdict") == "fail"
            and error_gate.get("firing") == ["rest_5xx_rate"], "error_gate_not_injected")
    return {"restored_version": version, "restored_image_digest": digest,
            "gate": restore["gate"], "takeover": True}


def prove(*, sleep=time.sleep, clock=time.monotonic):
    response = gh(f"repos/{REPO}/actions/workflows/deploy-staging.yml/dispatches", body=DISPATCH)
    run_id = response.get("workflow_run_id")
    require(type(run_id) is int and run_id > 0, "dispatch_missing_exact_run_id_no_retry")
    print(json.dumps({"child_run_id": run_id, "next_action": "wait_for_exact_staging_run"}))
    deadline = clock() + MAX_WAIT
    while True:
        run = gh(f"repos/{REPO}/actions/runs/{run_id}")
        run_identity(run, WORKFLOW, "workflow_dispatch")
        require(run.get("id") == run_id and run.get("run_attempt") == 1, "run_id_or_attempt_changed")
        require(run.get("display_title") == REHEARSAL_TITLE, "child_not_rehearsal")
        if run.get("status") == "completed":
            break
        require(clock() < deadline, "staging_run_timeout_inspect_child_no_redispatch")
        sleep(30)
    require(run.get("conclusion") == "failure", "child_must_remain_red")
    jobs = gh(f"repos/{REPO}/actions/runs/{run_id}/attempts/1/jobs?per_page=100")
    require(jobs.get("total_count", 101) <= 100, "jobs_listing_truncated")
    deploy = [job for job in jobs.get("jobs", []) if job.get("name") == "wrangler deploy + intended rollout gate"]
    require(len(deploy) == 1, "staging_deploy_job_missing")
    steps = deploy[0].get("steps") or []
    for name, outcome in REQUIRED_STEPS.items():
        found = [step for step in steps if step.get("name") == name]
        require(len(found) == 1 and found[0].get("conclusion") == outcome, "unexpected_staging_step_outcome")
    evidence = validate_evidence(artifact_files(run_id, "staging-rollout-evidence"))
    return dict(evidence, schema_version=1, verdict="pass", child_run_id=run_id,
                child_sha=run["head_sha"], child_run_attempt=1, child_conclusion="failure",
                environment="staging", completed_at=run["updated_at"])


def check_freshness(*, now=None):
    now = now or datetime.now(timezone.utc)
    runs = gh(f"repos/{REPO}/actions/workflows/staging-rollback-drill.yml/runs"
              "?branch=main&event=schedule&per_page=1").get("workflow_runs") or []
    require(len(runs) == 1, "daily_proof_missing")
    run = runs[0]
    run_identity(run, PROOF_WORKFLOW, "schedule")
    # Grade the newest scheduled run, not the newest success: an older good
    # proof must never hide today's failed, cancelled or stuck drill.
    require(run.get("status") == "completed" and run.get("conclusion") == "success",
            "daily_proof_not_green")
    require(timedelta(0) <= now - moment(run.get("created_at")) <= MAX_AGE, "daily_proof_stale")
    evidence = artifact_files(run["id"], ARTIFACT).get("proof.json") or {}
    require(type(evidence.get("schema_version")) is int and evidence["schema_version"] == 1
            and evidence.get("verdict") == "pass" and evidence.get("environment") == "staging"
            and evidence.get("child_conclusion") == "failure"
            and type(evidence.get("child_run_id")) is int and evidence["child_run_id"] > 0,
            "daily_proof_evidence_invalid")
    return {"verdict": "pass", "proof_run_id": run["id"], "child_run_id": evidence["child_run_id"]}


def alert(code):
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    client = importlib.import_module("synthetic_alert")
    content = (f"two-bot-next ALERT staging rollback proof: {code}. "
               "Inspect the daily staging drill and its child run before retrying; never drill production. "
               f"Runbook: https://github.com/{REPO}/blob/main/docs/production-deploy.md#daily-staging-proof "
               f"| logs {client.run_link()}")
    try:
        client.post(os.environ.get("SYNTHETIC_ALERT_WEBHOOK_URL"), content)
    except client.AlertError:
        raise ProofError("alert_delivery_failed") from None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("mode", choices=("run", "watch", "alert"))
    parser.add_argument("--evidence", type=Path)
    parser.add_argument("--dry-run", action="store_true", help="print the fixed contract; no network or mutation")
    args = parser.parse_args(argv)
    if args.dry_run:
        print(json.dumps({"environment": "staging", "dispatch": DISPATCH, "mode": args.mode}))
        return 0
    try:
        require(os.environ.get("GITHUB_REPOSITORY") == REPO
                and os.environ.get("GITHUB_REF") == "refs/heads/main", "repository_or_ref_refused")
        if args.mode == "alert":
            alert("daily_proof_failed")
            return 0
        if args.mode == "run":
            require(args.evidence is not None, "evidence_path_required_before_dispatch")
            evidence = prove()
            args.evidence.write_text(json.dumps(evidence, sort_keys=True, indent=2) + "\n", encoding="utf-8")
        else:
            try:
                evidence = check_freshness()
            except (ProofError, ValueError, TypeError, KeyError, AttributeError) as error:
                code = str(error) if isinstance(error, ProofError) else "daily_proof_unreadable"
                alert(code)
                raise ProofError(code) from None
        print(json.dumps(evidence, sort_keys=True))
        return 0
    except (ProofError, ValueError, TypeError, KeyError, AttributeError, OSError) as error:
        code = str(error) if isinstance(error, ProofError) else "proof_or_alert_failure"
        print(json.dumps({"verdict": "fail", "code": code, "retryable": False,
                          "next_action": "inspect_exact_child_run_and_staging_state_before_retry"}), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
