#!/usr/bin/env python3
"""Pre-deploy role-drift gate for deploy-staging/deploy-production (read-only, fail closed).

Runs the reviewed `two-bot db roles verify` binary against the target
database through the no-reviewer `*-migrate-plan` environment binding (a
login holding only `two_bot_migrator_ro`, physically read-only) and refuses
the deploy on any finding. The expected host, database and branch pins are
non-secret workflow vars, checked against the binding with the same rules as
the schema gates (imported, never reimplemented); a pooled, unparseable or
mismatched binding refuses before any verify runs. The gate never applies:
drift is fixed by the operator reapplying the full role plan
(`two-bot db roles plan`) until `verify` reads 0, never by this workflow.

Migrations create tables but grant nothing: the 2026-10-16 production deploy
applied migrations 0424-0441 without reapplying the role plan, the runtime
role lacked grants on the new tables (SQLSTATE 42501), and the gateway
crash-looped while the schema-drift gate stayed green (it checks migrations,
not grants). After every migrate apply the operator must reapply the role
plan; this gate enforces it before the next deploy.

A rollback restores the previously serving Worker version and container image
without introducing a new build, and a live-SHA takeover redeploy re-serves
the build already running in production: neither needs new-table grants, so
both waive this gate (recorded in the manifest, never a silent skip), exactly
as the schema gate waives its ledger check. Every other deploy must prove a
drift-free grant set first.
"""

import argparse
import json
import os
import subprocess
import sys

from production_schema_gate import Refused as ProductionRefused
from production_schema_gate import verify_binding as verify_production_binding
from staging_schema_gate import Refused as StagingRefused
from staging_schema_gate import verify_binding as verify_staging_binding

# Read-only audit login, shared with the migrate plan step: the gate bridges
# this secret binding to TWO_DATABASE_URL for the child verify process only.
# The value never appears in logs, errors or the manifest.
DATABASE_URL_ENV = "TWO_DATABASE_URL"
# Bare, non-secret provenance refs recorded in the gate manifest. The gate is
# the evidence: a read-only verify needs no human review reference.
GATE_REF = "deploy-roles-gate"
TARGETS = ("staging", "production")


class Refused(ValueError):
    """A bounded, non-secret refusal; never include rejected input in errors."""


def require(condition, message):
    if not condition:
        raise Refused(message)


def verify_binding(target, url, expected_host, expected_database, expected_branch):
    """Refuse unless the secret binding matches the independently pinned identity.

    Dispatches to the schema gate's own binding rules for the target (never
    reimplemented here), so a wrong-target binding refuses instead of reading
    the wrong database.
    """
    require(target in TARGETS, "verify target must be staging or production")
    try:
        if target == "staging":
            return verify_staging_binding(url, expected_host, expected_database, expected_branch)
        return verify_production_binding(url, expected_host, expected_database, expected_branch)
    except (StagingRefused, ProductionRefused) as error:
        raise Refused(str(error))


def findings_from_output(stdout, stderr):
    """Collect the `drift: ...` finding lines the CLI prints; never the contents."""
    findings = []
    for line in (stderr + "\n" + stdout).splitlines():
        if line.startswith("drift: "):
            finding = line[len("drift: "):].strip()
            if finding:
                findings.append(finding)
    return findings


def validated_findings(findings):
    """Return the finding list; refuse malformed shapes instead of passing."""
    require(
        isinstance(findings, list)
        and all(type(finding) is str and finding for finding in findings),
        "roles findings are invalid",
    )
    return findings


def drift_message(findings, target):
    """Fail-closed deploy refusal naming the count and its operator fix."""
    require(target in TARGETS, "verify target must be staging or production")
    count = len(validated_findings(findings))
    require(count > 0, "role drift needs at least one finding")
    rerun = "re-run deploy-staging" if target == "staging" else "re-dispatch deploy-production"
    return (
        f"role drift: {target} database has {count} finding(s). "
        f"Migrations create tables but grant nothing: ask the operator to "
        f"reapply the full role plan (`two-bot db roles plan`) and confirm "
        f"`two-bot db roles verify` reads 0, then {rerun}."
    )


def waive_reason(mode, allow_unstaged):
    """Why this run skips the findings check, or None when it must run."""
    if mode == "rollback":
        return "rollback restores the previously serving build without new-table grants"
    if allow_unstaged == "true":
        return "live-SHA takeover redeploy re-serves the running build without new-table grants"
    return None


def run_gate(binary, target, url, expected_host, expected_database, expected_branch):
    """Run `two-bot db roles verify` and return its findings list (raises on refusal).

    The independently pinned identity is verified against the binding first
    (never derived from it). Exit 0 means zero findings; exit 1 with `drift:`
    lines returns them for the fail-closed message; any other outcome
    (connection failure, usage error) refuses without echoing the binding.
    """
    verify_binding(target, url, expected_host, expected_database, expected_branch)
    env = dict(os.environ)
    env[DATABASE_URL_ENV] = url
    try:
        proc = subprocess.run(
            [binary, "db", "roles", "verify"],
            capture_output=True, text=True, check=False, timeout=600, env=env,
        )
    except (OSError, subprocess.SubprocessError):
        raise Refused("roles verify refused to run against the target database")
    stdout = proc.stdout or ""
    stderr = proc.stderr or ""
    findings = findings_from_output(stdout, stderr)
    if proc.returncode == 0:
        if any(line.startswith("DATABASE ROLES VERIFIED") for line in stdout.splitlines()) and not findings:
            return []
        raise Refused("roles verify returned an unverified clean result")
    if proc.returncode == 1 and findings:
        return findings
    raise Refused(f"roles verify refused the gate (exit {proc.returncode})")


def main(argv=None, env=None, run=None):
    parser = argparse.ArgumentParser(description="Fail closed on database role drift.")
    parser.add_argument("--binary", required=True, help="built two-bot binary")
    parser.add_argument("--target", choices=TARGETS, required=True,
                        help="which plan binding and pin rules apply")
    parser.add_argument("--expected-host", required=True,
                        help="non-secret pinned endpoint host (vars, never the binding)")
    parser.add_argument("--expected-database", required=True,
                        help="non-secret pinned database name (vars, never the binding)")
    parser.add_argument("--expected-branch-id", default="",
                        help="non-secret pinned branch id (required for *.psdb.cloud hosts)")
    parser.add_argument("--mode", choices=("deploy", "rollback"), default="deploy",
                        help="guard-verified run mode; rollback waives the findings check")
    parser.add_argument("--allow-unstaged", choices=("true", "false"), default="false",
                        help="guard-verified live-SHA redeploy flag; true waives the findings check")
    parser.add_argument("--manifest-out", default="roles-gate-manifest.json")
    args = parser.parse_args(argv)
    env = os.environ if env is None else env
    run = run_gate if run is None else run
    reason = waive_reason(args.mode, args.allow_unstaged)
    if reason is not None:
        with open(args.manifest_out, "w", encoding="utf-8") as handle:
            json.dump({"waived": reason, "mode": args.mode,
                       "target": args.target, "ref": GATE_REF}, handle, sort_keys=True)
            handle.write("\n")
        print(f"roles gate waived: {reason}")
        return 0
    findings = validated_findings(run(
        args.binary,
        args.target,
        env.get(DATABASE_URL_ENV, ""),
        args.expected_host,
        args.expected_database,
        args.expected_branch_id,
    ))
    with open(args.manifest_out, "w", encoding="utf-8") as handle:
        json.dump({"target": args.target, "findings": findings,
                   "ref": GATE_REF}, handle, sort_keys=True)
        handle.write("\n")
    if findings:
        print(drift_message(findings, args.target), file=sys.stderr)
        return 1
    print(f"roles gate clean: no findings on {args.target}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Refused as error:
        print(f"roles-gate refused: {error}", file=sys.stderr)
        sys.exit(2)
