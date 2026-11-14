"""Inactive helpers for collecting complete PR file inventories.

Adapted from base_attestation_gate.py at commit 3935a2c82e780fad7e923f427c43299b583a29be.
This module does not enforce an attestation policy or schedule a workflow.
"""

import re
from urllib.parse import quote


STATUSES = {"added", "removed", "modified", "renamed", "copied", "changed"}
MAX_FILES = 3000


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


def require_fresh(api, pr):
    """Compare one PR snapshot and its base ref; this is not an evaluation fence.

    The caller must bind event/run identities, bracket evaluation with comparisons,
    and establish the reader's trust, consistency and freshness guarantees.
    """
    expected = snapshot(pr)
    repository = pr["base"]["repo"]["full_name"]
    prefix = f"repos/{repository}/pulls/{pr['number']}"
    base_route = f"repos/{repository}/git/ref/heads/{quote(pr['base']['ref'], safe='')}"
    base_sha = pr["base"]["sha"]
    try:
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
