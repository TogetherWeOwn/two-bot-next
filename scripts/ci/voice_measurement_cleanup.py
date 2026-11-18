"""Validate a bounded staging measurement cleanup receipt."""

import json
import os
import stat

from voice_lifecycle_evidence import unique_object


MAX_RECEIPT_BYTES = 65536


def read_cleanup_receipt(path):
    """Read a small, regular JSONL receipt without following a symlink."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as source:
            if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                return None
            data = source.read(MAX_RECEIPT_BYTES + 1)
        if len(data) > MAX_RECEIPT_BYTES:
            return None
        lines = data.decode("utf-8").splitlines()
        if len(lines) != 3 or any(not line.strip() for line in lines):
            return None
        return [json.loads(line, object_pairs_hook=unique_object,
                           parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
                for line in lines]
    except (OSError, TypeError, ValueError, UnicodeError, RecursionError):
        return None


def cleanup_verified(records, run_id, attempt, expires_at, sha, deployment_id):
    """Accept only the successful release for this exact staging window."""
    if (not isinstance(records, list) or len(records) != 3
            or not isinstance(run_id, str) or not run_id.isascii()
            or not run_id.isdigit() or run_id.startswith("0")
            or type(attempt) is not int or attempt <= 0
            or type(expires_at) is not int or expires_at <= 0
            or not isinstance(sha, str) or len(sha) != 40
            or any(c not in "0123456789abcdef" for c in sha)
            or not isinstance(deployment_id, str) or not deployment_id):
        return False
    if not all(isinstance(record, dict) for record in records):
        return False
    start, status, release = records
    if (start.get("schema") != "two-bot-staging-pipeline.v1"
            or start.get("action") != "attempt" or start.get("operation") != "finish"
            or status.get("schema") != "two-bot-staging-use.v1" or status.get("action") != "status"
            or release.get("schema") != "two-bot-staging-use.v1" or release.get("action") != "release"):
        return False
    if (any(record.get("environment") != "staging" for record in records)
            or start.get("runId") != run_id or type(start.get("runAttempt")) is not int
            or start["runAttempt"] != attempt):
        return False
    use = release.get("use")
    prior = status.get("use")
    if (not isinstance(use, dict) or not isinstance(prior, dict)
            or status.get("sha") != sha or status.get("deploymentId") != deployment_id
            or type(status.get("epoch")) is not int or status["epoch"] != release.get("epoch")
            or not 0 < status["epoch"] <= 9007199254740991
            or type(status.get("running")) is not bool
            or prior.get("phase") not in ("preparing", "active", "released")
            or any(prior.get(key) != use.get(key) or type(prior.get(key)) is not type(use.get(key))
                   for key in ("environment", "runId", "runAttempt", "expiresAt", "acquiredAt",
                               "sha", "deploymentId", "epoch"))
            or type(use.get("acquiredAt")) is not int or not 0 <= use["acquiredAt"] < expires_at
            or expires_at - use["acquiredAt"] > 6 * 60 * 60 * 1000
            or len(run_id) > 20 or attempt > 9007199254740991 or expires_at > 9007199254740991
            or not 1 <= len(deployment_id) <= 128
            or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-" for c in deployment_id)):
        return False
    return (release.get("running") is False and release.get("sha") == sha
            and release.get("deploymentId") == deployment_id and isinstance(use, dict)
            and use.get("environment") == "staging" and use.get("runId") == run_id
            and type(use.get("runAttempt")) is int and use["runAttempt"] == attempt
            and type(use.get("expiresAt")) is int and use["expiresAt"] == expires_at
            and use.get("sha") == sha and use.get("deploymentId") == deployment_id
            and type(release.get("epoch")) is int and release["epoch"] > 0
            and type(use.get("epoch")) is int and use["epoch"] == release["epoch"]
            and use.get("phase") == "released")
