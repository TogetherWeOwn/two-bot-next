#!/usr/bin/env python3
"""Dependency-aware selector for the check.yml test jobs (TOG-12057).

Generalizes the merged TOG-12056 container-inputs pattern to the whole
matrix: PRs that touch only worker UI or unrelated docs skip the ~30-minute
Rust jobs, and Rust-only PRs skip the worker job. Fail-closed end to end:
any unrecognized path selects every job, so release-candidate and staging
validation (push to main always selects everything) never lose coverage.

Job map (each a pure function of the changed paths):
- ``rust``: the ``check`` aggregator's cargo suite plus the ``self-role-store``,
  ``community-db``, ``feeds-db`` and ``tickets-postgres`` jobs.
- ``worker``: the ``worker check`` job (npm typecheck/tests plus the offline
  script verifications it runs).
- ``parity``: the ``parity-docs`` job (baseline ancestry + link guard).

Coupling notes (verified 2026-10-02, enforced by test_job_inputs.py):
- Rust tests read a pinned doc set by content (parity/cutover/commands/
  configuration/voice-rooms plus the soak/parity JSON): those docs select
  ``rust``. staging-soak/backup/preflight/runbook are existence-only inputs
  to the preconditions binary (is_file, never content), so a content edit
  cannot break Rust and they fall through to the worker rule below; any
  deletion forces all jobs via the deletion rule. Every other doc has no
  file reader in PR-run code, so docs-only PRs skip the Rust matrix -- but
  ANY docs/ change still selects ``worker`` because
  wrangler/test/runbook.test.ts asserts on the docs/ directory listing
  itself.
- The worker drift test (wrangler/test/container-env.test.ts) reads every
  ``TWO_*`` name in crates/ and src/, so Rust changes select ``worker``.
- The corpus test (crates/core/tests/voice_template_corpus.rs) includes
  tests/voice_templates/corpus.json, and validate.py pins docs/voice-rooms.md:
  both select ``rust``. Coverage and validator assets under
  tests/voice_templates/ (coverage.json, test_validator.py, README.md) are
  validated by the check job's hermetic offline step, which always runs, so
  an asset-only change there selects no job and skips the heavy matrix.
- Worker verification steps execute scripts/test-*.py against the checkout, so
  scripts/ changes select ``rust`` + ``worker`` (parity scripts add parity).
- No workflow-level ``paths:`` filter: that would skip the whole workflow
  including the always-reporting ``check`` aggregator. Selection is per-job
  via the ``job-inputs`` job's outputs.
"""

import argparse
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

RUST = "rust"
WORKER = "worker"
PARITY = "parity"
ALL_JOBS = frozenset({RUST, WORKER, PARITY})
NO_JOBS = frozenset()

# Exact files that revalidate everything (gate wiring, manifests).
ALL_EXACT = frozenset({
    ".github/workflows/check.yml",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "deny.toml",
    "migrations.lock",
})

# Rust sources and their fixtures: the worker drift test
# (wrangler/test/container-env.test.ts) reads every TWO_* name in crates/
# AND src/, so Rust changes select the worker job too. The legacy-registry
# fixture is read by the parity job as well.
RUST_PREFIXES = (
    "sql/",
    ".cargo/",
)
RUST_WORKER_PREFIXES = (
    "crates/",
    "src/",
)
TESTS_PREFIX = "tests/"

# Voice-template fixtures: only the corpus is compiled into Rust tests via
# include_str! (voice_template_corpus.rs, voice_conditions.rs,
# voice_conditions_golden.rs). The validator pins docs/voice-rooms.md and
# runs hermetically in the check job's always-on offline step, so validator
# and coverage assets need no heavy job.
VOICE_TEMPLATES_PREFIX = "tests/voice_templates/"
VOICE_TEMPLATE_RUST_INPUTS = frozenset({
    "tests/voice_templates/corpus.json",
    "tests/voice_templates/validate.py",
})

# scripts/ verifications run in both the check and worker jobs; parity
# scripts additionally gate the parity-docs job.
SCRIPTS_PREFIX = "scripts/"
PARITY_SCRIPT_MARKER = "parity"

# The whole worker tree selects the worker job (typecheck, tests, and the
# env-bindings scripts under wrangler/scripts/).
WORKER_PREFIX = "wrangler/"

# Docs whose CONTENT is read in PR-run code (proven by test_job_inputs.py's
# reader-coverage test): parity/cutover by the preconditions binary,
# commands/configuration by the reference_docs test, voice-rooms by the
# fixture validator, soak/parity by the checklist and baseline guards.
# parity.md and the baseline also gate parity-docs. Existence-only docs
# (staging-soak/backup/preflight: the preconditions binary checks is_file,
# never content) and runbook/container-readiness (worker tests only) fall
# through to the worker-only docs rule below; their absence is covered by
# the deletion rule in selection().
RUST_DOCS = frozenset({
    "docs/parity.md",
    "docs/cutover.md",
    "docs/commands.md",
    "docs/configuration.md",
    "docs/voice-rooms.md",
    "docs/voice-conditions-core.md",
    "docs/soak-checklist.json",
    "docs/soak-checklist.md",
})
PARITY_DOCS = frozenset({
    "docs/parity.md",
    "docs/parity-baseline.json",
})
WORKER_DOCS = frozenset({
    "docs/runbook.md",
    "docs/container-readiness.md",
})
DOCS_PREFIX = "docs/"

# Exact workflow files whose offline verifications run in the worker job.
WORKER_WORKFLOWS = frozenset({
    ".github/workflows/supply-chain.yml",
    ".github/workflows/release.yml",
    ".github/workflows/deploy-production.yml",
    ".github/workflows/deploy-staging.yml",
})
WORKFLOWS_PREFIX = ".github/workflows/"

# Repo chrome no job reads: licenses, editor/ignore files, secret config
# (gitleaks runs in its own workflow), PR template/owners, dependabot, and
# root markdown (proven reader-free by test_job_inputs.py).
SKIP_EXACT = frozenset({
    "LICENSE",
    "CHANGELOG.md",
    ".editorconfig",
    ".gitignore",
    ".gitleaks.toml",
    ".gitleaksignore",
    ".github/pull_request_template.md",
    ".github/CODEOWNERS",
    ".github/dependabot.yml",
})

# The release-please config is read by the worker job's release-lifecycle
# verification step.
WORKER_EXACT = frozenset({
    "release-please-config.json",
    ".release-please-manifest.json",
    "wrangler/wrangler.toml",
})

# Image-only inputs: the container selector owns them, and the check job's
# offline manifest step still runs ungated, so no test job needs them.
IMAGE_ONLY_EXACT = frozenset({
    "Dockerfile",
    "Dockerfile.distroless",
    ".dockerignore",
})

# deploy/ is image-safe but its readers are unverified: fail closed.
FAIL_CLOSED_PREFIXES = (
    "deploy/",
)


def normalize(path):
    """Repo-relative path with ./ and leading / stripped (see container-inputs)."""
    path = path.strip()
    while path.startswith("./"):
        path = path[2:]
    return path.lstrip("/")


def classify(path):
    """Jobs affected by one repo-relative path (fail-closed: unknown runs all)."""
    path = normalize(path)
    if path in ALL_EXACT:
        return ALL_JOBS
    if path.startswith(RUST_WORKER_PREFIXES):
        jobs = {RUST, WORKER}
        if path == "crates/core/tests/fixtures/legacy_registry.json":
            jobs.add(PARITY)
        return frozenset(jobs)
    if path.startswith(VOICE_TEMPLATES_PREFIX):
        # Asset-only fast pass: coverage/test/readme assets are validated by
        # the check job's hermetic offline step, which always runs. Only the
        # corpus (include_str! in Rust tests) and the validator itself can
        # break the Rust matrix.
        if path in VOICE_TEMPLATE_RUST_INPUTS:
            return frozenset({RUST})
        return NO_JOBS
    if path.startswith(RUST_PREFIXES) or path.startswith(TESTS_PREFIX):
        return frozenset({RUST})
    if path.startswith(SCRIPTS_PREFIX):
        jobs = {RUST, WORKER}
        if PARITY_SCRIPT_MARKER in Path(path).name:
            jobs.add(PARITY)
        return frozenset(jobs)
    if path.startswith(WORKER_PREFIX):
        return frozenset({WORKER})
    if path in RUST_DOCS:
        jobs = {RUST}
        if path in PARITY_DOCS:
            jobs.add(PARITY)
        return frozenset(jobs)
    if path in PARITY_DOCS or path in WORKER_DOCS:
        jobs = set()
        if path in PARITY_DOCS:
            jobs.add(PARITY)
        if path in WORKER_DOCS:
            jobs.add(WORKER)
        return frozenset(jobs)
    if path.startswith(DOCS_PREFIX):
        # No file reader in PR-run code (proven by the reader-coverage
        # test), but runbook.test.ts asserts on the docs/ listing itself.
        return frozenset({WORKER})
    if path.startswith(WORKFLOWS_PREFIX):
        return frozenset({WORKER})
    if path in SKIP_EXACT or path in IMAGE_ONLY_EXACT:
        return NO_JOBS
    if path in WORKER_EXACT:
        return frozenset({WORKER})
    if path.startswith(FAIL_CLOSED_PREFIXES):
        return ALL_JOBS
    # Markdown outside docs//.github/ is never compiled or asserted on.
    if Path(path).suffix.lower() == ".md" and not path.startswith(".github/"):
        return NO_JOBS
    # Fail-closed: unrecognized paths (e.g. a brand-new top-level directory)
    # run everything rather than risk silently skipped coverage.
    return ALL_JOBS


def selection(changed, deleted=()):
    """Map of job -> bool for changed repo-relative paths.

    ``deleted`` paths force every job: existence gates (the preconditions
    REQUIRED_DOCS list, the worker docs/-listing assertion) cannot tell a
    modification from a deletion, so any deletion revalidates everything.
    """
    jobs = {job: False for job in (RUST, WORKER, PARITY)}
    if any(path.strip() for path in deleted):
        return {job: True for job in jobs}
    for path in changed:
        for job in classify(path):
            jobs[job] = True
    return jobs


def git_diff_names(base_ref, head_ref, root=ROOT, diff_filter=None):
    """Repo-relative paths changed between two refs (two-dot diff).

    ``--no-renames`` reports a rename as a delete plus an add: the
    deletion rule in :func:`selection` then forces full jobs. Without it a
    rename like ``src/x.rs`` -> ``docs/y.md`` would classify as a docs-only
    change while Rust code was deleted.
    """
    command = ["git", "-C", str(root), "diff", "--name-only", "--no-renames"]
    if diff_filter is not None:
        command.append(f"--diff-filter={diff_filter}")
    command.append(f"{base_ref}..{head_ref}")
    output = subprocess.run(command, capture_output=True, text=True, check=True)
    return [line for line in output.stdout.splitlines() if line.strip()]


def changed_files(base_ref, head_ref, root=ROOT):
    """Repo-relative paths changed between two refs (two-dot diff)."""
    return git_diff_names(base_ref, head_ref, root)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--head-ref", required=True)
    parser.add_argument("--job", choices=(RUST, WORKER, PARITY), default=None,
                        help="print only this job's selection (default: all)")
    args = parser.parse_args(argv)
    try:
        changed = git_diff_names(args.base_ref, args.head_ref,
                                 diff_filter="ACMRTUXB")
        deleted = git_diff_names(args.base_ref, args.head_ref,
                                 diff_filter="D")
    except subprocess.CalledProcessError as exc:
        # Undecidable diff (shallow clone, missing ref): fail closed.
        print(f"job-inputs: git diff failed ({exc.returncode}); "
              "selecting full jobs", flush=True)
        return 2
    jobs = selection(changed, deleted)
    if args.job is not None:
        print("true" if jobs[args.job] else "false")
    else:
        for job in (RUST, WORKER, PARITY):
            print(f"{job}={'true' if jobs[job] else 'false'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
