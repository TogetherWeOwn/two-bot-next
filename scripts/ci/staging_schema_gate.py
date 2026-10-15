#!/usr/bin/env python3
"""Pre-deploy schema-drift gate for deploy-staging (read-only plan, fail closed).

Runs the reviewed `staging-migrate --plan` binary against the staging
database through the no-reviewer `staging-migrate-plan` environment binding
and refuses the deploy when `pending_before` is non-empty. The host,
database and branch pins are derived from that same binding URL (parsed, not
printed), so the gate needs no new secret, input or reviewer; a pooled,
unparseable or mismatched binding refuses before any plan runs. DDL never
runs here: drift is fixed by the manual `staging-migrate` apply dispatch,
exactly as the drift message says.
"""
import argparse
import json
import os
import subprocess
import sys
from urllib.parse import urlparse

PLAN_URL_ENV = "TWO_BOT_STAGING_PLAN_DATABASE_URL"
# Bare, non-secret provenance refs recorded in the plan manifest. The gate is
# the evidence: a read-only plan needs no human review reference.
GATE_REF = "deploy-staging-schema-gate"
# Mirror the staging-migrate plan warning (CONNECTION_WARN_PCT there):
# strictly above 80% of max_connections warns, never fails.
CONNECTION_WARN_PCT = 80.0
I64_MAX = 2**63 - 1


class Refused(ValueError):
    """A bounded, non-secret refusal; never include rejected input in errors."""


def require(condition, message):
    if not condition:
        raise Refused(message)


def pins_from_url(url):
    """Derive (host, database, branch_id) pins from the plan binding URL.

    Branch pin applies only to PlanetScale hosts (username suffix
    `{role}.{branch_id}`); every other host takes an empty pin.
    """
    require(isinstance(url, str) and url, "plan database binding is not provided")
    try:
        parsed = urlparse(url)
    except ValueError:
        raise Refused("plan database binding is not a valid database URL")
    require(parsed.scheme in ("postgresql", "postgres"), "plan database binding is not a postgres URL")
    host = (parsed.hostname or "").lower()
    require(host and "@" not in host and "/" not in host, "plan database host is not a bare name")
    require("-pooler" not in host, "pooler endpoints are refused; use the direct endpoint")
    port = parsed.port
    require(port is None or port == 5432, "pooled ports are refused; use the direct 5432 endpoint")
    username = (parsed.username or "")
    require("|" not in username, "pooler-style usernames are refused; use the direct role login")
    database = (parsed.path or "").lstrip("/").split("?", 1)[0]
    require(database and "@" not in database and "/" not in database,
            "plan database name is not a bare name")
    branch = ""
    if host.endswith(".psdb.cloud"):
        suffix = username.rsplit(".", 1)
        require(len(suffix) == 2 and suffix[1], "binding branch does not pin a staging branch")
        branch = suffix[1]
    return host, database, branch


def pending_versions(manifest):
    """Return the strictly-ascending pending list; refuse malformed manifests."""
    require(isinstance(manifest, dict), "plan manifest is not an object")
    pending = manifest.get("pending_before")
    require(isinstance(pending, list), "pending versions are invalid")
    require(all(type(v) is int and 0 < v <= I64_MAX for v in pending),
            "pending version is outside the positive i64 range")
    require(pending == sorted(set(pending)), "pending versions are not strictly ascending")
    return pending


def drift_message(pending, source_sha):
    """Fail-closed deploy refusal naming the drift and its manual fix."""
    versions = ",".join(str(v) for v in pending)
    return (
        f"schema drift: staging database is behind source {source_sha}: "
        f"pending versions [{versions}]. Run the manual staging-migrate apply "
        f"dispatch for that source with expected_pending \"{versions}\" "
        "(reviewed plan required), then re-run deploy-staging."
    )


def connection_warning(manifest):
    """Surface a hot staging pool in logs; never a gate failure.

    Reads the real plan-manifest shape the binary emits:
    ``manifest["audit"]["connection_usage"]`` with ``used`` and
    ``max_connections`` counts. Degenerate or absent usage never warns,
    mirroring the binary's own warning boundary (strictly above 80%).
    """
    if not isinstance(manifest, dict):
        return None
    audit = manifest.get("audit")
    usage = audit.get("connection_usage") if isinstance(audit, dict) else None
    if not isinstance(usage, dict):
        return None
    used = usage.get("used")
    maximum = usage.get("max_connections")
    if isinstance(used, bool) or not isinstance(used, int) or used < 0:
        return None
    if isinstance(maximum, bool) or not isinstance(maximum, int) or maximum <= 0:
        return None
    if used / maximum * 100.0 > CONNECTION_WARN_PCT:
        return (
            f"staging database connection usage is {used}/{maximum} "
            f"({used / maximum:.0%} of max_connections); "
            "logins may refuse under saturation."
        )
    return None


def run_gate(binary, source_sha, url):
    """Run the plan binary and return its manifest dict (raises on refusal)."""
    host, database, branch = pins_from_url(url)
    proc = subprocess.run(
        [binary, "--target", "staging", "--plan", "--source-sha", source_sha,
         "--staging-host", host, "--staging-database", database,
         "--staging-branch-id", branch,
         "--recovery-evidence-ref", GATE_REF, "--acl-plan-ref", GATE_REF],
        capture_output=True, text=True, check=False, timeout=600,
    )
    if proc.returncode != 0:
        raise Refused(
            f"staging-migrate plan refused the gate (exit {proc.returncode}): "
            f"{proc.stderr.strip().splitlines()[-1] if proc.stderr.strip() else 'no detail'}"
        )
    try:
        manifest = json.loads(proc.stdout.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        raise Refused("plan manifest is not valid JSON")
    return manifest


def main(argv=None, env=None, run=None):
    parser = argparse.ArgumentParser(description="Fail closed on staging schema drift.")
    parser.add_argument("--binary", required=True, help="built staging-migrate binary")
    parser.add_argument("--source-sha", required=True, help="full 40-char deploy SHA")
    parser.add_argument("--manifest-out", default="staging-schema-gate-manifest.json")
    args = parser.parse_args(argv)
    env = os.environ if env is None else env
    run = run_gate if run is None else run
    require(len(args.source_sha) == 40, "source SHA must be a full 40-char commit")
    manifest = run(args.binary, args.source_sha, env.get(PLAN_URL_ENV, ""))
    with open(args.manifest_out, "w", encoding="utf-8") as handle:
        json.dump(manifest, handle, sort_keys=True)
        handle.write("\n")
    pending = pending_versions(manifest)
    warning = connection_warning(manifest)
    if warning is not None:
        print(warning)
    if pending:
        print(drift_message(pending, args.source_sha), file=sys.stderr)
        return 1
    print(f"schema gate clean: no pending migrations on {args.source_sha}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Refused as error:
        print(f"staging-schema-gate refused: {error}", file=sys.stderr)
        sys.exit(2)
