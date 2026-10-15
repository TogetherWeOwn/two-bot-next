#!/usr/bin/env python3
"""Fail-fast staging schema gate for deploy-staging (TOG-24362).

Evaluates the read-only `staging-migrate --target staging --plan` manifest
the schema-gate job prints at the deploy SHA, and refuses (exit 1) when
staging would serve drifted schema or grants: pending migrations, role-verify
findings, or an unreadable audit. Prints only counts and versions; the
manifest carries no credentials.

The gate never migrates and never grants: remediation stays manual
(staging-migrate plan then apply for migrations, host role-plan re-apply per
docs/database-roles.md for grants), then re-run deploy-staging.
"""
import argparse
import json
import re
import sys

MAX_MANIFEST_BYTES = 1024 * 1024
SHA40 = re.compile(r"[0-9a-f]{40}\Z")
I64_MAX = 2**63 - 1
# Findings render from reviewed SQL, but cap what reaches the log anyway.
MAX_FINDINGS_SHOWN = 20
MAX_PROBLEM_CHARS = 2000


def versions(values):
    """Pending versions are positive i64s in strictly ascending order."""
    if not isinstance(values, list):
        return None
    if any(type(v) is not int or not 0 < v <= I64_MAX for v in values):
        return None
    if values != sorted(set(values)):
        return None
    return values


def evaluate(manifest, expected_sha):
    """Return the list of drift problems; empty means the deploy may proceed."""
    if not isinstance(manifest, dict):
        return ["schema gate refused: manifest is not a JSON object"]
    if manifest.get("mode") != "plan":
        return ["schema gate refused: manifest is not a read-only plan readout"]
    if manifest.get("migration_target") != "staging":
        return ["schema gate refused: manifest target is not staging"]
    source_sha = manifest.get("source_sha")
    if not isinstance(source_sha, str) or not SHA40.match(source_sha):
        return ["schema gate refused: manifest source SHA is not a 40-char hex commit"]
    if source_sha != expected_sha:
        return [f"schema gate refused: manifest is for {source_sha}, "
                f"deploying {expected_sha}; re-run the gate at the deploy SHA"]
    pending = versions(manifest.get("pending_before"))
    if pending is None:
        return ["schema gate refused: pending_before is not an ascending version list"]
    audit = manifest.get("audit")
    if not isinstance(audit, dict) or "error" in audit:
        return ["schema gate refused: role audit is unreadable; "
                "re-run the gate, then staging-migrate plan if it persists"]
    findings = audit.get("verify_findings")
    if not isinstance(findings, list) or any(type(f) is not str for f in findings):
        return ["schema gate refused: verify_findings is not a string list"]
    problems = []
    if pending:
        listed = ",".join(str(v) for v in pending)
        problems.append(
            f"staging schema drift: {len(pending)} migration(s) pending at "
            f"{source_sha} ({listed}); run staging-migrate plan then apply for "
            f"this SHA, then re-run deploy-staging")
    if findings:
        shown = "; ".join(findings[:MAX_FINDINGS_SHOWN])
        more = f" (+{len(findings) - MAX_FINDINGS_SHOWN} more)" if len(findings) > MAX_FINDINGS_SHOWN else ""
        problems.append(
            f"staging grant drift: {len(findings)} role-verify finding(s): "
            f"{shown}{more}; re-apply the staging role plan "
            f"(sql/database_roles.sql) per docs/database-roles.md, "
            f"then re-run deploy-staging")
    return [p[:MAX_PROBLEM_CHARS] for p in problems]


def main(argv):
    parser = argparse.ArgumentParser(description="Fail fast when staging schema or grants drifted.")
    parser.add_argument("--manifest", required=True, help="Path to the plan manifest JSON.")
    parser.add_argument("--expected-sha", required=True, help="40-char deploy SHA the manifest must name.")
    args = parser.parse_args(argv)
    if not SHA40.match(args.expected_sha or ""):
        print("schema gate refused: expected SHA is not a 40-char hex commit", file=sys.stderr)
        return 2
    try:
        with open(args.manifest, "rb") as handle:
            raw = handle.read(MAX_MANIFEST_BYTES + 1)
        if len(raw) > MAX_MANIFEST_BYTES:
            print("schema gate refused: manifest exceeds size bound", file=sys.stderr)
            return 2
        manifest = json.loads(raw)
    except (OSError, ValueError) as exc:
        print(f"schema gate refused: manifest unreadable ({type(exc).__name__})", file=sys.stderr)
        return 2
    problems = evaluate(manifest, args.expected_sha)
    if problems:
        for problem in problems:
            print(f"schema-gate FAILED: {problem}", file=sys.stderr)
        return 1
    print(f"schema-gate passed: no pending migrations and no role findings at {args.expected_sha}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
