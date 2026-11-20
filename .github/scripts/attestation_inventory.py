"""Inactive helpers for collecting complete PR file inventories.

Adapted from base_attestation_gate.py at commit 3935a2c82e780fad7e923f427c43299b583a29be.
This module does not enforce an attestation policy or schedule a workflow.
"""

import copy
import re
from urllib.parse import quote


STATUSES = {"added", "removed", "modified", "renamed", "copied", "changed"}
MAX_FILES = 3000
SELF_GATE_PATHS = frozenset({
    ".github/workflows/base-attestation.yml",
    ".github/scripts/base_attestation_gate.py",
    "scripts/ci/test_base_attestation_gate.py",
    ".github/scripts/attestation_inventory.py",
    ".github/scripts/attestation_transport.py",
    "scripts/ci/test_attestation_transport.py",
    "scripts/ci/test_attestation_inventory.py",
    "scripts/ci/test_attestation_snapshot.py",
    "scripts/ci/test_attestation_event_identity.py",
    "scripts/ci/test_attestation_policy.py",
    "scripts/ci/test_attestation_orchestration.py",
})


class InventoryError(ValueError):
    """Reject an incomplete or unrepresentable inventory without echoing input."""


def snapshot(pr):
    """Capture identity fields; callers must independently establish freshness."""
    def text(value):
        return isinstance(value, str) and bool(value)

    def positive_int(value):
        return type(value) is int and value > 0

    if not (isinstance(pr, dict) and pr.get("state") == "open"
            and positive_int(pr.get("number"))
            and type(pr.get("changed_files")) is int and 0 < pr["changed_files"] <= MAX_FILES
            and text(pr.get("title")) and "body" in pr
            and isinstance(pr["body"], (str, type(None))) and text(pr.get("updated_at"))
            and isinstance(pr.get("user"), dict) and text(pr["user"].get("login"))):
        raise InventoryError("Invalid pull request metadata")
    identity = [pr["number"], pr["state"], pr["title"], pr["body"],
                pr["user"]["login"], pr["updated_at"], pr["changed_files"]]
    for side in ("base", "head"):
        value = pr.get(side)
        if not (isinstance(value, dict) and text(value.get("ref"))
                and isinstance(value.get("sha"), str)
                and re.fullmatch(r"[0-9a-f]{40}", value["sha"])):
            raise InventoryError("Invalid head or base identity")
        repo = value.get("repo")
        if not (isinstance(repo, dict) and positive_int(repo.get("id"))
                and isinstance(repo.get("full_name"), str)
                and re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo["full_name"])):
            raise InventoryError("Invalid repository identity")
        identity.extend((value["ref"], value["sha"], (repo["id"], repo["full_name"])))
    return tuple(identity)


def validate_event_identity(event, repository, run_sha):
    """Bind a synthetic event to a trusted repository name and base-run SHA.

    Return the immutable snapshot tuple on success; raise InventoryError with a
    constant message otherwise. No API reads or policy evaluation are performed.
    The caller must supply trusted run context and independently fence evaluation.
    A fork head is allowed; it is not used as the base repository identity.
    """
    if not (isinstance(event, dict) and isinstance(repository, str)
            and re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository)
            and isinstance(run_sha, str) and re.fullmatch(r"[0-9a-f]{40}", run_sha)):
        raise InventoryError("Invalid event identity")
    repo = event.get("repository")
    if not (isinstance(repo, dict) and type(repo.get("id")) is int and repo["id"] > 0
            and repo.get("full_name") == repository):
        raise InventoryError("Invalid event identity")
    pr = event.get("pull_request")
    identity = snapshot(pr)
    if not (type(event.get("number")) is int and event["number"] == pr["number"]
            and pr["base"]["ref"] == "main"
            and pr["base"]["sha"] == run_sha
            and (repo["id"], repository) == (pr["base"]["repo"]["id"],
                                               pr["base"]["repo"]["full_name"])):
        raise InventoryError("Invalid event identity")
    return identity


def evaluate_metadata(event, repository, run_sha, paths, policy):
    """Evaluate synthetic metadata with the caller's trusted base policy module.

    paths must be the complete expanded result of changed_paths for this snapshot.
    The caller must establish policy provenance and bracket inventory/evaluation
    with trusted live-read fences; this function is not enforcement.
    Returns only identity and counts; every refusal has the same safe message.
    """
    try:
        validate_event_identity(event, repository, run_sha)
        pr = event["pull_request"]
        if not (type(paths) is list and pr["changed_files"] <= len(paths)
                <= 2 * pr["changed_files"]):
            raise ValueError
        validated = [inventory_path(path) for path in paths]
        if len(set(validated)) != len(validated):
            raise ValueError
        body = pr["body"] or ""
        if body.strip().startswith("This release is too large to preview in the pull request body."):
            raise ValueError
        findings = list(policy.evaluate({
            "EVENT": "pull_request", "TITLE": pr["title"], "BODY": body,
            "AUTHOR": pr["user"]["login"], "HEAD_REF": pr["head"]["ref"],
            "CHANGED_FILES": "\n".join(validated), "REPO_PRIVATE": "false",
            "REQUIRE_CARD_REF": "false", "PR_STANDARDS_MODE": "warn",
        }))
        if SELF_GATE_PATHS.intersection(validated):
            findings.extend(policy.attestation_findings(body, [".github/workflows/supply-chain.yml"]))
        if any(finding.level != "warning" for finding in findings):
            raise ValueError
        return {"head_sha": pr["head"]["sha"], "base_sha": run_sha,
                "file_count": pr["changed_files"], "path_count": len(validated)}
    except Exception:
        raise InventoryError("Metadata policy evaluation refused") from None


def evaluate_bracketed(event, repository, run_sha, api, policy):
    """Compose inactive comparisons, inventory and policy; not a live fence.

    The caller must establish reader and policy provenance, provider consistency,
    atomicity and snapshot semantics before using this for any enforcement.
    """
    try:
        baseline = copy.deepcopy(event)
        validate_event_identity(baseline, repository, run_sha)
        pr = baseline["pull_request"]
        require_fresh(api, pr)
        prefix = f"repos/{repository}/pulls/{pr['number']}"
        paths = changed_paths(api, prefix, pr["changed_files"])
        result = evaluate_metadata(copy.deepcopy(baseline), repository, run_sha, paths, policy)
        require_fresh(api, pr)
        return result
    except Exception:
        raise InventoryError("Attestation evaluation refused") from None


def require_fresh(api, pr):
    """Compare one PR snapshot and its base ref; this is not an evaluation fence.

    The caller must bind event/run identities, bracket evaluation with comparisons,
    and establish the reader's trust, consistency and freshness guarantees.
    """
    expected = snapshot(pr)
    repository = pr["base"]["repo"]["full_name"]
    prefix = f"repos/{repository}/pulls/{pr['number']}"
    base_sha = pr["base"]["sha"]
    try:
        base_route = f"repos/{repository}/git/ref/heads/{quote(pr['base']['ref'], safe='')}"
        current = api(prefix)
        if snapshot(current) != expected:
            raise InventoryError("Stale pull request metadata")
        ref = api(base_route)
        if not (isinstance(ref, dict) and isinstance(ref.get("object"), dict)
                and ref["object"].get("sha") == base_sha):
            raise InventoryError("Stale or invalid base reference")
    except Exception:
        # Reader exceptions and remote content may contain credentials or PR text.
        raise InventoryError("Freshness comparison failed") from None


def inventory_path(value):
    if not (isinstance(value, str) and value and value == value.strip()
            and value.splitlines() == [value]
            and not any(ord(c) < 32 or ord(c) == 127 for c in value)
            and "\\" not in value
            and all(part not in ("", ".", "..") for part in value.split("/"))):
        raise InventoryError("Unrepresentable repository-relative path")
    return value


def changed_paths(api, prefix, expected):
    """Return sorted current and supplied previous paths from bounded REST pages."""
    if type(expected) is not int or not 0 < expected <= MAX_FILES:
        raise InventoryError("Invalid file inventory count")
    paths, filenames = set(), set()
    seen = 0
    for page in range(1, (expected + 99) // 100 + 1):
        batch = api(f"{prefix}/files?per_page=100&page={page}")
        if not isinstance(batch, list) or len(batch) != min(100, expected - seen):
            raise InventoryError("Incomplete or inconsistent file inventory")
        for row in batch:
            if (not isinstance(row, dict) or not isinstance(row.get("status"), str)
                    or row["status"] not in STATUSES):
                raise InventoryError("Malformed file inventory entry")
            filename = inventory_path(row.get("filename"))
            if filename in filenames:
                raise InventoryError("Duplicate file inventory entry")
            filenames.add(filename)
            paths.add(filename)
            old = row.get("previous_filename")
            if row["status"] == "renamed" and old is None:
                raise InventoryError("Rename inventory is missing its source path")
            if old is not None:
                old = inventory_path(old)
                if row["status"] == "renamed" and old == filename:
                    raise InventoryError("Rename inventory repeats its destination")
                paths.add(old)
        seen += len(batch)
    if expected < MAX_FILES and expected % 100 == 0:
        successor = api(f"{prefix}/files?per_page=100&page={expected // 100 + 1}")
        if not isinstance(successor, list) or successor:
            raise InventoryError("Incomplete or inconsistent file inventory")
    return sorted(paths)
