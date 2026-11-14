"""Inactive helpers for collecting complete PR file inventories.

Adapted from base_attestation_gate.py at commit 3935a2c82e780fad7e923f427c43299b583a29be.
This module does not enforce an attestation policy or schedule a workflow.
"""

STATUSES = {"added", "removed", "modified", "renamed", "copied", "changed"}
MAX_FILES = 3000


class InventoryError(ValueError):
    """Reject an incomplete or unrepresentable inventory without echoing input."""


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
    return sorted(paths)
