#!/usr/bin/env python3
"""Refuse a production promotion that has no valid rollback target.

Runs inside the deploy-production `production` job after the pre-deploy
snapshot and before anything is mutated. A promotion is only as safe as the
rollback behind it, so this preflight proves the rollback target exists and
still fits the candidate's schema before the deploy:

1. The snapshot names a complete provenance receipt: a previous serving
   Worker version, the Git SHA it was deployed from, and its container image
   digest. A first deploy, an unstamped version or a split-traffic state has
   no receipt and refuses.
2. Live Cloudflare state still binds that receipt, with reads only: the
   snapshotted rollout still names the image, the version was deployed from
   the SHA and served 100% before, and the image came from a converged
   rollout (`worker_container_rollback.validate_receipt_against_live`, the
   same check a rollback runs before any restore POST).
3. The image digest still resolves in the registry (`docker manifest
   inspect` with a 15-minute pull-only credential), so a restore cannot fail
   on a garbage-collected image.
4. No migration added between the serving SHA and the candidate is
   backward-incompatible: the serving build must run against the candidate's
   schema after a rollback (a Worker rollback never undoes schema). A
   statement that drops or renames an object, changes a column type, adds a
   NOT NULL constraint (or a NOT NULL column without a default), drops a
   default, truncates or revokes refuses, unless the file carries a reviewed
   `-- rollback-compatible: <reason>` line. A rewritten or deleted migration
   always refuses. The serving SHA must be an ancestor of the candidate.

Evidence carries allowlisted IDs, file names and fixed-vocabulary codes only:
never SQL text, API bodies or credentials. Exit 0 passes, 1 refuses.

Usage (from the `production` job, working directory `wrangler`):
  python3 ../tooling/scripts/rollback_target_preflight.py \\
      --snapshot "$RUNNER_TEMP/production-rollback-snapshot.json" \\
      --sha "$SHA" --repo .. --out "$RUNNER_TEMP/production-rollback-preflight.json"
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
# Same E402-safe convention as the other scripts/ tools.
import staging_rollout as base
import worker_container_rollback as wcr

GateError = base.GateError
require = base.require

VARIANT = "production"
WORKER = wcr.WORKERS[VARIANT]
APPLICATION = wcr.APPLICATIONS[VARIANT]
REGISTRY_HOST = "registry.cloudflare.com"
SNAPSHOT_MAX_BYTES = 65536
GIT_TIMEOUT_SECONDS = 30
COMMAND_TIMEOUT_SECONDS = 120
COMMAND_OUTPUT_CAP = 1024 * 1024
MIGRATION = re.compile(r"crates/[a-z0-9_-]+/migrations/[0-9]{4}_[a-z][a-z0-9_]*\.sql")
# A reviewed exception: the author states why the serving build still runs
# against this schema. At least a short sentence, never an empty marker.
OVERRIDE = re.compile(r"^--\s*rollback-compatible:\s*(\S.{9,})$", re.MULTILINE)
# Statements the previous build cannot survive after a rollback. Matched per
# statement after comments are stripped; the code names the first match.
INCOMPATIBLE = (
    ("drop_object", re.compile(
        r"\bDROP\s+(TABLE|COLUMN|TYPE|VIEW|MATERIALIZED\s+VIEW|SCHEMA|FUNCTION|SEQUENCE)\b", re.I)),
    ("rename", re.compile(r"\bRENAME\b", re.I)),
    ("column_type_change", re.compile(r"\bALTER\s+COLUMN\s+\S+\s+(SET\s+DATA\s+)?TYPE\b", re.I)),
    ("set_not_null", re.compile(r"\bSET\s+NOT\s+NULL\b", re.I)),
    ("drop_default", re.compile(r"\bDROP\s+DEFAULT\b", re.I)),
    ("truncate", re.compile(r"\bTRUNCATE\b", re.I)),
    ("revoke", re.compile(r"\bREVOKE\b", re.I)),
)
ADD_COLUMN = re.compile(r"\bADD\s+COLUMN\b", re.I)
NOT_NULL = re.compile(r"\bNOT\s+NULL\b", re.I)
DEFAULT = re.compile(r"\bDEFAULT\b", re.I)


def statements(sql):
    """SQL statements with comments and quoted literals blanked out."""
    sql = re.sub(r"/\*.*?\*/", " ", sql, flags=re.S)
    sql = re.sub(r"--[^\n]*", " ", sql)
    sql = re.sub(r"'(?:[^']|'')*'", "''", sql)
    return [part.strip() for part in sql.split(";") if part.strip()]


def incompatible_code(sql):
    """The first backward-incompatible statement's code, or None."""
    for statement in statements(sql):
        for code, pattern in INCOMPATIBLE:
            if pattern.search(statement):
                return code
        if ADD_COLUMN.search(statement) and NOT_NULL.search(statement) \
                and not DEFAULT.search(statement):
            return "add_not_null_column_without_default"
    return None


def git(repo, *args):
    try:
        return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True,
                              timeout=GIT_TIMEOUT_SECONDS).stdout
    except (OSError, subprocess.SubprocessError):
        raise GateError("git_read_failed") from None


def migration_findings(repo, serving_sha, sha, git_fn=git):
    """(checked, refusals) for migrations between the serving SHA and the candidate."""
    try:
        git_fn(repo, "merge-base", "--is-ancestor", serving_sha, sha)
    except GateError:
        raise GateError("rollback_target_not_ancestor") from None
    raw = git_fn(repo, "diff", "--name-status", "--no-renames", "-z", serving_sha, sha, "--", "crates")
    fields = raw.decode("utf-8", "replace").split("\0")
    checked, refusals = [], []
    for status, path in zip(fields[0::2], fields[1::2]):
        if not MIGRATION.fullmatch(path):
            continue
        name = path.rsplit("/", 1)[-1]
        if status != "A":
            # The checksum lock already forbids this; a rollback across a
            # rewritten history is never proven compatible.
            refusals.append({"migration": name, "code": "migration_rewritten"})
            continue
        sql = git_fn(repo, "show", f"{sha}:{path}").decode("utf-8", "replace")
        checked.append(name)
        code = incompatible_code(sql)
        if code is not None and not OVERRIDE.search(sql):
            refusals.append({"migration": name, "code": code})
    return checked, refusals


def load_receipt(path):
    """The snapshot's complete provenance receipt, or a refusal."""
    try:
        raw = Path(path).read_bytes()
    except OSError:
        raise GateError("snapshot_unreadable") from None
    require(len(raw) <= SNAPSHOT_MAX_BYTES, "snapshot_too_large")
    try:
        snapshot = json.loads(raw.decode("utf-8"), object_pairs_hook=wcr._reject_duplicate_keys)
    except (ValueError, UnicodeDecodeError):
        raise GateError("snapshot_unreadable") from None
    require(isinstance(snapshot, dict), "snapshot_unreadable")
    require(snapshot.get("has_previous") is True, "no_rollback_target")
    require(snapshot.get("provenance_complete") is True, "rollback_target_unstamped")
    require(snapshot.get("worker") == WORKER and snapshot.get("application") == APPLICATION,
            "snapshot_identity_mismatch")
    try:
        receipt = wcr.ProvenanceReceipt(WORKER, APPLICATION, snapshot.get("serving_version"),
                                        snapshot.get("serving_sha"), snapshot.get("serving_image"))
    except GateError:
        raise GateError("rollback_target_incomplete") from None
    return snapshot, receipt


def validate_live(client, snapshot, receipt):
    """Bind the receipt to Cloudflare with reads only (the rollback's own check)."""
    rollout = base.identifier(snapshot.get("serving_rollout_id"))
    row = base.mapping(client.api(
        f"/containers/applications/{base.identifier(snapshot.get('application_id'))}"
        f"/rollouts/{rollout}"))
    require(row.get("id") == rollout, "rollout_identity_drift")
    require(wcr.rollout_image(row, APPLICATION) == receipt.image, "provenance_mismatch", "member=image")
    wcr.validate_receipt_against_live(client, receipt)


def command(argv, env=None, input_bytes=None):
    try:
        result = subprocess.run(argv, check=True, capture_output=True, env=env, input=input_bytes,
                                timeout=COMMAND_TIMEOUT_SECONDS)
    except (OSError, subprocess.SubprocessError):
        raise GateError("registry_command_failed") from None
    require(len(result.stdout) <= COMMAND_OUTPUT_CAP, "registry_output_too_large")
    return result.stdout


def image_in_registry(image, account, run=command):
    """Raise unless the registry still serves the image's manifest by digest."""
    require(image.split("/", 2)[1] == account, "rollback_image_wrong_account")
    try:
        creds = json.loads(run(
            ["npx", "--no-install", "wrangler", "containers", "registries", "credentials",
             REGISTRY_HOST, "--pull", "--expiration-minutes", "15", "--json"]).decode("utf-8"),
            object_pairs_hook=wcr._reject_duplicate_keys)
    except (ValueError, UnicodeDecodeError):
        raise GateError("registry_authentication_refused") from None
    require(isinstance(creds, dict) and creds.get("account_id") == account
            and creds.get("registry_host") == REGISTRY_HOST
            and isinstance(creds.get("username"), str)
            and re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", creds["username"]) is not None
            and isinstance(creds.get("password"), str) and 0 < len(creds["password"]) <= 4096,
            "registry_authentication_refused")
    with tempfile.TemporaryDirectory(prefix="rollback-preflight-docker-") as docker_config:
        env = dict(os.environ, DOCKER_CONFIG=docker_config)
        run(["docker", "login", "--password-stdin", "--username", creds["username"], REGISTRY_HOST],
            env=env, input_bytes=creds["password"].encode())
        # A by-digest fetch is content-addressed: the client verifies the
        # bytes against the digest, so a parsed answer proves the image is
        # still stored. A missing manifest exits non-zero.
        try:
            manifest = json.loads(run(["docker", "manifest", "inspect", image], env=env).decode("utf-8"))
        except (GateError, ValueError, UnicodeDecodeError):
            raise GateError("rollback_image_missing") from None
    require(isinstance(manifest, dict) and manifest.get("schemaVersion") == 2, "rollback_image_missing")


def preflight(args, *, client, git_fn=git, registry=image_in_registry, account=""):
    snapshot, receipt = load_receipt(args.snapshot)
    require(wcr.SHA.fullmatch(args.sha or ""), "invalid_candidate_sha")
    validate_live(client, snapshot, receipt)
    registry(receipt.image, account)
    checked, refusals = migration_findings(args.repo, receipt.sha, args.sha, git_fn)
    evidence = {
        "candidate_sha": args.sha,
        "rollback_version": receipt.version,
        "rollback_sha": receipt.sha,
        "rollback_image": receipt.image,
        "migrations_checked": checked,
        "migration_refusals": refusals,
        "verdict": "fail" if refusals else "pass",
        "code": "backward_incompatible_migration" if refusals else "rollback_target_valid",
    }
    return evidence


def main(argv=None, *, environ=None, client_factory=wcr.RollbackClient, git_fn=git,
         registry=image_in_registry):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--snapshot", required=True)
    parser.add_argument("--sha", required=True, help="candidate SHA being promoted")
    parser.add_argument("--repo", default=".", help="checkout holding both SHAs")
    parser.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    env = os.environ if environ is None else environ
    try:
        account = env.get("CLOUDFLARE_ACCOUNT_ID", "")
        client = client_factory(account, env.get("CLOUDFLARE_API_TOKEN"))
        evidence = preflight(args, client=client, git_fn=git_fn, registry=registry, account=account)
    except GateError as error:
        evidence = {"candidate_sha": args.sha if wcr.SHA.fullmatch(args.sha or "") else None,
                    "verdict": "fail", "code": str(error)}
    Path(args.out).write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    print("rollback target preflight: " + json.dumps(evidence, sort_keys=True))
    summary = env.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            if evidence["verdict"] == "pass":
                handle.write(f"- Rollback target: Worker `{evidence['rollback_version']}`, "
                             f"image `{evidence['rollback_image']}`, sha `{evidence['rollback_sha']}` "
                             f"(live, in the registry; {len(evidence['migrations_checked'])} "
                             "migration(s) since it, none backward-incompatible)\n")
            else:
                handle.write(f"- Rollback target preflight refused: `{evidence['code']}`\n")
    if evidence["verdict"] != "pass":
        print(f"::error::Refusing promotion: no valid rollback target ({evidence['code']})")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
