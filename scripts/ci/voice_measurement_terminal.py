"""Fail-closed classification of the disabled voice measurement job graph."""

import json
import os
from pathlib import Path
import stat
import subprocess
import sys
from datetime import datetime, timezone

from voice_lifecycle_evidence import unique_object, validate as validate_lifecycle
from voice_measurement_cleanup import cleanup_verified, read_cleanup_receipt

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from script_intake import decode_legacy_json, read_with_overflow
sys.path.pop(0)


JOBS = (
    "staging-window", "measurement-before", "voice-first", "measurement-after",
    "voice-second", "measurement-final", "staging-use-release",
)


def lifecycles_verified(first, second, run_id, second_required):
    """Revalidate raw invocation evidence rather than trusting candidate summaries."""
    if (not isinstance(run_id, str) or not run_id.isascii() or not run_id.isdigit()
            or run_id.startswith("0") or second_required not in ("true", "false")):
        return False
    if not isinstance(first, (str, os.PathLike)):
        return False
    if validate_lifecycle(first, run_id)[0] != "single_successful_lifecycle":
        return False
    if second_required == "true":
        return (isinstance(second, (str, os.PathLike))
                and validate_lifecycle(second, run_id)[0] == "single_successful_lifecycle")
    return second is None


def cleanup_started_at(records, acquired_at):
    """Validate the observation sequence; finish must start after the last capture."""
    try:
        times = []
        for record in records:
            stamp = record["observedAt"]
            parsed = datetime.fromisoformat(stamp)
            if parsed.tzinfo is not timezone.utc or stamp != parsed.isoformat(timespec="milliseconds")[:-6] + "Z":
                return None
            times.append(round(parsed.timestamp() * 1000))
    except (KeyError, TypeError, ValueError, OverflowError):
        return None
    start, status, release = times
    return start if acquired_at <= start <= status <= release else None


def candidate_verified(second_required, grant, candidate, receipts):
    """Recompute silently using the validated grant and timing snapshot."""
    if (second_required not in ("true", "false") or not isinstance(receipts, list)
            or len(receipts) != (3 if second_required == "true" else 2)
            or any(not isinstance(path, (str, os.PathLike)) for path in [candidate, *receipts])):
        return False
    try:
        result = subprocess.run(
            ["node", "wrangler/scripts/voice-measurement-candidate.mjs",
             second_required, "-", candidate, *receipts],
            input=json.dumps(grant), text=True,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15, check=False,
        )
        return result.returncode == 0
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return False


def read_matching_grant(path, run_id, attempt, expires_at, sha, deployment_id):
    """Return bounded regular-file metadata only if it agrees with cleanup."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as source:
            if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                return None
            data = read_with_overflow(source, 4096)
        if len(data) > 4096:
            return None
        metadata = decode_legacy_json(data.decode("utf-8"), object_pairs_hook=unique_object)
        expiry = datetime.fromtimestamp(expires_at / 1000, timezone.utc).isoformat(timespec="milliseconds")
        expected = {"runId": run_id, "runAttempt": str(attempt), "sha": sha,
                    "deploymentId": deployment_id, "expiresAt": expiry.replace("+00:00", "Z")}
        return expected if metadata == expected else None
    except (OSError, TypeError, ValueError, OverflowError, RecursionError):
        return None


def terminal_verdict(results, second_required, cleanup_verified, candidate_verified):
    """Return a fixed verdict without copying untrusted input into evidence."""
    if not isinstance(results, dict) or set(results) != set(JOBS):
        return "refused"
    if second_required not in ("true", "false"):
        return "refused"
    if cleanup_verified is not True or candidate_verified is not True:
        return "refused"
    expected = {job: "success" for job in JOBS}
    if second_required == "false":
        expected["voice-second"] = "skipped"
        expected["measurement-final"] = "skipped"
    return "accepted" if results == expected else "refused"


def evaluate_terminal(results, second_required, run_id, attempt, expires_at, sha, deployment_id,
                      cleanup_path, first_lifecycle, second_lifecycle, grant, candidate, receipts):
    """Accept only when the job graph and all original evidence independently agree."""
    if terminal_verdict(results, second_required, True, True) != "accepted":
        return "refused"
    cleanup = read_cleanup_receipt(cleanup_path)
    if not cleanup_verified(cleanup, run_id, attempt, expires_at, sha, deployment_id):
        return "refused"
    acquired_at = cleanup[2]["use"]["acquiredAt"]
    started_at = cleanup_started_at(cleanup, acquired_at)
    if started_at is None or not lifecycles_verified(first_lifecycle, second_lifecycle, run_id, second_required):
        return "refused"
    metadata = read_matching_grant(grant, run_id, attempt, expires_at, sha, deployment_id)
    if metadata is None or not candidate_verified(
            second_required, dict(metadata, acquiredAt=acquired_at, cleanupStartedAt=started_at), candidate, receipts):
        return "refused"
    return "accepted"


def main():
    """Write a fixed terminal verdict; absent or invalid inputs always refuse."""
    if len(sys.argv) != 2:
        return 1
    verdict = "refused"
    try:
        env = os.environ
        second = env.get("SECOND_REQUIRED", "")
        root = env["EVIDENCE_DIR"]
        results = {job: env.get("RESULT_" + job.upper().replace("-", "_"), "") for job in JOBS}
        candidate_dir = os.path.join(root, "candidate")
        receipts = [os.path.join(candidate_dir, "voice-before", "voice-before.json"),
                    os.path.join(candidate_dir, "voice-middle", "voice-after.json") if second == "true"
                    else os.path.join(candidate_dir, "voice-after.json")]
        if second == "true":
            receipts.append(os.path.join(candidate_dir, "voice-final.json"))
        verdict = evaluate_terminal(
            results, second, env["GITHUB_RUN_ID"], int(env["GITHUB_RUN_ATTEMPT"]),
            int(env["STAGING_USE_EXPIRES_AT"]), env["STAGING_USE_SHA"], env["STAGING_USE_DEPLOYMENT"],
            os.path.join(root, "cleanup", "staging-use-measurement-stop.jsonl"),
            os.path.join(root, "first"), os.path.join(root, "second") if second == "true" else None,
            os.path.join(root, "grant.json"),
            os.path.join(candidate_dir, "voice-combined.json" if second == "true" else "voice-decision.json"),
            receipts,
        )
    except (KeyError, ValueError, TypeError, OSError, RecursionError):
        pass
    try:
        fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            json.dump({"schema": "voice-measurement-terminal.v1", "verdict": verdict}, output)
            output.write("\n")
    except OSError:
        return 1
    return 0 if verdict == "accepted" else 1


if __name__ == "__main__":
    raise SystemExit(main())
