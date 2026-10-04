#!/usr/bin/env python3
"""Publish a non-secret apply request before its Environment can be approved.

The protection-rule consumer must still authenticate both runs/artifacts and
require independent CEO GO. This publisher is request transport, not approval.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sys

REPOSITORY = "TogetherWeOwn/two-bot-next"
WORKFLOW_PATH = ".github/workflows/staging-migrate.yml"
CLAIM_KIND = "staging-migrate-apply-claim"
MAX_MANIFEST_BYTES = 1024 * 1024
DECIMAL = re.compile(r"[1-9][0-9]{0,19}\Z")
SHA40 = re.compile(r"[0-9a-f]{40}\Z")
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
CHECKSUM = re.compile(r"[0-9a-f]{96}\Z")
I64_MAX = 2**63 - 1


class Refused(ValueError):
    """A bounded, non-secret refusal; never include rejected input in errors."""


def require(condition, message):
    if not condition:
        raise Refused(message)


def versions(values):
    require(isinstance(values, list), "pending versions are invalid")
    require(all(type(v) is int and 0 < v <= I64_MAX for v in values),
            "pending version is outside the positive i64 range")
    require(values == sorted(set(values)), "pending versions are not strictly ascending")
    return values


def projection_hash(manifest):
    """Match crates/cutover/src/staging_migrate.rs::manifest_hash byte-for-byte.

    source SHA + LF; each pending i64 decimal + LF; ALL up migrations in source
    order as version:description:sha384 + LF. Not JSON bytes or a ZIP digest.
    Target/role/approval references need separate binding checks below.
    """
    source = manifest.get("source_sha")
    require(isinstance(source, str) and SHA40.fullmatch(source), "source SHA is invalid")
    pending = versions(manifest.get("pending_before"))
    migrations = manifest.get("source_migrations")
    require(isinstance(migrations, list), "source migrations are invalid")
    migration_versions = []
    rows = [source + "\n"] + [f"{v}\n" for v in pending]
    for migration in migrations:
        require(isinstance(migration, dict), "source migration is invalid")
        version, checksum = migration.get("version"), migration.get("sha384")
        description = migration.get("description")
        require(type(version) is int and 0 < version <= I64_MAX,
                "source migration version is invalid")
        require(isinstance(checksum, str) and CHECKSUM.fullmatch(checksum),
                "source migration checksum is invalid")
        require(isinstance(description, str) and "\n" not in description and "\r" not in description,
                "source migration description is invalid")
        migration_versions.append(version)
        rows.append(f"{version}:{description}:{checksum}\n")
    require(migration_versions == sorted(set(migration_versions)),
            "source migration versions are not strictly ascending")
    require(set(pending).issubset(migration_versions), "pending versions are not present in the source")
    return hashlib.sha256("".join(rows).encode()).hexdigest()


def build_claim(manifest, env):
    require(isinstance(manifest, dict), "plan manifest is invalid")
    require(env.get("GITHUB_REPOSITORY") == REPOSITORY, "repository is outside scope")
    require(env.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
            and env.get("GITHUB_REF") == "refs/heads/main", "dispatch must be on main")
    head = env.get("GITHUB_SHA", "")
    require(SHA40.fullmatch(head), "workflow head SHA is invalid")
    require(env.get("GITHUB_WORKFLOW_REF") == f"{REPOSITORY}/{WORKFLOW_PATH}@refs/heads/main",
            "workflow identity is invalid")
    require(env.get("MODE") == "apply", "claim is apply-only")
    for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "PLAN_RUN_ID"):
        require(DECIMAL.fullmatch(env.get(key, "")), "run identity is invalid")
    require(env["PLAN_RUN_ID"] != env["GITHUB_RUN_ID"], "plan producer must be a different run")
    require(manifest.get("mode") == "plan", "manifest must be read-only plan mode")
    require(type(manifest.get("runner_version")) is int and manifest["runner_version"] == 1,
            "manifest runner version is unsupported")
    require(manifest.get("role") == "two_bot_migrator_ro", "manifest is not a read-only plan")
    require(type(manifest.get("applied_count")) is int and manifest["applied_count"] == 0
            and manifest.get("plan_provenance_verified") is False
            and manifest.get("plan_run_id") == ""
            and isinstance(manifest.get("ledger_before"), list)
            and manifest["ledger_before"] == manifest.get("ledger_after"),
            "manifest is not an unchanged read-only plan")
    digest = env.get("PLAN_MANIFEST_SHA256", "")
    require(SHA256.fullmatch(digest), "claimed projection hash is invalid")
    require(projection_hash(manifest) == manifest.get("plan_manifest_sha256") == digest,
            "plan projection hash does not match the request")
    require(manifest.get("source_sha") == env.get("SOURCE_SHA"), "source SHA does not match the request")
    target = manifest.get("target")
    require(isinstance(target, dict) and set(target) == {"host", "database"}, "plan target is invalid")
    require(isinstance(target.get("host"), str)
            and re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9.-]{0,252}", target["host"])
            and isinstance(target.get("database"), str)
            and re.fullmatch(r"[a-zA-Z0-9_.-]{1,63}", target["database"])
            and all("prod" not in value.lower() for value in target.values()),
            "target is not a bare staging identity")
    require(target == {"host": env.get("STAGING_HOST"), "database": env.get("STAGING_DATABASE")},
            "plan target does not match the request")
    for key, env_key in (("recovery_evidence_ref", "RECOVERY_REF"), ("acl_plan_ref", "ACL_REF")):
        value = env.get(env_key, "")
        # Match the runner's is_ref contract; punctuation is not a URL or credential.
        require(isinstance(value, str) and 0 < len(value.encode("utf-8")) <= 200
                and not any(c.isspace() for c in value)
                and "@" not in value and "://" not in value,
                "approval reference is not a bare reference")
        require(manifest.get(key) == value, "approval reference does not match the request")
    raw = env.get("EXPECTED_PENDING", "").strip()
    parts = [] if not raw else [part.strip() for part in raw.split(",")]
    require(all(re.fullmatch(r"[+]?[0-9]+", part) for part in parts),
            "requested pending versions are invalid")
    try:
        requested = versions([int(part) for part in parts])
    except ValueError:
        raise Refused("requested pending versions are invalid") from None
    require(requested == versions(manifest.get("pending_before")),
            "pending versions do not match the request")
    pending = [str(version) for version in requested]
    return {
        "schema_version": 1,
        "kind": CLAIM_KIND,
        "repository": REPOSITORY,
        "workflow_path": WORKFLOW_PATH,
        "environment_name": "staging-migrate-apply",
        "apply_run_id": env["GITHUB_RUN_ID"],
        "apply_run_attempt": env["GITHUB_RUN_ATTEMPT"],
        "workflow_head_sha": head,
        "plan_run_id": env["PLAN_RUN_ID"],
        "plan_manifest_sha256": digest,
        "source_sha": env["SOURCE_SHA"],
        "target": target,
        "expected_pending": pending,
        "recovery_evidence_ref": env["RECOVERY_REF"],
        "acl_plan_ref": env["ACL_REF"],
    }


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "manifest has duplicate fields")
        result[key] = value
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        require(args.manifest.stat().st_size <= MAX_MANIFEST_BYTES, "manifest is too large")
        manifest = json.loads(args.manifest.read_text(), object_pairs_hook=unique_object)
        claim = build_claim(manifest, os.environ)
        # Exclusive create: a refusal never leaves a claim, nor reuses an old one.
        with args.output.open("x") as output:
            output.write(json.dumps(claim, indent=2) + "\n")
    except (OSError, UnicodeError, json.JSONDecodeError, Refused) as error:
        message = str(error) if isinstance(error, Refused) else "manifest or output is unavailable/invalid"
        print(f"staging migration claim refused: {message}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
