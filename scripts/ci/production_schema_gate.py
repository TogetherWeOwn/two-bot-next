#!/usr/bin/env python3
"""Pre-deploy schema-drift gate for deploy-production (read-only plan, fail closed).

Runs the reviewed `staging-migrate --target production --plan` binary against
the production database through the no-reviewer `production-migrate-plan`
environment binding and refuses a new build when either `pending_before` or
`pending_store_before` is non-empty. The expected host, database and branch pins are non-secret
workflow vars (`vars.TWO_BOT_PRODUCTION_HOST/DATABASE/BRANCH_ID`), read here
from the environment
independently of the secret binding; a pooled, unparseable or mismatched
binding refuses before any plan runs, exactly as the manual plan does. DDL
never runs here: drift is fixed by the manual `production-migrate` apply
dispatch named in the failure, never by this workflow.

A rollback restores the previously serving Worker version and container image
without introducing a new build, and a live-SHA takeover redeploy re-serves
the build already running in production: neither changes the schema the
source needs, so both waive this gate (recorded in the manifest, never a
silent skip). Every other deploy must prove a drift-free production ledger
first: the 10-15 incident crash-looped a build whose migrations (event-store
capture, retention queues, member catch-up) staging had and production
lacked, and the release gate cannot catch a missing table the binary needs
at gateway startup.
"""

import os
import subprocess
import sys
from urllib.parse import urlparse

from schema_gate_common import (
    GateArgumentParser, I64_MAX, Refused, cli, pending_versions, require,
    result, run_plan, validate_manifest, validate_source_sha, verified_pins,
    write_manifest,
)

PLAN_URL_ENV = "TWO_BOT_PRODUCTION_PLAN_DATABASE_URL"
# Non-secret expected production identity, wired in deploy-production.yml from
# `vars.*` (never from the secret binding: a binding compared with itself
# cannot mismatch). Names match the binary's
# --production-host/database/branch-id flags; the binary re-checks the same
# comparison in verify_target.
EXPECTED_HOST_ENV = "TWO_BOT_PRODUCTION_HOST"
EXPECTED_DATABASE_ENV = "TWO_BOT_PRODUCTION_DATABASE"
EXPECTED_BRANCH_ENV = "TWO_BOT_PRODUCTION_BRANCH_ID"
# Bare, non-secret provenance refs recorded in the plan manifest. The gate is
# the evidence: a read-only plan needs no human review reference.
GATE_REF = "deploy-production-schema-gate"
# Mirror the staging-migrate plan warning (CONNECTION_WARN_PCT there):
# strictly above 80% of the effective (non-superuser) ceiling warns,
# never fails.
CONNECTION_WARN_PCT = 80.0


def is_staging_host(host_lower):
    """Mirror the binary's staging fence: production never targets these."""
    return (
        host_lower == "agent-testdb"
        or host_lower == "127.0.0.1"
        or host_lower == "localhost"
        or "staging" in host_lower
        or "neon.tech" in host_lower
    )


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
    try:
        port = parsed.port
    except ValueError:
        raise Refused("plan database binding is not a valid database URL") from None
    require(port is None or port == 5432, "pooled ports are refused; use the direct 5432 endpoint")
    username = (parsed.username or "")
    require("|" not in username, "pooler-style usernames are refused; use the direct role login")
    database = (parsed.path or "").lstrip("/").split("?", 1)[0]
    require(database and "@" not in database and "/" not in database,
            "plan database name is not a bare name")
    branch = ""
    if host.endswith(".psdb.cloud"):
        suffix = username.rsplit(".", 1)
        require(len(suffix) == 2 and suffix[1], "binding branch does not pin a production branch")
        branch = suffix[1]
    return host, database, branch


def branch_id_valid(value):
    """Mirror the binary's bare branch-id shape (alphanumerics, dash, underscore, <=63)."""
    return (
        isinstance(value, str)
        and 0 < len(value) <= 63
        and all(ch.isascii() and (ch.isalnum() or ch in "-_") for ch in value)
    )


def verify_binding(url, expected_host, expected_database, expected_branch):
    """Refuse unless the secret binding matches the independently pinned identity.

    Mirrors `validate_request` + `verify_target` in staging_migrate.rs for the
    production target: the expected pins are production-fenced (non-empty bare
    names, never a staging host; the `prod` substring stays allowed because
    the production host and database are production-like by construction; a
    PlanetScale pin requires a bare branch id), the binding shape is parsed
    by `pins_from_url`, and the binding host/database must equal the pins
    (host case-insensitively, database exactly). A non-empty expected branch
    must equal the binding username's `{role}.{branch_id}` suffix
    case-insensitively; an empty expected branch refuses a PlanetScale
    binding, so an unpinned gate cannot read an unpinned branch. Every
    refusal is a fixed string: nothing echoes the URL or the pins.
    """
    host_pin = (expected_host or "").strip()
    db_pin = (expected_database or "").strip()
    branch_pin = (expected_branch or "").strip()
    for label, pin in (("expected host", host_pin), ("expected database", db_pin)):
        require(pin and "@" not in pin and "/" not in pin,
                f"{label} is empty or not a bare name")
    require("-pooler" not in host_pin.lower(), "expected host is a pooler endpoint")
    require(not is_staging_host(host_pin.lower()),
            "production target refuses a staging host pin; use the production host")
    if branch_pin:
        require(branch_id_valid(branch_pin), "expected branch id is not a bare branch pin")
    elif host_pin.lower().endswith(".psdb.cloud"):
        raise Refused("PlanetScale hosts require TWO_BOT_PRODUCTION_BRANCH_ID")
    host, database, branch = pins_from_url(url)
    require(not is_staging_host(host),
            "production target refuses a staging binding host; use the production binding")
    require(
        host == host_pin.lower() and database == db_pin,
        "plan database binding does not match the verified production identity",
    )
    if branch_pin:
        require(
            branch and branch.lower() == branch_pin.lower(),
            "plan database binding does not match the verified production branch",
        )
    elif host.endswith(".psdb.cloud"):
        raise Refused("plan database binding is an unpinned PlanetScale branch")
    return host_pin, db_pin, branch_pin


def drift_message(pending, source_sha, store_pending=()):
    """Fail-closed deploy refusal naming each lagging chain and its manual fix."""
    chains = []
    if pending:
        versions = ",".join(str(v) for v in pending)
        chains.append(f"cutover chain pending versions [{versions}]")
    if store_pending:
        store_versions = ",".join(str(v) for v in store_pending)
        chains.append(f"store chain pending versions [{store_versions}]")
    require(chains, "drift message requires at least one pending chain")
    message = (
        f"schema drift: production database is behind source {source_sha}: "
        + "; ".join(chains)
        + ". Run the manual production-migrate apply dispatch for that source"
    )
    if pending:
        versions = ",".join(str(v) for v in pending)
        message += f" with expected_pending \"{versions}\""
    if store_pending:
        store_versions = ",".join(str(v) for v in store_pending)
        message += f" with expected_store_pending \"{store_versions}\""
    return message + " (reviewed plan required), then re-dispatch deploy-production."


def waive_reason(mode, allow_unstaged):
    """Why this run skips the ledger check, or None when it must run."""
    if mode == "rollback":
        return "rollback restores the previously serving build without a new schema"
    if allow_unstaged == "true":
        return "live-SHA takeover redeploy re-serves the running build without a new schema"
    return None


def connection_warning(manifest):
    """Surface a hot production pool in logs; never a gate failure.

    Reads the real plan-manifest shape the binary emits:
    ``manifest["audit"]["connection_usage"]`` with ``used`` and
    ``max_connections`` counts. The ratio uses the non-superuser ceiling
    the binary warns on: ``effective_max_connections`` when present, else
    ``max_connections - reserved_connections``, else raw ``max_connections``
    (older manifests). Degenerate or absent usage never warns, mirroring
    the binary's own warning boundary (strictly above 80%).
    """
    if not isinstance(manifest, dict):
        return None
    audit = manifest.get("audit")
    usage = audit.get("connection_usage") if isinstance(audit, dict) else None
    if not isinstance(usage, dict):
        return None
    used = usage.get("used")
    maximum = usage.get("max_connections")
    if type(used) is not int or not 0 <= used <= I64_MAX:
        return None
    if type(maximum) is not int or not 0 < maximum <= I64_MAX:
        return None
    ceiling = usage.get("effective_max_connections")
    ceiling_label = "the non-superuser connection ceiling"
    if type(ceiling) is not int or not 0 < ceiling <= I64_MAX:
        reserved = usage.get("reserved_connections")
        if type(reserved) is not int or not 0 <= reserved <= I64_MAX:
            ceiling = maximum
            ceiling_label = "max_connections"
        else:
            ceiling = max(maximum - max(reserved, 0), 1)
    if used / ceiling * 100.0 > CONNECTION_WARN_PCT:
        return (
            f"production database connection usage is {used}/{ceiling} "
            f"({used / ceiling:.0%} of {ceiling_label}); "
            "logins may refuse under saturation."
        )
    return None


def run_gate(binary, source_sha, url, expected_host, expected_database, expected_branch):
    """Run the plan binary and return its manifest dict (raises on refusal).

    The independently pinned identity is verified against the binding first,
    then passed to the binary (never derived from the binding), so the
    binary's own `verify_target` compares the secret with the pins, as the
    manual plan does.
    """
    validate_source_sha(source_sha)
    host, database, branch = verified_pins(
        verify_binding, url, expected_host, expected_database, expected_branch,
    )
    return run_plan(
        [binary, "--target", "production", "--plan", "--source-sha", source_sha,
         "--production-host", host, "--production-database", database,
         "--production-branch-id", branch,
         "--recovery-evidence-ref", GATE_REF, "--acl-plan-ref", GATE_REF],
        run=subprocess.run,
    )


def main(argv=None, env=None, run=None):
    parser = GateArgumentParser(description="Fail closed on production schema drift.")
    parser.add_argument("--binary", required=True, help="built staging-migrate binary")
    parser.add_argument("--source-sha", required=True, help="canonical lowercase 40-hex deploy SHA")
    parser.add_argument("--mode", choices=("deploy", "rollback"), required=True,
                        help="guard-verified run mode; rollback waives the ledger check")
    parser.add_argument("--allow-unstaged", choices=("true", "false"), default="false",
                        help="guard-verified live-SHA redeploy flag; true waives the ledger check")
    parser.add_argument("--manifest-out", default="production-schema-gate-manifest.json")
    args = parser.parse_args(argv)
    env = os.environ if env is None else env
    run = run_gate if run is None else run
    validate_source_sha(args.source_sha)
    reason = waive_reason(args.mode, args.allow_unstaged)
    if reason is not None:
        write_manifest(args.manifest_out, {"waived": reason, "mode": args.mode,
                                          "source_sha": args.source_sha})
        print(f"schema gate waived: {reason}")
        return result("recovery_waived")
    url = env.get(PLAN_URL_ENV, "")
    pins = verified_pins(
        verify_binding, url, env.get(EXPECTED_HOST_ENV, ""),
        env.get(EXPECTED_DATABASE_ENV, ""), env.get(EXPECTED_BRANCH_ENV, ""),
    )
    manifest = run(args.binary, args.source_sha, url, *pins)
    pending, store_pending = validate_manifest(manifest, args.source_sha, "production", pins)
    write_manifest(args.manifest_out, manifest)
    warning = connection_warning(manifest)
    if warning is not None:
        print(warning)
    if pending or store_pending:
        print(drift_message(pending, args.source_sha, store_pending), file=sys.stderr)
        return result("schema_drift")
    print(f"schema gate clean: no pending migrations on {args.source_sha}")
    return result("schema_clean")


if __name__ == "__main__":
    sys.exit(cli(main))
