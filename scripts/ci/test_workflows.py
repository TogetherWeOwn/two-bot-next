"""Offline workflow policy fixtures; never dispatch, deploy or probe staging."""

from copy import deepcopy
import importlib.util
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch

import yaml

ROOT = Path(__file__).resolve().parents[2]
_HISTORY_SPEC = importlib.util.spec_from_file_location(
    "synthetic_alert_history", ROOT / "scripts/synthetic_alert_history.py")
_HISTORY_MODULE = importlib.util.module_from_spec(_HISTORY_SPEC)
_HISTORY_SPEC.loader.exec_module(_HISTORY_MODULE)
_TRACKER_SPEC = importlib.util.spec_from_file_location(
    "slow_burn_tracker", ROOT / "scripts/slow_burn_tracker.py")
_TRACKER_MODULE = importlib.util.module_from_spec(_TRACKER_SPEC)
_TRACKER_SPEC.loader.exec_module(_TRACKER_MODULE)
ALERT_STEPS = _HISTORY_MODULE.ALERT_STEPS
DECIDE_STEPS = _HISTORY_MODULE.DECIDE_STEPS
JOB_NAMES = _HISTORY_MODULE.JOB_NAMES
PROBE_STEPS = _HISTORY_MODULE.PROBE_STEPS
STATIC_FALSE = "${{ false }}"
JOB_INVENTORY = {
    # The nextest branch adds its required lane; main adds the production schema gate.
    # The inventory pin must retain both changes.
    "check.yml": {"check", "moderation-db", "parity-docs", "self-role-store", "job-inputs", "container-inputs", "container",
                  "community-db", "feeds-db", "tickets-postgres", "worker", "supply-chain", "ci-ok",
                  # `check` is now the lint lane; the test steps it used to
                  # carry run in these parallel lanes, all gated by ci-ok.
                  # The HARD patch-coverage measurement is sharded: the plan
                  # selects the legs, the shard matrix runs them, and
                  # `patch-coverage` merges and gates, so the unit-test signal
                  # and the coverage verdict fail independently.
                  "rust-tests", "coverage-plan", "coverage-shard",
                  "patch-coverage", "ignored-db-stores", "ignored-db-runtime",
                  # Compile-only libFuzzer build, gated by ci-ok like the Rust lanes.
                  "fuzz-compile",
                  # Promoted nextest lane (shared reusable), gated by ci-ok
                  # like the Rust lanes.
                  "nextest"},
    "deploy-production.yml": {"guard", "schema-gate", "roles-gate", "soak", "staging-soak-evidence", "recheck",
                              "production"},
    "deploy-staging.yml": {"deploy", "schema-gate", "roles-gate", "soak-hold", "voice-synthetic", "user-flow-synthetic",
                           "pinned-history-guard", "certify-promotion"},
    "nightly.yml": {"changes", "pipeline-benchmark", "advisories", "sweep"},
    "pipeline-benchmark.yml": {"benchmark"},
    "release.yml": {"sbom-target", "release-sbom", "attach-sbom", "verdict"},
    # Daily digest release (docs/releases.md): at most one vYYYY.MDD.P tag a day,
    # plus the missing-digest alert; pinned shape below.
    "daily-digest.yml": {"digest", "alert", "rollback-proof-watch"},
    "staging-migrate.yml": {"plan", "claim", "apply"},
    "production-migrate.yml": {"plan", "claim", "apply"},
    # Daily staging automatic recovery proof plus the manual Worker-only drill.
    "staging-rollback-drill.yml": {"drill", "proof", "alert"},
    # Container-image backout/restore drill: manual, staging-only, pinned shape below.
    "staging-container-drill.yml": {"container-drill"},
    # Manual read-only staging events read for the B2 soak; pinned shape below.
    "staging-events-read.yml": {"read"},
    # Manual read-only staging privacy verification read for one fixture
    # pair (six tables, guild alias); pinned shape below.
    "staging-privacy-verify-read.yml": {"read"},
    # Manual read-only staging event-store retention census (counts, digests
    # and timestamps only, log only); pinned shape below.
    "staging-retention-census.yml": {"census"},
    # Manual read-only staging fixture-scope census (counts, digests and one
    # observation timestamp only, log only); pinned shape below.
    "staging-fixture-census.yml": {"fixture-census"},
    # Manual read-only authenticated MCP prove-out (TOG-23456); pinned shape below.
    "staging-mcp-proveout.yml": {"proveout"},
    # Manual read-only staging ownership status read; pinned shape below.
    "staging-ownership-status.yml": {"status"},
    # Manual gradual-promote proof caller (passive until the synthetic
    # fixture lands); pinned shape below.
    "gradual-promote-proof.yml": {"proof"},
    "supply-chain.yml": {"pr-lint", "gitleaks"},
    # TOG-10893: read-only SBOM inventory/gates shared by the PR dry-run and releases.
    # `image` builds/scans the untrusted ref with pinned actions only; `verify`
    # runs the local validation/evidence/preflight scripts without ever
    # checking out inputs.ref (CodeQL cache-poisoning gate).
    "sbom.yml": {"image", "verify"},
    # Live staging voice synthetic (docs/voice-synthetic.md); pinned shape below.
    "voice-synthetic.yml": {"synthetic"},
    # Five-minute user-flow synthetic (docs/synthetic-user-flow.md): staging
    # health-contract plus passive production probes; pinned shape below.
    "synthetic-user-flow.yml": {"staging", "prod-passive"},
    # Creator-join slow-burn card filer
    # (docs/runbook.md#alert-voice-join-slow-burn): live staging `tracker`
    # plus the environment-free `fixture` proof job; pinned shape below.
    "slow-burn-tracker.yml": {"tracker", "fixture"},
    # SDLC separation gate (org test-guard reusable, warn mode): one
    # reusable call, least-privilege grant pinned below.
    "test-guard.yml": {"test-guard"},
    # Advisory changed-file review rules; no deploy route or new required gate.
    "review-rules.yml": {"review-rules"},
    # TOG-21027: manual, bounded vote_kick libFuzzer smoke on a GitHub-hosted
    # runner. Read-only, no secrets, no deploy route: the generic
    # deployment/probe scan already passes, so only the inventory pin is needed.
    "vote-kick-fuzz-smoke.yml": {"vote-kick-smoke"},
    # Manual, bounded Helix token parser fuzz smoke; read-only and secret-free.
    "helix-token-fuzz-smoke.yml": {"helix-token-smoke"},
    # SDLC 2.3 CI speed evidence (the shared nextest lane graduated to a
    # required check.yml job): dorny/paths-filter affected-only selection
    # and the minutes-per-merged-PR report.
    "ci-speed.yml": {"changes", "report"},
    # Shared nextest lane, called by check.yml; org-level home is
    # TogetherWeOwn/.github with a pinned SHA like test-guard.
    "reusable-rust-nextest.yml": {"nextest"},
    # Advisory PR-size labels (org reusable, advisory only, never required):
    # thin caller to the central labeller + weekly tally.
    "pr-size.yml": {"pr-size", "pr-size-weekly"},
    # Weekly DORA delivery metrics (docs/dora-metrics.md): reads 30 days of
    # production Deployments and commits one JSON file plus a markdown
    # summary to ops/metrics/ on the automation branch with the job token.
    "dora-metrics.yml": {"metrics"},
}
# Reusable-workflow calls are allowed only to these non-deploy workflows.
# TOG-10893 registers the read-only sbom.yml calls alongside the benchmark one.
REUSABLE_CALLS = {("nightly.yml", "pipeline-benchmark"): "./.github/workflows/pipeline-benchmark.yml",
                   ("check.yml", "supply-chain"): "./.github/workflows/sbom.yml",
                   ("release.yml", "release-sbom"): "./.github/workflows/sbom.yml",
                   # After every staging deploy; its failure fails deploy-staging (promotion gate).
                   ("deploy-staging.yml", "voice-synthetic"): "./.github/workflows/voice-synthetic.yml",
                   # SDLC separation gate: org reusable, warn mode, pinned SHA.
                   ("test-guard.yml", "test-guard"): "TogetherWeOwn/.github/.github/workflows/test-guard.yml@3fffeb9f9465f371763fcc3c789ed4baefe38f95",
                   # Read-only shadow engine, pinned to the published central merge commit.
                   ("review-rules.yml", "review-rules"): "TogetherWeOwn/.github/.github/workflows/review-rules.yml@63699075b492914c137281acffd4c89e51cc1c4c",
                   # SDLC 2.3 CI speed pilot, promoted: required check.yml
                   # caller to the shared nextest lane (local path until
                   # the central move).
                   ("check.yml", "nextest"): "./.github/workflows/reusable-rust-nextest.yml",
                   # Advisory PR-size labels: thin caller to the central
                   # reusable, pinned SHA (repin both refs together).
                   ("pr-size.yml", "pr-size"): "TogetherWeOwn/.github/.github/workflows/pr-size.yml@82c5b367c6e3e8032666029dc22d206fff39c3f2",
                   ("pr-size.yml", "pr-size-weekly"): "TogetherWeOwn/.github/.github/workflows/pr-size-weekly.yml@82c5b367c6e3e8032666029dc22d206fff39c3f2"}
# Main's runner routing (#265, 2026-10-02): the repo is public and the org's
# self-hosted runner group refuses public repos, so every job routes through
# one expression — public repo -> GitHub-hosted, private -> CI_OVERFLOW_* switch
# for this named job, then the self-hosted pool. Mirrors scripts/test-runner-routing.py.
ROUTED_RUNNER = (
    "${{{{ fromJSON((!github.event.repository.private && '[\"ubuntu-latest\"]') || "
    "(contains(fromJSON(vars.CI_OVERFLOW_JOBS || '[]'), '{job}') && "
    "contains(fromJSON(vars.CI_OVERFLOW_EVENTS || '[]'), github.event_name) && "
    "vars.CI_OVERFLOW_RUNNER) || '[\"self-hosted\",\"two-selfhosted\"]') }}}}"
)
DEPLOY_MARKERS = (
    "cloudflare/wrangler-action", "wrangler deploy", "wrangler publish",
    "cloudflare_api_token", "staging_worker_url", "production_worker_url", "/health", "/readyz",
)


class UniqueKeyLoader(yaml.BaseLoader):
    def construct_mapping(self, node, deep=False):
        mapping = {}
        for key_node, value_node in node.value:
            key = self.construct_object(key_node, deep=deep)
            if key in mapping:
                raise ValueError(f"duplicate workflow key: {key}")
            mapping[key] = self.construct_object(value_node, deep=deep)
        return mapping


def load_workflows():
    return {path.name: yaml.load(path.read_text(), Loader=UniqueKeyLoader)
            for path in (ROOT / ".github/workflows").glob("*.y*ml")}


def runner_allowed(job_id, runs_on):
    # The routing expression must name this job, so one job's switch cannot move another.
    return runs_on == ROUTED_RUNNER.format(job=job_id)


def shell_logical_lines(script):
    """Join backslash continuations so each shell command is one line."""
    lines, pending = [], ""
    for raw_line in script.splitlines():
        stripped = raw_line.strip()
        pending = f"{pending} {stripped}".strip() if pending else stripped
        if stripped.endswith("\\"):
            pending = pending[:-1].strip()
            continue
        lines.append(pending)
        pending = ""
    if pending:
        lines.append(pending)
    return lines


# An `#[ignore ...]` line, further attributes or trivia, then the fn it marks.
# A single regex with a block-comment alternative inside `(?:...)*` backtracks
# exponentially (CodeQL `py/polynomial-regular-expression-used-on-uncontrolled-data`);
# scan forward with string ops instead so each byte is visited once.
IGNORE_MARK = re.compile(r"^[ \t]*#\[ignore\b", re.MULTILINE)


def _skip_trivia(text, pos):
    """Skip whitespace, line/block comments and `#[...]` lines from `pos`."""
    end = len(text)
    while pos < end:
        char = text[pos]
        if char.isspace():
            pos += 1
            continue
        if text.startswith("//", pos):
            newline = text.find("\n", pos + 2)
            if newline == -1:
                return end
            pos = newline + 1
            continue
        if text.startswith("/*", pos):
            close = text.find("*/", pos + 2)
            if close == -1:
                return end
            pos = close + 2
            continue
        if text.startswith("#[", pos):
            newline = text.find("\n", pos)
            if newline == -1:
                return end
            pos = newline + 1
            continue
        return pos
    return pos


def _match_fn_name(text, pos):
    """The fn name at `pos`, allowing an `async` prefix on the same line."""
    end = len(text)
    if text.startswith("async", pos) and pos + 5 < end and text[pos + 5] in " \t":
        pos += 5
        while pos < end and text[pos] in " \t":
            pos += 1
    if not text.startswith("fn", pos):
        return None
    pos += 2
    if pos >= end or text[pos] not in " \t":
        return None
    while pos < end and text[pos] in " \t":
        pos += 1
    if pos >= end or not (text[pos].isalpha() or text[pos] == "_"):
        return None
    start = pos
    pos += 1
    while pos < end and (text[pos].isalnum() or text[pos] == "_"):
        pos += 1
    return text[start:pos]


def ignored_test_names(suite):
    """Find ignored test fns, refusing any ignore mark the scanner cannot map."""
    [path] = ROOT.glob(f"crates/*/tests/{suite}.rs")
    text = path.read_text(encoding="utf-8")
    marks = list(IGNORE_MARK.finditer(text))
    names = []
    for mark in marks:
        line_end = text.find("\n", mark.end())
        if line_end == -1:
            continue
        name = _match_fn_name(text, _skip_trivia(text, line_end + 1))
        if name is not None:
            names.append(name)
    # This is a source-style scanner, not a Rust parser. Unsupported syntax
    # must fail policy validation, never silently shrink the sweep's skip set.
    if len(names) != len(marks):
        raise ValueError(
            f"{path.name}: discovered {len(names)} of {len(marks)} ignored test functions; "
            "extend the scanner for the unsupported declaration",
        )
    return names


def tokio_test_names(text):
    """Collect each Tokio test through intervening trivia, or fail closed.

    Like ignored_test_names this is a source-style scanner, not a Rust parser.
    Every attribute mark must resolve, and every `tokio::test` token must be
    consumed by exactly one mark; imported, renamed, cfg_attr or
    macro-generated spellings the mark regex cannot see fail validation
    instead of silently shrinking the skip set while other ordinary
    declarations still match.
    """
    marks = list(re.finditer(r"^[ \t]*#\[(?:::)?tokio::test\b", text, re.MULTILINE))
    tokens = re.findall(r"(?<!\w)tokio::test\b", text)
    names = []
    for mark in marks:
        line_end = text.find("\n", mark.end())
        name = None if line_end == -1 else _match_fn_name(text, _skip_trivia(text, line_end + 1))
        if name is not None:
            names.append(name)
    if (
        not marks
        or len(tokens) != len(marks)
        or len(names) != len(marks)
        or len(set(names)) != len(names)
    ):
        raise ValueError(
            f"found {len(tokens)} tokio::test tokens and {len(marks)} marks "
            f"for {len(names)} unique Tokio test functions; "
            "extend the scanner for the unsupported declaration",
        )
    # The token census cannot see crate aliases or grouped/glob imports.
    # This fixture paths Tokio fully: reject any use/extern item mentioning
    # tokio, including visibility-qualified and nested use trees. Scan from
    # the keyword, not the start of a line. Opaque import syntax also refuses
    # validation: a comment/literal semicolon must not truncate the item and
    # hide a Tokio rename. Like the census above this deliberately scans raw
    # source and may reject prose; extend the scanner rather than waive it.
    for keyword in re.finditer(r"(?<![\w$])(?:use|extern)\b", text):
        stop = text.find(";", keyword.end())
        item = text[keyword.end():] if stop == -1 else text[keyword.end():stop]
        if re.search(r"\btokio\b|[/\"']", item):
            raise ValueError(
                "unsupported Tokio import or opaque import syntax; "
                "path Tokio fully so every test attribute stays visible",
            )
    # Supported macro source is deliberately small: no local definitions
    # (including nested/raw-identifier forms), macro imports or opaque item/
    # source generators. Only these bare standard expression macros are used
    # by the fixture. Do not try to expand arbitrary Rust with this scanner.
    # Scan raw words so trivia, metavariables and repetitions cannot hide a
    # definition; prose/literals may conservatively refuse validation too.
    expression_macros = r"assert|assert_eq|assert_ne|eprintln|format|vec"
    macro_error = (
        "unsupported macro source; keep fixtures as direct Tokio test functions "
        "and use only bare standard expression macros, or extend the scanner"
    )
    if re.search(r"\b(?:macro_rules|macro|macro_use)\b", text):
        raise ValueError(macro_error)
    for keyword in re.finditer(r"\buse\b", text):
        stop = text.find(";", keyword.end())
        item = text[keyword.end():] if stop == -1 else text[keyword.end():stop]
        # A glob or renamed import could shadow an allowed expression macro.
        if re.search(rf"\b(?:{expression_macros})\b|\*", item):
            raise ValueError(macro_error)
    for bang in re.finditer(r"!\s*(?:[({\[]|/\*|//)", text):
        prefix = text[:bang.start()]
        if prefix.rsplit("\n", 1)[-1].lstrip().startswith("//"):
            continue  # Inner doc comments (`//!`) are not macro invocations.
        name = re.search(rf"\b(?:{expression_macros})\s*$", prefix)
        before = "" if name is None else prefix[:name.start()].rstrip()
        # Require a bare name after an expression/statement separator. Refuse
        # qualified/raw/Unicode-suffixed names and comment-obscured paths or
        # delimiters rather than normalizing them into an allowed spelling.
        # include! and imported item macros are opaque even without Tokio text.
        if (name is None or (before and before[-1] not in "([{=,;}")
                or "//" in before.rsplit("\n", 1)[-1]
                or bang.group().rstrip().endswith(("/*", "//"))):
            raise ValueError(macro_error)
    return names


def skip_blocks_test(skip, name):
    """Whether a cargo `--skip` token blocks a bare test fn `name`.

    Cargo matches `--skip` as a substring of the full test path (for
    example `database::my_test`), but the sweep policy only knows the
    bare fn name. A module-qualified token never occurs in the bare
    name, so compare its last `::` segment instead. This over-blocks
    when the module part would not match, which fails closed.
    """
    return skip.rsplit("::", 1)[-1] in name


# Quarantined red backfill acceptance (TOG-34944): 15 ignored tests that run
# in NO lane — skipped in the check.yml coverage sweep, the check.yml store
# lane and the nightly sweep — because their builders have not landed yet.
# Each entry maps the test fn to the build card tracking it. Shrink-only: a
# builder PR removes the "quarantined red" marker (keeping the DB ignore),
# un-skips its family in all three lanes AND ratchets this pin down in the
# same PR. Growth fails closed; see backfill_quarantine_errors.
BACKFILL_QUARANTINE = {
    # Audit-log family: TOG-22948 lands the fetcher + backfill_audit_entries.
    "audit_log_pagination_limit_1_to_100_before_after_covers_45d_window": "TOG-22948",
    "audit_log_requests_ride_rate_limit_client_never_direct_rest": "TOG-22948",
    "audit_cursor_persists_per_window_and_resumes_mid_window": "TOG-22948",
    "audit_rows_hold_ids_timestamps_enums_only": "TOG-22948",
    "audit_429_retry_after_pauses_without_losing_place": "TOG-22948",
    # Event-users/reactions/pins family: TOG-22951 lands the three fetchers.
    "event_users_paginate_through_rate_limit_client_with_max_100": "TOG-22951",
    "reaction_users_paginate_through_rate_limit_client": "TOG-22951",
    "pins_list_through_rate_limit_client_persists_metadata_only": "TOG-22951",
    "blocklisted_users_skipped_for_per_user_rows_while_aggregates_keep_counting": "TOG-22951",
    "persisted_rows_hold_ids_timestamps_enums_only": "TOG-22951",
    "resume_continues_from_persisted_cursors_per_family": "TOG-22951",
    # Bans/invites/onboarding family: TOG-22952 lands the family fetchers.
    "bans_fetch_applies_blocklist_and_counts_everyone": "TOG-22952",
    "invite_rows_hold_metadata_only": "TOG-22952",
    "onboarding_rows_hold_ids_enums_metadata_only": "TOG-22952",
    "bans_invites_onboarding_resume_continues_from_persisted_cursors": "TOG-22952",
}
BACKFILL_QUARANTINE_SUITES = (
    "backfill_audit_log",
    "backfill_bans_invites_onboarding",
    "backfill_event_users_reactions_pins",
)


def ignored_test_reasons(suite):
    """Map ignored test fns to their ignore-mark source line.

    Same discovery as ignored_test_names, but keeps the `#[ignore ...]`
    line so the quarantine tripwire can require a tracking ref in it.
    Unsupported syntax fails closed exactly like ignored_test_names.
    """
    [path] = ROOT.glob(f"crates/*/tests/{suite}.rs")
    text = path.read_text(encoding="utf-8")
    marks = list(IGNORE_MARK.finditer(text))
    reasons = {}
    for mark in marks:
        line_end = text.find("\n", mark.end())
        if line_end == -1:
            continue
        name = _match_fn_name(text, _skip_trivia(text, line_end + 1))
        if name is not None:
            line_start = text.rfind("\n", 0, mark.start()) + 1
            reasons[name] = text[line_start:line_end]
    if len(reasons) != len(marks):
        raise ValueError(
            f"{path.name}: discovered {len(reasons)} of {len(marks)} ignored test functions; "
            "extend the scanner for the unsupported declaration",
        )
    return reasons


def backfill_quarantine_members(reasons):
    """Select explicit quarantine markers, not passing tests' DB opt-ins."""
    return {name: reason for name, reason in reasons.items() if "quarantined red" in reason}


def backfill_quarantine_errors(pinned, current):
    """Shrink-only tripwire for the quarantined red backfill tests.

    `pinned` maps test fn -> tracking ref (BACKFILL_QUARANTINE); `current`
    maps test fn -> ignore-mark line, derived from the suite sources.
    Only reasons marked "quarantined red" are quarantine members. A fixed
    test can retain its plain DB ignore while leaving quarantine.
    Growth is blocked outright; shrinkage must ratchet the pin down in the
    same PR; every surviving member must carry its tracking ref in its
    ignore reason. Returns a list of error strings (empty when clean).
    """
    current = backfill_quarantine_members(current)
    errors = []
    for name in sorted(set(current) - set(pinned)):
        errors.append(
            f"{name}: quarantine grew; blocked: fix the test or quarantine it "
            "with a tracking ref in BACKFILL_QUARANTINE",
        )
    removed = sorted(set(pinned) - set(current))
    if removed:
        errors.append(
            "quarantine shrank (good): ratchet BACKFILL_QUARANTINE down to "
            f"{sorted(set(pinned) & set(current))} in this PR "
            f"(fixed: {', '.join(removed)})",
        )
    for name in sorted(set(pinned) & set(current)):
        if pinned[name] not in current[name]:
            errors.append(
                f"{name}: ignore reason must carry tracking ref {pinned[name]}",
            )
    return errors


# Staging deploy push filter. Each deploy restarts the bot container and drops the
# gateway for ~2 minutes, so a push that touches only non-runtime paths must not
# start one (the B2 soak needs a stable staging). The filter is a deny-list so an
# unclassified path still deploys; these samples pin both sides of it.
RUNTIME_PATHS = (
    "crates/bot/src/main.rs", "crates/core/Cargo.toml", "crates/core/README.md",
    "crates/core/tests/fixtures/reference_settings.json", "src/main.rs",
    "sql/web_v1.sql", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "deny.toml",
    "migrations.lock", "Dockerfile", ".dockerignore",
    "wrangler/wrangler.toml", "wrangler/src/index.ts", "wrangler/package.json",
    "wrangler/package-lock.json", "wrangler/scripts/ownership-control.mjs",
    "wrangler/README.md", "scripts/staging_rollout.py", "scripts/container-smoke.py",
    ".github/workflows/deploy-staging.yml", ".github/workflows/check.yml",
    ".github/scripts/anything.py", "deploy/two-bot.service", "tests/voice_templates/corpus.json",
    "release-please-config.json", ".release-please-manifest.json", "fuzz/Cargo.toml",
    "fuzz/Cargo.lock",
    "a-new-top-level-dir/file.txt", ".cargo/config.toml",
)
NON_RUNTIME_PATHS = (
    "docs/runbook.md", "docs/parity-baseline.json", "docs/adr/0001-example.md",
    "docs/soak-checklist.json", "README.md", "CHANGELOG.md", "AGENTS.md",
    "CONTRIBUTING.md", "LICENSE", ".editorconfig", ".gitignore", ".gitleaks.toml",
    ".gitleaksignore", ".github/ISSUE_TEMPLATE/bug_report.yml",
    ".github/pull_request_template.md", ".github/CODEOWNERS", ".github/dependabot.yml",
    "ops/metrics/dora-2026-W43.json", "ops/metrics/dora-2026-W43.md",
)


def filter_pattern_matches(pattern, path):
    """GitHub path-filter semantics: anchored at the root, `*` stops at `/`, `**` does not."""
    regex, i = "", 0
    while i < len(pattern):
        if pattern.startswith("**", i):
            regex, i = regex + ".*", i + 2
        elif pattern[i] == "*":
            regex, i = regex + "[^/]*", i + 1
        elif pattern[i] == "?":
            regex, i = regex + "[^/]", i + 1
        else:
            regex, i = regex + re.escape(pattern[i]), i + 1
    return re.fullmatch(regex, path) is not None


def staging_push_filter_errors(name, push):
    """The push trigger is main-only with a deny-list of non-runtime paths.

    GitHub skips a push run only when every changed path matches `paths-ignore`,
    so a runtime path matching ANY pattern would be skipped only when it ships
    alone. Pin that no pattern matches a runtime path, that the known non-runtime
    set stays skipped, and that an allow-list (`paths`) never replaces the
    deny-list (an unclassified path must still deploy).
    """
    if not isinstance(push, dict) or push.get("branches") != ["main"]:
        return [f"{name}: push trigger must be branches [main]"]
    errors = []
    if set(push) - {"branches", "paths-ignore"}:
        errors.append(f"{name}: push trigger may only carry branches and paths-ignore")
    patterns = push.get("paths-ignore") or []
    if not isinstance(patterns, list) or not all(isinstance(p, str) for p in patterns):
        return errors + [f"{name}: paths-ignore must be a list of patterns"]
    for path in RUNTIME_PATHS:
        hits = [p for p in patterns if filter_pattern_matches(p, path)]
        if hits:
            errors.append(f"{name}: paths-ignore {hits} would skip a runtime path {path}")
    for path in NON_RUNTIME_PATHS:
        if not any(filter_pattern_matches(p, path) for p in patterns):
            errors.append(f"{name}: paths-ignore no longer skips non-runtime path {path}")
    return errors


def staging_dispatch_errors(workflow):
    """The active staging workflow keeps exactly six fenced dispatch inputs.

    `release_fence` exists on main (TOG-11143) so post-handoff release of an
    uninitialized/parked singleton is explicit and auditable; it defaults to
    false and the deploy job itself runs unconditionally (TOG-12856).
    `inject_gateway_failure` is the staging rollback-drill switch: also an
    opt-in boolean defaulting to false, readable only through the pinned
    dispatch-only expression in staging_gateway_injection_errors.
    `inject_error_rate_spike` is the error-rate rollback-drill switch with
    the same shape, readable only through the pinned dispatch-only
    expression in staging_error_rate_gate_errors. `inject_voice_stall` and
    `inject_unpaired_disconnect` are the remediation-drill switches with the
    same shape, readable only through the pinned dispatch-only expressions
    in staging_fault_injection_errors. `sha` pins the deployed commit for
    arm/disarm at one exact revision: an optional string, empty by default
    (empty deploys the workflow head), readable only through the pinned
    dispatch-or-head expression in staging_pinned_sha_errors. Anything else
    in the dispatch block (an arming switch, a deploy flag, a default of
    true) is an unreviewed deploy route outside the pinned shape.
    """
    name = "deploy-staging.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"push", "workflow_dispatch"}:
        errors.append(f"{name}: triggers changed")
    errors.extend(staging_push_filter_errors(name, on.get("push")))
    dispatch = on.get("workflow_dispatch") or {}
    expected_booleans = {"release_fence", "inject_gateway_failure", "inject_error_rate_spike",
                         "inject_voice_stall", "inject_unpaired_disconnect"}
    expected = expected_booleans | {"sha"}
    if set(dispatch) != {"inputs"} or set(dispatch.get("inputs") or {}) != expected:
        errors.append(f"{name}: workflow_dispatch must carry only the release_fence, drill and sha inputs")
    else:
        for input_name in expected_booleans:
            spec = dispatch["inputs"][input_name] or {}
            if spec.get("type") != "boolean" or spec.get("default") not in (False, "false"):
                errors.append(f"{name}: {input_name} must be an opt-in boolean defaulting to false")
        sha = dispatch["inputs"]["sha"] or {}
        if (sha.get("type") != "string" or str(sha.get("required")).lower() != "false"
                or sha.get("default") != ""):
            errors.append(f"{name}: sha must be an optional string defaulting to empty "
                          "(empty deploys the workflow head)")
    return errors


INJECTION_EXPRESSION = "${{ github.event_name == 'workflow_dispatch' && inputs.inject_gateway_failure && 'true' || 'false' }}"
INJECTION_MARKERS = (
    "inject_gateway_failure", "INJECT_GATEWAY_FAILURE",
    "TWO_GATEWAY_READINESS_FAIL_INJECTION", "--inject-gateway-failure",
)
FAULT_EXPRESSIONS = {
    "INJECT_VOICE_STALL": "${{ github.event_name == 'workflow_dispatch' && inputs.inject_voice_stall && 'true' || 'false' }}",
    "INJECT_UNPAIRED_DISCONNECT": "${{ github.event_name == 'workflow_dispatch' && inputs.inject_unpaired_disconnect && 'true' || 'false' }}",
}
FAULT_MARKERS = (
    "inject_voice_stall", "INJECT_VOICE_STALL",
    "TWO_VOICE_STALL_FAULT_INJECTION", "--inject-voice-stall",
    "inject_unpaired_disconnect", "INJECT_UNPAIRED_DISCONNECT",
    "TWO_GATEWAY_UNPAIRED_FAULT_INJECTION", "--inject-unpaired-disconnect",
)
# Pinned-SHA arm/disarm: a dispatch names its exact commit, anything else
# deploys the workflow head. Every checkout ref and every SHA-step env in
# deploy-staging must use exactly this expression.
DEPLOY_SHA_EXPRESSION = "${{ github.event_name == 'workflow_dispatch' && inputs.sha || github.sha }}"
# Soak-hold bindings: the Operator-owned repo variable holding the ISO-8601
# UTC deadline, event name and dispatch-only rehearsal flag. Explicit arm/disarm
# dispatches bypass the hold; automatic error-gate proofs never do.
SOAK_HOLD_ENV = {"HOLD_UNTIL": "${{ vars.STAGING_SOAK_HOLD_UNTIL }}",
                 "EVENT": "${{ github.event_name }}",
                 "REHEARSAL": "${{ github.event_name == 'workflow_dispatch' && inputs.inject_error_rate_spike && 'true' || 'false' }}"}
REHEARSAL_RUN_NAME = "${{ github.event_name == 'workflow_dispatch' && inputs.inject_error_rate_spike && 'staging rollback proof' || github.workflow }}"


def staging_gateway_injection_errors(workflows):
    """The drill injection is dispatch-only, staging-only plumbing.

    Exactly one expression in exactly one staging step may read the input,
    it must also require `workflow_dispatch` (a push never carries inputs,
    and the guard makes that structural rather than implied), and the step
    may only hand the prepare flag to the generated staging deploy config.
    Every other workflow — deploy-production above all — must not mention
    the switch at all, so no route can forward it to production.
    """
    errors = []
    for name, workflow in workflows.items():
        if name == "deploy-staging.yml":
            continue
        text = str(workflow)
        for marker in INJECTION_MARKERS:
            if marker in text:
                errors.append(f"{name}: gateway-failure injection marker {marker} outside deploy-staging")
    staging = workflows.get("deploy-staging.yml") or {}
    text = str(staging)
    if text.count("inputs.inject_gateway_failure") != 1:
        errors.append("deploy-staging.yml: inject_gateway_failure must be read exactly once")
    steps = ((staging.get("jobs") or {}).get("deploy") or {}).get("steps") or []
    armed = [step for step in steps
             if isinstance(step, dict) and "INJECT_GATEWAY_FAILURE" in str(step)]
    if len(armed) != 1:
        errors.append("deploy-staging.yml: exactly one deploy step may carry the injection switch")
        return errors
    step = armed[0]
    env = step.get("env") or {}
    if env.get("INJECT_GATEWAY_FAILURE") != INJECTION_EXPRESSION:
        errors.append("deploy-staging.yml: injection must use the pinned dispatch-only expression")
    run = str(step.get("run") or "")
    for needle in ("staging_rollout.py prepare", "--inject-gateway-failure",
                   '[ "$INJECT_GATEWAY_FAILURE" = "true" ]'):
        if run.count(needle) != 1:
            errors.append(f"deploy-staging.yml: injection step must contain exactly one {needle}")
    return errors


ERROR_RATE_EXPRESSION = "${{ github.event_name == 'workflow_dispatch' && inputs.inject_error_rate_spike && 'true' || 'false' }}"
ERROR_RATE_MARKERS = (
    "inject_error_rate_spike", "INJECT_ERROR_RATE_SPIKE",
    "promotion-error-gate.mjs", "error-rate-gate.json",
)


def production_error_rate_gate_errors(workflow, name="deploy-production.yml"):
    """The production error-rate gate scrapes live production, never a rehearsal.

    Failure injection has no production input, guard binding or runtime path.
    An exact input allowlist also refuses newly named fault switches, not just
    the retired switch. No production step carries a drill switch or a
    rehearsal flag. The error_rate_gate step scrapes live production
    /ops/metrics and fails the stage so auto-rollback runs. The scrape token
    is the production binding only; the staging token must never appear.
    Reviewed tooling runs from the workflow commit (../tooling), never the
    guarded SHA.
    """
    errors = []
    inputs = (((workflow.get("on") or {}).get("workflow_dispatch") or {}).get("inputs") or {})
    if set(inputs) != {"sha", "rollback", "image", "allow_unstaged", "takeover", "cutover_hold"}:
        errors.append(f"{name}: production inputs must contain only the reviewed deploy/restore controls")
    jobs = workflow.get("jobs", {}) or {}
    steps = ((jobs.get("production") or {}).get("steps") or [])
    for marker in ("inject_error_rate_spike", "inject_gateway_failure", "--rehearsal-firing",
                   "rehearsal", "fault_injection", "fail_injection"):
        if marker in str(workflow).lower():
            errors.append(f"{name}: workflow must never carry the drill marker {marker}")
    gates = [step for step in steps if isinstance(step, dict) and step.get("id") == "error_rate_gate"]
    if len(gates) != 1:
        errors.append(f"{name}: exactly one production step must be the error_rate_gate")
        return errors
    step = gates[0]
    # After a rollback restore there is no snapshot to roll back to: the
    # gate must not fail the recovery run red with no action possible.
    if step.get("if") != "env.MODE == 'deploy'":
        errors.append(f"{name}: error_rate_gate must run in deploy mode only")
    env = step.get("env") or {}
    if env.get("METRICS_SCRAPE_TOKEN") != "${{ secrets.PRODUCTION_METRICS_SCRAPE_TOKEN }}":
        errors.append(f"{name}: error_rate_gate must bind the production scrape token by name")
    if env.get("PRODUCTION_URL") != "${{ vars.PRODUCTION_WORKER_URL }}":
        errors.append(f"{name}: error_rate_gate must read the production Worker URL from vars")
    run = str(step.get("run") or "")
    for needle in ("../tooling/wrangler/scripts/promotion-error-gate.mjs", "--metrics-url", "--evidence",
                   "--sha", "--user-agent"):
        if run.count(needle) != 1:
            errors.append(f"{name}: error_rate_gate step must contain exactly one {needle}")
    # Recovery is in this same job: no dispatch, reusable job or reviewer hop.
    by_id = {item.get("id"): item for item in steps}
    snapshot = by_id.get("snapshot", {})
    deploy = by_id.get("deploy", {})
    rollback = by_id.get("auto_rollback", {})
    gate = by_id.get("gate", {})
    if not all((snapshot, deploy, rollback, gate)):
        errors.append(f"{name}: production must retain snapshot, deploy, gate and auto_rollback")
    else:
        if not (steps.index(snapshot) < steps.index(deploy) < steps.index(gate)
                < steps.index(step) < steps.index(rollback)):
            errors.append(f"{name}: snapshot must precede deploy and both gates must precede rollback")
    if rollback.get("if") != ("failure() && env.MODE == 'deploy' && env.TAKEOVER == 'true' "
                              "&& steps.deploy.outcome == 'success' && steps.snapshot.outcome == 'success'"):
        errors.append(f"{name}: auto_rollback must retain the failure/deploy/takeover/snapshot chain")
    restore = str(rollback.get("run") or "")
    for required in ("../tooling/scripts/worker_container_rollback.py rollback",
                     "--variant production", '--snapshot "$RUNNER_TEMP/production-rollback-snapshot.json"',
                     '--evidence "$RUNNER_TEMP/production-auto-rollback-evidence.json"'):
        if required not in restore:
            errors.append(f"{name}: auto_rollback must restore the pre-deploy receipt: {required}")
    if ("uses" in rollback or "workflow run" in restore or "dispatch" in restore
            or "worker_container_rollback.py snapshot" not in str(snapshot.get("run") or "")):
        errors.append(f"{name}: auto_rollback must restore the local snapshot without dispatch")
    if "continue-on-error" in (jobs.get("production") or {}) or any(
            "continue-on-error" in item for item in (snapshot, deploy, gate, step, rollback)):
        errors.append(f"{name}: failure must stay red after recovery; never continue-on-error")
    # The evidence filename appears exactly twice: the --evidence write path
    # and the read-back print, which must name the same file.
    if run.count("production-error-rate-gate.json") != 2:
        errors.append(f"{name}: error_rate_gate must write and read back the same production-error-rate-gate.json")
    if "STAGING_METRICS_SCRAPE_TOKEN" in str(step):
        errors.append(f"{name}: error_rate_gate must never read the staging scrape token")
    return errors


def staging_error_rate_gate_errors(workflows):
    """The error-rate promotion gate is dispatch-gated rehearsal plus live plumbing.

    The input has exactly three pinned dispatch-only readers: run identity,
    soak hold, and error-rate gate. The first two distinguish expected-red
    proofs and prevent interrupting an observation window; only the gate
    judges the rehearsal firing. It scrapes live staging /ops/metrics otherwise,
    failing the stage so auto-rollback runs. The scrape token is optional
    (absent means a recorded skip, pinned in the gate script's tests); the
    rehearsal path needs no secret. No production input/binding may carry it.
    """
    errors = []
    for name, workflow in workflows.items():
        if name in ("deploy-staging.yml", "deploy-production.yml"):
            continue
        text = str(workflow)
        for marker in ERROR_RATE_MARKERS:
            if marker in text:
                errors.append(f"{name}: error-rate gate marker {marker} outside deploy-staging/deploy-production")
    staging = workflows.get("deploy-staging.yml") or {}
    text = str(staging)
    if text.count("inputs.inject_error_rate_spike") != 3:
        errors.append("deploy-staging.yml: rehearsal input belongs only to its identity, hold and error gate")
    if staging.get("run-name") != REHEARSAL_RUN_NAME:
        errors.append("deploy-staging.yml: rehearsal identity must be pinned to the dispatch-only switch")
    hold = ((staging.get("jobs") or {}).get("soak-hold") or {}).get("steps") or []
    if len(hold) != 1 or hold[0].get("env") != SOAK_HOLD_ENV:
        errors.append("deploy-staging.yml: rehearsal must honor the soak hold")
    steps = ((staging.get("jobs") or {}).get("deploy") or {}).get("steps") or []
    armed = [step for step in steps
             if isinstance(step, dict) and "INJECT_ERROR_RATE_SPIKE" in str(step)]
    if len(armed) != 1:
        errors.append("deploy-staging.yml: exactly one deploy step may carry the error-rate drill switch")
        return errors
    step = armed[0]
    if step.get("id") != "error_rate_gate":
        errors.append("deploy-staging.yml: the error-rate drill switch must live on the error_rate_gate step")
    env = step.get("env") or {}
    if env.get("INJECT_ERROR_RATE_SPIKE") != ERROR_RATE_EXPRESSION:
        errors.append("deploy-staging.yml: error-rate rehearsal must use the pinned dispatch-only expression")
    if "METRICS_SCRAPE_TOKEN" not in env:
        errors.append("deploy-staging.yml: error_rate_gate must bind the optional scrape token by name")
    run = str(step.get("run") or "")
    for needle in ("scripts/promotion-error-gate.mjs", "--metrics-url", "--evidence",
                   "--rehearsal-firing",
                   '[ "$INJECT_ERROR_RATE_SPIKE" = "true" ]'):
        if run.count(needle) != 1:
            errors.append(f"deploy-staging.yml: error_rate_gate step must contain exactly one {needle}")
    # The evidence filename appears exactly twice: the --evidence write path
    # and the read-back print, which must name the same file.
    if run.count("error-rate-gate.json") != 2:
        errors.append("deploy-staging.yml: error_rate_gate must write and read back the same error-rate-gate.json")
    return errors


def staging_fault_injection_errors(workflows):
    """The remediation-drill switches are dispatch-only, staging-only plumbing.

    Same shape as the gateway-failure injection above: each switch is read by
    exactly one pinned dispatch-only expression in exactly one staging step,
    the step hands only the prepare flags to the generated staging deploy
    config, and every other workflow — deploy-production above all — must not
    mention any fault marker at all, so no route can forward a drill to
    production.
    """
    errors = []
    for name, workflow in workflows.items():
        if name == "deploy-staging.yml":
            continue
        text = str(workflow)
        for marker in FAULT_MARKERS:
            if marker in text:
                errors.append(f"{name}: remediation-fault marker {marker} outside deploy-staging")
    staging = workflows.get("deploy-staging.yml") or {}
    text = str(staging)
    for input_name in ("inject_voice_stall", "inject_unpaired_disconnect"):
        if text.count(f"inputs.{input_name}") != 1:
            errors.append(f"deploy-staging.yml: {input_name} must be read exactly once")
    steps = ((staging.get("jobs") or {}).get("deploy") or {}).get("steps") or []
    armed = [step for step in steps
             if isinstance(step, dict) and ("INJECT_VOICE_STALL" in str(step)
                                            or "INJECT_UNPAIRED_DISCONNECT" in str(step))]
    if len(armed) != 1:
        errors.append("deploy-staging.yml: exactly one deploy step may carry the fault switches")
        return errors
    step = armed[0]
    env = step.get("env") or {}
    for var, expression in FAULT_EXPRESSIONS.items():
        if env.get(var) != expression:
            errors.append(f"deploy-staging.yml: {var} must use the pinned dispatch-only expression")
    run = str(step.get("run") or "")
    for needle in ("--inject-voice-stall", "--inject-unpaired-disconnect",
                   '[ "$INJECT_VOICE_STALL" = "true" ]',
                   '[ "$INJECT_UNPAIRED_DISCONNECT" = "true" ]'):
        if run.count(needle) != 1:
            errors.append(f"deploy-staging.yml: fault step must contain exactly one {needle}")
    return errors


def staging_pinned_sha_errors(workflows):
    """The pinned-SHA arm/disarm is dispatch-only, exact and ancestor-checked.

    A dispatch may name its exact commit; a push (or an empty dispatch) always
    deploys the workflow head. All three jobs check out the pinned
    dispatch-or-head ref at full depth, and each carries one SHA step that
    resolves the same expression through the environment, proves HEAD equals
    it, enforces a full 40-hex shape and refuses anything that is not an
    ancestor of origin/main. Each SHA step refuses exactly four ways (shape,
    length, checkout equality, ancestry), pinned by exit count so no refusal
    can go green. Exactly nine reads exist (three checkout refs, three
    SHA-step envs, the schema gate's --source-sha env, the user-flow
    synthetic's expected SHA, the history guard's comparison env): eight use
    the pinned expression and the ninth is the guard's raw pin-vs-head
    comparison, so no other step can resolve or judge a different commit. Build identity and provenance then follow the
    validated SHA through the deploy job's DEPLOY_SHA export, pinned by
    staging_deploy_identity_errors. Every other workflow must not mention the
    switch at all, except deploy-production, which carries its own reviewed
    sha input (the production guard's verified SHA) pinned by its own checks.
    """
    errors = []
    for name, workflow in workflows.items():
        if name in ("deploy-staging.yml", "deploy-production.yml"):
            continue
        if "inputs.sha" in str(workflow):
            errors.append(f"{name}: pinned-sha marker inputs.sha outside the deploy workflows")
    staging = workflows.get("deploy-staging.yml") or {}
    text = str(staging)
    if text.count("inputs.sha") != 9:
        errors.append("deploy-staging.yml: sha must be read exactly nine times "
                      "(three checkouts, three SHA steps, the schema gate env, "
                      "the user-flow synthetic expected SHA, the history guard comparison)")
    if text.count(DEPLOY_SHA_EXPRESSION) != 8:
        errors.append("deploy-staging.yml: every sha read but the history guard comparison "
                      "must use the pinned dispatch-or-head expression")
    jobs = staging.get("jobs") or {}
    for job_id in ("schema-gate", "roles-gate", "deploy"):
        job = jobs.get(job_id) or {}
        steps = job.get("steps") or []
        checkouts = [step for step in steps
                     if str(step.get("uses", "")).startswith("actions/checkout@")]
        if job_id == "deploy":
            # The deploy job carries one extra checkout: reviewed tooling
            # from the workflow head, so the deploy-ID steps run even when
            # the deployed pin predates the script. It must be exactly that
            # shape — the workflow head at shallow depth, never the
            # deployed ref — and credentialless like every checkout.
            tooling = [step for step in checkouts
                       if (step.get("with") or {}).get("path") == "tooling"]
            if len(tooling) != 1:
                errors.append("deploy-staging.yml:deploy: must carry exactly one "
                              "workflow-head tooling checkout")
            else:
                tooling_with = tooling[0].get("with") or {}
                if tooling_with.get("ref") != "${{ github.sha }}":
                    errors.append("deploy-staging.yml:deploy: tooling checkout must pin "
                                  "the workflow head, never the deployed ref")
                if tooling_with.get("fetch-depth") != "1":
                    errors.append("deploy-staging.yml:deploy: tooling checkout must stay shallow")
            checkouts = [step for step in checkouts
                         if (step.get("with") or {}).get("path") != "tooling"]
        if len(checkouts) != 1:
            errors.append(f"deploy-staging.yml:{job_id}: must check out exactly once")
            continue
        with_ = checkouts[0].get("with") or {}
        if with_.get("ref") != DEPLOY_SHA_EXPRESSION:
            errors.append(f"deploy-staging.yml:{job_id}: checkout must pin the dispatch-or-head ref")
        if with_.get("fetch-depth") != "0":
            errors.append(f"deploy-staging.yml:{job_id}: checkout must be full-depth for the ancestry check")
        sha_steps = [step for step in steps
                     if step.get("name") == "Deploy SHA must be exact and reachable from main"]
        if len(sha_steps) != 1:
            errors.append(f"deploy-staging.yml:{job_id}: must carry exactly one pinned-SHA check step")
            continue
        step = sha_steps[0]
        if (step.get("env") or {}).get("SOURCE_SHA") != DEPLOY_SHA_EXPRESSION:
            errors.append(f"deploy-staging.yml:{job_id}: SHA step must resolve the dispatch-or-head "
                          "SHA through the environment")
        run = str(step.get("run") or "")
        for needle in ("git rev-parse HEAD", "git fetch origin main",
                       'git merge-base --is-ancestor "$SOURCE_SHA" origin/main',
                       "[!0-9a-f]", '"${#SOURCE_SHA}"', "-eq 40"):
            if needle not in run:
                errors.append(f"deploy-staging.yml:{job_id}: SHA step must contain {needle}")
        # Four refusals, four exits: flipping any refusal green (in
        # particular the ancestry rejection) trips this count.
        if run.count("exit 1") != 4:
            errors.append(f"deploy-staging.yml:{job_id}: SHA step must refuse exactly four ways "
                          "(shape, length, checkout equality, ancestry)")
        # The shape check runs before the checkout equality: HEAD is always a
        # full SHA, so an equality-first order would refuse a short or branch
        # pin with no message before the shape refusal could explain it.
        if ("[!0-9a-f]" in run and "git rev-parse HEAD" in run
                and run.index("[!0-9a-f]") > run.index("git rev-parse HEAD")):
            errors.append(f"deploy-staging.yml:{job_id}: SHA step must check the 40-hex shape "
                          "before the checkout equality")
    gate_run = ((jobs.get("schema-gate") or {}).get("steps") or [])
    gate_run = [step for step in gate_run if "staging_schema_gate.py" in str(step.get("run") or "")]
    if len(gate_run) != 1 or (gate_run[0].get("env") or {}).get("SOURCE_SHA") != DEPLOY_SHA_EXPRESSION:
        errors.append("deploy-staging.yml:schema-gate: gate step must check the dispatch-or-head source")
    return errors


def soak_hold_errors(workflow, name="deploy-staging.yml"):
    """The soak hold defers push deploys loudly, never as a quiet green.

    While the Operator holds staging armed for a remediation-drill soak,
    push deploys must not redeploy it — and the run must fail fast here with
    a deferral message instead of reporting success without deploying (the
    production guard accepts a successful staging run as staging history, so
    a quiet skip would void that leg). Explicit arm/disarm dispatches
    proceed, but error-rate proofs honor the hold. One job runs first on every
    trigger (no `if:`, no `needs:`, no environment, no checkout): a single step
    binds the hold deadline, event and pinned dispatch-only proof flag. It
    exits 0 for a non-proof dispatch, empty var or expired deadline (fail open),
    and exits 1 for an unparsable or over-long value
    (fail closed with a fix-it message) or an armed hold. Both the expiry and
    the deferral leave a job-summary line. The gates wait on this job; the
    deploy and the synthetic follow transitively, so a failed hold skips them
    and fails the run red.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    job = jobs.get("soak-hold") or {}
    if job.get("name") != "soak hold (defer push deploys while armed)":
        errors.append(f"{name}:soak-hold: consumer job identity changed")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:soak-hold: must keep contents:read only")
    if not runner_allowed("soak-hold", job.get("runs-on")):
        errors.append(f"{name}:soak-hold: must use the routed runner expression for job 'soak-hold'")
    if job.get("timeout-minutes") != "5":
        errors.append(f"{name}:soak-hold: timeout must stay 5 minutes")
    for key in ("if", "needs", "uses", "environment", "services", "container",
                "continue-on-error", "strategy", "env"):
        if key in job:
            errors.append(f"{name}:soak-hold: must not set {key} "
                          "(it runs first, on every trigger, with no bindings)")
    text = str(job)
    if "secrets." in text:
        errors.append(f"{name}:soak-hold: must never read secrets")
    steps = job.get("steps", [])
    if len(steps) != 1 or any("if" in step or "continue-on-error" in step for step in steps):
        errors.append(f"{name}:soak-hold: must run exactly one unconditional check step")
        return errors
    step = steps[0]
    if step.get("env") != SOAK_HOLD_ENV:
        errors.append(f"{name}:soak-hold: step must bind exactly the hold deadline, event and rehearsal "
                      "via the environment")
    run = str(step.get("run") or "")
    for needle in ("set -euo pipefail", '[ "$EVENT" = "workflow_dispatch" ] && [ "${REHEARSAL:-false}" = "false" ]', '[ -z "$HOLD_UNTIL" ]',
                   'date -u -d "$HOLD_UNTIL" +%s', "14400", "GITHUB_STEP_SUMMARY", "::error::"):
        if needle not in run:
            errors.append(f"{name}:soak-hold: missing pin {needle}")
    if run.count("exit 1") != 3 or run.count("exit 0") != 3:
        errors.append(f"{name}:soak-hold: must exit 0 for dispatch/empty/expired and "
                      "1 for unparsable/over-long/armed")
    if "uses" in step or "${{" in run:
        errors.append(f"{name}:soak-hold: step must use the environment only, never an inline expression")
    return errors


def user_flow_synthetic_errors(workflow, name="deploy-staging.yml"):
    """The exact-SHA user-flow synthetic gates promotion, read-only.

    One job runs after the deploy with no Environment, no secret and no
    write grant: it checks out the reviewed probe from the workflow head and
    runs the read-only staging health-contract probe pinned to the deployed
    dispatch-or-head SHA, refusing when the staging origin is unset.
    certify-promotion needs it, so a red or skipped synthetic certifies no
    bytes, and the production guard refuses a staging attempt without a
    successful job of this exact name (scripts/test-deploy-production.py).
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    job = jobs.get("user-flow-synthetic") or {}
    if job.get("name") != "staging user-flow synthetic (exact SHA)":
        errors.append(f"{name}:user-flow-synthetic: job identity changed (the production guard reads it by name)")
    if job.get("needs") != "deploy":
        errors.append(f"{name}:user-flow-synthetic: must run after the deploy")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:user-flow-synthetic: must keep contents:read only")
    if not runner_allowed("user-flow-synthetic", job.get("runs-on")):
        errors.append(f"{name}:user-flow-synthetic: must use the routed runner expression "
                      "for job 'user-flow-synthetic'")
    for key in ("if", "uses", "environment", "services", "container",
                "continue-on-error", "strategy", "concurrency"):
        if key in job:
            errors.append(f"{name}:user-flow-synthetic: must not set {key}")
    text = str(job).lower()
    if "secrets." in text or any(marker in text for marker in DEPLOY_MARKERS[:4]):
        errors.append(f"{name}:user-flow-synthetic: must never read a secret or deploy")
    steps = job.get("steps") or []
    probes = [step for step in steps
              if "staging_health_contract_probe.py" in str(step.get("run", ""))]
    if len(probes) != 1 or any(key in probes[0] for key in ("if", "continue-on-error")):
        errors.append(f"{name}:user-flow-synthetic: must run the probe in exactly one unconditional step")
    else:
        probe = probes[0]
        run = str(probe.get("run", ""))
        if probe.get("env") != {"STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}",
                                "EXPECTED_SHA": DEPLOY_SHA_EXPRESSION}:
            errors.append(f"{name}:user-flow-synthetic: probe must bind exactly the staging origin "
                          "and the deployed dispatch-or-head SHA")
        if '--expected-sha "$EXPECTED_SHA"' not in run or "set -euo pipefail" not in run:
            errors.append(f"{name}:user-flow-synthetic: probe must enforce the exact deployed SHA")
        if '[ -z "$STAGING_WORKER_URL" ]' not in run or "exit 1" not in run:
            errors.append(f"{name}:user-flow-synthetic: an unset staging origin must fail, never pass empty")
    checkouts = [step for step in steps if str(step.get("uses", "")).startswith("actions/checkout@")]
    if len(checkouts) != 1 or (checkouts[0].get("with") or {}).get("ref") != "${{ github.sha }}" \
            or (checkouts[0].get("with") or {}).get("persist-credentials") != "false":
        errors.append(f"{name}:user-flow-synthetic: must check out the reviewed workflow head without credentials")
    certify = jobs.get("certify-promotion") or {}
    if "user-flow-synthetic" not in (certify.get("needs") or []):
        errors.append(f"{name}:certify-promotion: must need the user-flow synthetic")
    return errors


def pinned_history_guard_errors(workflow, name="deploy-staging.yml"):
    """A pinned drill deploy stays red, so it never reads as staging history.

    The production guard keys staging runs by head SHA: a dispatch that
    deploys a pinned SHA below the workflow head would otherwise mint a
    successful run for a commit that never served staging. One job runs after
    the voice synthetic (a deploy-job step cannot judge: a failure there
    would auto-roll back the drill build): a single step binds the raw pin
    and the head, passes when the pin is empty or equal to the head (the
    deploy served the head, so the run is honest history), and fails the run
    red otherwise with the reason. The drill still deploys and the synthetic
    still runs; only the run verdict is red, exactly like the rollback
    drill's intentionally-red run.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    job = jobs.get("pinned-history-guard") or {}
    if job.get("name") != "pinned drill deploys stay red (not staging history)":
        errors.append(f"{name}:pinned-history-guard: consumer job identity changed")
    if job.get("needs") != "voice-synthetic":
        errors.append(f"{name}:pinned-history-guard: must run after the voice synthetic "
                      "(it judges a deployed drill)")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:pinned-history-guard: must keep contents:read only")
    if not runner_allowed("pinned-history-guard", job.get("runs-on")):
        errors.append(f"{name}:pinned-history-guard: must use the routed runner expression "
                      "for job 'pinned-history-guard'")
    if job.get("timeout-minutes") != "5":
        errors.append(f"{name}:pinned-history-guard: timeout must stay 5 minutes")
    for key in ("if", "uses", "environment", "services", "container",
                "continue-on-error", "strategy", "env"):
        if key in job:
            errors.append(f"{name}:pinned-history-guard: must not set {key} "
                          "(it judges after the synthetic, with no bindings)")
    if "secrets." in str(job):
        errors.append(f"{name}:pinned-history-guard: must never read a secret")
    steps = job.get("steps", [])
    if len(steps) != 1 or any("if" in step or "continue-on-error" in step for step in steps):
        errors.append(f"{name}:pinned-history-guard: must run exactly one unconditional verdict step")
        return errors
    step = steps[0]
    if step.get("env") != {"PINNED_SHA": "${{ inputs.sha }}", "HEAD_SHA": "${{ github.sha }}"}:
        errors.append(f"{name}:pinned-history-guard: step must bind exactly the pin and the head "
                      "via the environment")
    run = str(step.get("run") or "")
    for needle in ("set -euo pipefail", '[ -z "$PINNED_SHA" ]', '[ "$PINNED_SHA" = "$HEAD_SHA" ]',
                   "not staging history", "::error::"):
        if needle not in run:
            errors.append(f"{name}:pinned-history-guard: missing pin {needle}")
    if run.count("exit 1") != 1 or run.count("exit 0") != 1:
        errors.append(f"{name}:pinned-history-guard: must exit 0 for head deploys and 1 for pins below the head")
    if "uses" in step or "${{" in run:
        errors.append(f"{name}:pinned-history-guard: step must use the environment only, "
                      "never an inline expression")
    return errors


def staging_deploy_identity_errors(workflow, name="deploy-staging.yml"):
    """A pinned drill deploy stamps the pin, never the workflow head.

    The checkout builds the pinned commit, so every build-identity and
    provenance consumer in the deploy job must stamp the validated
    dispatch-or-head SHA: the deploy SHA step exports it as DEPLOY_SHA, the
    prepare step overrides GITHUB_SHA with it (staging_rollout.py prepare
    reads GITHUB_SHA as the build revision for BOT_BUILD_REVISION,
    CONTAINER_RELEASE and the baseline receipt the verify step judges
    against), and the wrangler --message, the ownership and rollback actors
    and the promotion-gate SHA resolve it. Stamping the head instead would
    serve the pin labeled as the head, break the runbook's read-the-serving-
    SHA-from-build_revision step and poison the next rollback snapshot's
    serving_sha (recorded from the workers/message annotation). No other
    github.sha may appear in the deploy job outside the two pinned
    dispatch-or-head expressions (checkout ref, SHA-step env) and the
    workflow-head tooling checkout ref (which runs deploy_id.py for pins
    whose commits predate it).
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    deploy = jobs.get("deploy") or {}
    steps = deploy.get("steps") or []

    def find(label, predicate):
        found = [step for step in steps if predicate(step)]
        if len(found) != 1:
            errors.append(f"{name}:deploy: must carry exactly one {label}")
            return None
        return found[0]

    sha_step = find("SHA check step",
                    lambda step: step.get("name") == "Deploy SHA must be exact and reachable from main")
    if sha_step is not None and 'echo "DEPLOY_SHA=$SOURCE_SHA" >> "$GITHUB_ENV"' not in str(sha_step.get("run") or ""):
        errors.append(f"{name}:deploy: SHA step must export the validated dispatch-or-head SHA")
    prepare = find("staging identity snapshot step",
                   lambda step: step.get("name") == "Snapshot staging identity and generate per-deploy image provenance")
    if prepare is not None and (prepare.get("env") or {}).get("GITHUB_SHA") != "${{ env.DEPLOY_SHA }}":
        errors.append(f"{name}:deploy: prepare step must stamp the validated deploy SHA as GITHUB_SHA")
    wrangler = find("wrangler deploy step", lambda step: step.get("id") == "deploy")
    if wrangler is not None:
        command = str((wrangler.get("with") or {}).get("command") or "")
        if "--message ${{ env.DEPLOY_SHA }}" not in command or "github.sha" in command:
            errors.append(f"{name}:deploy: wrangler deploy must message the validated deploy SHA")
    takeover = find("ownership takeover step", lambda step: step.get("id") == "takeover")
    if takeover is not None and (takeover.get("env") or {}).get("OWNERSHIP_ACTOR") != \
            "github-actions:${{ github.run_id }}:${{ env.DEPLOY_SHA }}":
        errors.append(f"{name}:deploy: ownership actor must name the validated deploy SHA")
    gate = find("promotion error-rate gate step", lambda step: step.get("id") == "error_rate_gate")
    if gate is not None and (gate.get("env") or {}).get("SHA") != "${{ env.DEPLOY_SHA }}":
        errors.append(f"{name}:deploy: promotion gate must judge the validated deploy SHA")
    rollback = find("auto-rollback step", lambda step: step.get("id") == "auto_rollback")
    if rollback is not None and (rollback.get("env") or {}).get("ROLLBACK_ACTOR") != \
            "github-actions:${{ github.run_id }}:${{ env.DEPLOY_SHA }}":
        errors.append(f"{name}:deploy: rollback actor must name the validated deploy SHA")
    text = str(deploy)
    if text.count("github.sha") != text.count(DEPLOY_SHA_EXPRESSION) + 1 or text.count("github.sha") != 3:
        errors.append(f"{name}:deploy: github.sha may only appear in the checkout ref, "
                      "the SHA-step dispatch-or-head expressions and the tooling checkout ref")
    return errors


def staging_deploy_id_errors(workflow, name="deploy-staging.yml"):
    """The staging deploy ID is derived in-lane and recorded as a Deployment.

    The compute step derives `YYYY.MM.DD.N+sha7` from the validated deploy
    SHA inside the serialized staging lane (N counts today's recorded staging
    deploy IDs, so a queued entry cannot mint a duplicate) and exports it
    before the prepare step stamps it. Both ID steps run reviewed tooling
    from the workflow's own commit, since a pinned drill deploys a commit
    whose checkout may predate the script. The record step stores the deploy
    as a GitHub Deployment carrying the ID — never a tag or Release — with
    success only when the rollout verify and the error-rate gate both
    passed; it runs after the auto-rollback so bookkeeping can never gate
    recovery. No other step may call deploy_id.py.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    deploy = jobs.get("deploy") or {}
    steps = deploy.get("steps") or []

    def find(label, predicate):
        found = [step for step in steps if predicate(step)]
        if len(found) != 1:
            errors.append(f"{name}:deploy: must carry exactly one {label}")
            return None, -1
        return found[0], steps.index(found[0])

    compute, compute_at = find(
        "deploy-ID compute step",
        lambda step: step.get("name") == "Compute the deploy ID for this staging deploy")
    if compute is not None:
        if (compute.get("env") or {}).get("GH_TOKEN") != "${{ github.token }}":
            errors.append(f"{name}:deploy: compute step must list Deployments with github.token")
        run = str(compute.get("run") or "")
        if ("tooling/scripts/deploy_id.py next --environment staging --sha \"$DEPLOY_SHA\""
                not in run or '>> "$GITHUB_ENV"' not in run):
            errors.append(f"{name}:deploy: compute step must derive the staging ID "
                          "from the validated deploy SHA via reviewed tooling")
    prepare, prepare_at = find(
        "staging identity snapshot step",
        lambda step: step.get("name") == "Snapshot staging identity and generate per-deploy image provenance")
    if compute_at >= 0 and prepare_at >= 0 and compute_at > prepare_at:
        errors.append(f"{name}:deploy: compute step must run before the prepare step stamps the ID")
    verify, _ = find(
        "rollout verify step",
        lambda step: step.get("name") == "Verify exact new rollout and serving Worker/container readiness")
    if verify is not None and verify.get("id") != "verify":
        errors.append(f"{name}:deploy: verify step must carry id verify for the record step")
    gate, gate_at = find("promotion error-rate gate step",
                         lambda step: step.get("id") == "error_rate_gate")
    record, record_at = find(
        "deployment record step",
        lambda step: step.get("name") == "Record the staging deployment")
    if record is not None:
        if record.get("if") != "always() && steps.deploy.outcome == 'success'":
            errors.append(f"{name}:deploy: record step must run whenever a deploy went out")
        env = record.get("env") or {}
        if env.get("GH_TOKEN") != "${{ github.token }}":
            errors.append(f"{name}:deploy: record step must create the Deployment with github.token")
        if env.get("VERIFY_OUTCOME") != "${{ steps.verify.outcome }}":
            errors.append(f"{name}:deploy: record step must judge the rollout verify outcome")
        if env.get("GATE_OUTCOME") != "${{ steps.error_rate_gate.outcome }}":
            errors.append(f"{name}:deploy: record step must judge the error-rate gate outcome")
        run = str(record.get("run") or "")
        if ("tooling/scripts/deploy_id.py record --environment staging --sha \"$DEPLOY_SHA\""
                not in run or '--deploy-id "$DEPLOY_ID"' not in run):
            errors.append(f"{name}:deploy: record step must record the staging ID "
                          "for the validated deploy SHA via reviewed tooling")
        if "--rollback" in run:
            errors.append(f"{name}:deploy: staging has no rollback mode to record")
    if gate_at >= 0 and record_at >= 0 and record_at < gate_at:
        errors.append(f"{name}:deploy: record step must run after the error-rate gate judges")
    rollback_at = next((index for index, step in enumerate(steps)
                        if step.get("id") == "auto_rollback"), -1)
    if rollback_at >= 0 and record_at >= 0 and record_at < rollback_at:
        errors.append(f"{name}:deploy: record step must run after the auto-rollback "
                      "so bookkeeping can never gate recovery")
    if str(deploy).count("deploy_id.py") != 2:
        errors.append(f"{name}:deploy: only the compute and record steps may call deploy_id.py")
    return errors


def staging_claim_errors(claim, name="staging-migrate.yml",
                           job_name="staging-migrate (claim)",
                           manifest="staging-migrate-manifest",
                           claim_artifact="staging-migrate-apply-claim",
                           script="scripts/ci/staging_migrate_claim.py",
                           host_env="STAGING_HOST", host_input="staging_host",
                           db_env="STAGING_DATABASE", db_input="staging_database",
                           branch_env="STAGING_BRANCH_ID", branch_input="staging_branch_id"):
    """Claim transport must complete before the protected apply job waits.

    Parametrized by workflow name, job identity, artifact names, publisher
    script and host/database/branch env/input names so the production mirror
    pins the same shape with its own names.
    """
    errors = []
    prefix = f"{name}:claim:"
    if claim.get("name") != job_name:
        errors.append(f"{prefix} consumer job identity changed")
    if "environment" in claim or "secrets." in str(claim) or "uses" in claim:
        errors.append(f"{prefix} must be unprotected with no secrets or reusable job")
    if claim.get("permissions") != {"contents": "read"}:
        errors.append(f"{prefix} must keep contents:read only")
    if claim.get("needs") != "plan" or claim.get("if") != "github.ref == 'refs/heads/main' && inputs.mode == 'apply'":
        errors.append(f"{prefix} must wait for a green plan, main and mode=apply")
    if not runner_allowed("claim", claim.get("runs-on")):
        errors.append(f"{prefix} must use the claim routed runner")
    steps = claim.get("steps", [])
    if len(steps) != 4 or any("if" in s or "continue-on-error" in s for s in steps):
        errors.append(f"{prefix} must execute four fail-closed transport steps")
        return errors
    checkout, fetch, publish, upload = steps
    if (not str(checkout.get("uses", "")).startswith("actions/checkout@")
            or checkout.get("with") != {"persist-credentials": "false", "ref": "${{ github.sha }}"}):
        errors.append(f"{prefix} checkout must pin the workflow head without persisted credentials")
    if (not str(fetch.get("uses", "")).startswith("actions/download-artifact@")
            or fetch.get("with") != {"name": manifest, "path": "current-plan"}):
        errors.append(f"{prefix} must read this dispatch's plan artifact, not an arbitrary run")
    pairs = [("MODE", "mode"), ("SOURCE_SHA", "source_sha"), (host_env, host_input),
             (db_env, db_input)]
    if branch_env is not None and branch_input is not None:
        pairs.append((branch_env, branch_input))
    pairs.extend([("RECOVERY_REF", "recovery_evidence_ref"),
                  ("ACL_REF", "acl_plan_ref"), ("EXPECTED_PENDING", "expected_pending"),
                  ("EXPECTED_STORE_PENDING", "expected_store_pending"),
                  ("PLAN_MANIFEST_SHA256", "plan_manifest_sha256"),
                  ("STORE_PLAN_MANIFEST_SHA256", "store_plan_manifest_sha256"),
                  ("PLAN_RUN_ID", "plan_run_id")])
    expected_env = {key: "${{ inputs." + value + " }}" for key, value in pairs}
    if publish.get("env") != expected_env or publish.get("shell") != "bash":
        errors.append(f"{prefix} must pass exactly the dispatch fields via env, in bash")
    script_text = str(publish.get("run", ""))
    for pin in ("set -o pipefail", f"python3 {script}",
                f"--manifest current-plan/{manifest}.json",
                f"--output {claim_artifact}.json"):
        if pin not in script_text:
            errors.append(f"{prefix} missing publisher pin {pin}")
    if not str(upload.get("uses", "")).startswith("actions/upload-artifact@"):
        errors.append(f"{prefix} must upload the claim")
    options = upload.get("with", {})
    if options != {"name": claim_artifact, "path": f"{claim_artifact}.json",
                   "if-no-files-found": "error", "retention-days": "14", "compression-level": "0"}:
        errors.append(f"{prefix} must publish the named claim with stored ZIP entries and fail if absent")
    return errors


def production_claim_errors(claim):
    """Production mirror of the claim transport (production-migrate.yml)."""
    return staging_claim_errors(
        claim,
        name="production-migrate.yml",
        job_name="production-migrate (claim)",
        manifest="production-migrate-manifest",
        claim_artifact="production-migrate-apply-claim",
        script="scripts/ci/production_migrate_claim.py",
        host_env="PRODUCTION_HOST",
        host_input="production_host",
        db_env="PRODUCTION_DATABASE",
        db_input="production_database",
        branch_env="PRODUCTION_BRANCH_ID",
        branch_input="production_branch_id",
    )


def job_env_text(job):
    """Job-level `env` plus every step's `env`: everywhere a binding can be exported."""
    return " ".join([str(job.get("env", ""))]
                    + [str(step.get("env", "")) for step in job.get("steps", [])])


def migrate_errors(workflow, *, name, plan_env, apply_env, plan_secret,
                     migrator_secret, host_input, db_input, manifest,
                     target_flag, host_flag, db_flag, other_host_flag,
                     other_db_flag, other_plan_secret, other_migrator_secret,
                     branch_input="staging_branch_id", branch_flag="--staging-branch-id",
                     branch_label="staging", other_branch_flag=None):
    """Parametrized migration-runner shape (staging and production mirrors).

    Dispatch-only with exactly the reviewed inputs (each target carries its
    own PlanetScale branch pin), three jobs
    (plan always runs through the no-reviewer plan environment, unprotected
    claim transports the apply request, apply runs only for mode=apply after a
    green plan and claim through the reviewed apply environment), each job with
    its own routed runner and pipefail Run step. No push/pull_request/schedule
    trigger, no wrangler/probe markers. The caller supplies the workflow file
    name, environment names, secret names, host/database/branch input names,
    artifact prefix, runner target/host/database/branch flags and the other
    target's flags and secrets (which must never appear). A None branch pin
    skips the branch assertions for that target.
    """
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    inputs = ((on.get("workflow_dispatch") or {}).get("inputs") or {})
    optional = {"mode", "expected_pending", "expected_store_pending",
                "plan_manifest_sha256", "store_plan_manifest_sha256", "plan_run_id"}
    expected = {"mode", "source_sha", host_input, db_input,
                "recovery_evidence_ref", "acl_plan_ref", "expected_pending",
                "expected_store_pending", "plan_manifest_sha256",
                "store_plan_manifest_sha256", "plan_run_id"}
    if branch_input is not None:
        expected.add(branch_input)
        optional.add(branch_input)
    if set(inputs) != expected:
        errors.append(f"{name}: workflow_dispatch inputs must be exactly {sorted(expected)}")
    else:
        mode = inputs.get("mode") or {}
        if (mode.get("type") != "choice"
                or set(mode.get("options") or []) != {"plan", "apply"}
                or mode.get("default") != "plan"):
            errors.append(f"{name}: mode must be plan/apply defaulting to plan")
        for key in expected - optional:
            field = inputs.get(key) or {}
            if str(field.get("required")).lower() != "true":
                errors.append(f"{name}: input {key} must be required")
        if branch_input is not None:
            branch = inputs.get(branch_input) or {}
            if (str(branch.get("required")).lower() != "false"
                    or branch.get("default") != ""
                    or "branch" not in str(branch.get("description")).lower()):
                errors.append(f"{name}: {branch_input} must stay optional, default empty, "
                              "and documented as the PlanetScale branch pin")
        pending = inputs.get("expected_pending") or {}
        if (str(pending.get("required")).lower() != "false"
                or pending.get("default") != ""
                or "ascending" not in str(pending.get("description")).lower()):
            errors.append(f"{name}: expected_pending must stay optional, default empty, "
                          "and documented as the ascending reviewed plan list")
        store_pending = inputs.get("expected_store_pending") or {}
        if (str(store_pending.get("required")).lower() != "false"
                or store_pending.get("default") != ""
                or "ascending" not in str(store_pending.get("description")).lower()):
            errors.append(f"{name}: expected_store_pending must stay optional, default empty, "
                          "and documented as the ascending reviewed store plan list")
        for key in ("plan_manifest_sha256", "store_plan_manifest_sha256", "plan_run_id"):
            field = inputs.get(key) or {}
            if str(field.get("required")).lower() != "false" or field.get("default") != "":
                errors.append(f"{name}: {key} must stay optional and default empty "
                              "(the runner requires it for apply, not the dispatch)")
        digest = inputs.get("plan_manifest_sha256") or {}
        if "sha" not in str(digest.get("description")).lower():
            errors.append(f"{name}: plan_manifest_sha256 must document the manifest-hash contract "
                          "(apply refuses unless it matches on the same source_sha)")
        store_digest = inputs.get("store_plan_manifest_sha256") or {}
        if "sha" not in str(store_digest.get("description")).lower():
            errors.append(f"{name}: store_plan_manifest_sha256 must document the store "
                          "manifest-hash contract (apply refuses unless it matches)")
        acl = inputs.get("acl_plan_ref") or {}
        if "bare" not in str(acl.get("description")).lower():
            errors.append(f"{name}: acl_plan_ref must document the bare reference contract "
                          "(refused before any DDL otherwise)")
        host_desc = inputs.get(host_input) or {}
        if "binding must match" not in str(host_desc.get("description")).lower():
            errors.append(f"{name}: {host_input} must document the binding-match contract")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"plan", "claim", "apply"}:
        errors.append(f"{name}: jobs must be exactly plan, claim and apply")
        return errors
    plan, claim, apply = jobs.get("plan", {}), jobs.get("claim", {}), jobs.get("apply", {})
    if name == "staging-migrate.yml":
        errors.extend(staging_claim_errors(claim))
    else:
        errors.extend(production_claim_errors(claim))
    if plan.get("environment") != plan_env:
        errors.append(f"{name}:plan: must read the {plan_env} Environment binding")
    if apply.get("environment") != apply_env:
        errors.append(f"{name}:apply: must read the {apply_env} Environment binding")
    if plan.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:plan: must keep contents:read only (it produces the manifest)")
    if apply.get("permissions") != {"contents": "read", "actions": "read"}:
        errors.append(f"{name}:apply: must carry exactly contents:read plus actions:read "
                      "(provenance artifact fetch, nothing more)")
    if plan.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:plan: must run only from main")
    if apply.get("if") != "github.ref == 'refs/heads/main' && inputs.mode == 'apply'":
        errors.append(f"{name}:apply: must run only from main for mode=apply")
    if apply.get("needs") != ["plan", "claim"]:
        errors.append(f"{name}:apply: must wait for a green plan and pre-approval claim")
    for job_id, job in (("plan", plan), ("apply", apply)):
        if not runner_allowed(job_id, job.get("runs-on")):
            errors.append(f"{name}:{job_id}: must use the routed runner expression for job '{job_id}'")
        text = str(job).lower()
        if job.get("uses") is not None or any(marker in text for marker in DEPLOY_MARKERS):
            errors.append(f"{name}:{job_id}: deployment/probe alternative outside approved deploy workflows")
        run_steps = [step for step in job.get("steps", []) if "tee" in str(step.get("run", ""))]
        if not run_steps:
            errors.append(f"{name}:{job_id}: no Run step piping through tee")
        for step in run_steps:
            if step.get("shell") != "bash" or "set -o pipefail" not in str(step.get("run", "")):
                errors.append(f"{name}:{job_id}: Run step must use a pipefail shell so a "
                              "migrator refusal/failure fails the job instead of reporting green")
    plan_env_text = job_env_text(plan)
    apply_env_text = job_env_text(apply)
    plan_job, apply_job = str(plan).lower(), str(apply).lower()
    if plan_secret not in plan_env_text:
        errors.append(f"{name}:plan: must read only the {plan_secret} binding")
    if migrator_secret.lower() in plan_job:
        errors.append(f"{name}:plan: must never read the migrator {migrator_secret} binding")
    if migrator_secret not in apply_env_text:
        errors.append(f"{name}:apply: must read only the {migrator_secret} binding")
    if plan_secret.lower() in apply_job:
        errors.append(f"{name}:apply: must never read the plan {plan_secret} binding")
    # Cross-target isolation: a staging job never reads a production binding
    # and a production job never reads a staging binding, in any case or key.
    for other in (other_plan_secret.lower(), other_migrator_secret.lower()):
        if other in plan_job:
            errors.append(f"{name}:plan: must never read the other target's {other} binding")
        if other in apply_job:
            errors.append(f"{name}:apply: must never read the other target's {other} binding")
    for job_id, text in (("plan", plan_job), ("apply", apply_job)):
        if "tojson(secrets" in text.replace(" ", "") or "secrets[" in text.replace(" ", ""):
            errors.append(f"{name}:{job_id}: must name each secret explicitly "
                          "(no toJSON(secrets) or indexed secrets access)")
    plan_runs = " ".join(str(step.get("run", "")) for step in plan.get("steps", []))
    apply_runs = " ".join(str(step.get("run", "")) for step in apply.get("steps", []))
    if target_flag not in plan_runs or target_flag not in apply_runs:
        errors.append(f"{name}: must run the migrator with {target_flag} on both jobs")
    if other_host_flag in plan_runs or other_host_flag in apply_runs:
        errors.append(f"{name}: must never pass the other target's {other_host_flag} flag")
    if other_db_flag in plan_runs or other_db_flag in apply_runs:
        errors.append(f"{name}: must never pass the other target's {other_db_flag} flag")
    if other_branch_flag is not None and (other_branch_flag in plan_runs
                                          or other_branch_flag in apply_runs):
        errors.append(f"{name}: must never pass the other target's {other_branch_flag} flag")
    if host_flag not in plan_runs or host_flag not in apply_runs:
        errors.append(f"{name}: must pass {host_flag} to the runner on both jobs")
    if db_flag not in plan_runs or db_flag not in apply_runs:
        errors.append(f"{name}: must pass {db_flag} to the runner on both jobs")
    if "--plan " not in plan_runs or "--apply" in plan_runs:
        errors.append(f"{name}:plan: must run the migrator with --plan only")
    if "--apply " not in apply_runs or "--plan " in apply_runs:
        errors.append(f"{name}:apply: must run the migrator with --apply only")
    if "--plan-manifest-sha256" not in apply_runs or "--plan-run-id" not in apply_runs:
        errors.append(f"{name}:apply: must pass the plan-bound manifest hash and run id to the runner")
    if "--store-plan-manifest-sha256" not in apply_runs or "--expected-store-pending" not in apply_runs:
        errors.append(f"{name}:apply: must pass the store plan-bound manifest hash and pending list to the runner")
    if "--plan-manifest-sha256" in plan_runs or "--plan-run-id" in plan_runs:
        errors.append(f"{name}:plan: must not take plan-bound inputs (it produces the manifest)")
    if "--store-plan-manifest-sha256" in plan_runs:
        errors.append(f"{name}:plan: must not take the store plan-bound hash (it produces the manifest)")
    if "--plan-manifest-path" not in apply_runs:
        errors.append(f"{name}:apply: must pass the producing run's downloaded manifest to the runner")
    if "--plan-manifest-path" in plan_runs:
        errors.append(f"{name}:plan: must not take the provenance manifest path (it produces the manifest)")
    # PlanetScale branch pin: a target that declares the pin passes the
    # non-secret branch id to the runner on both jobs; the runner requires it
    # for `*.psdb.cloud` hosts and refuses pooled ports, `|bouncer` usernames
    # and branch mismatches before any DDL.
    if branch_flag is not None:
        if branch_flag not in plan_runs:
            errors.append(f"{name}:plan: must pass the {branch_label} branch pin to the runner")
        if branch_flag not in apply_runs:
            errors.append(f"{name}:apply: must pass the {branch_label} branch pin to the runner")
    apply_fetch = [step for step in apply.get("steps", [])
                   if str(step.get("uses", "")).startswith("actions/download-artifact@")]
    if len(apply_fetch) != 1:
        errors.append(f"{name}:apply: must fetch exactly one artifact (the producing plan manifest)")
    else:
        fetch = apply_fetch[0]
        fetch_with = fetch.get("with", {})
        if fetch_with.get("name") != manifest:
            errors.append(f"{name}:apply: must fetch the {manifest} artifact")
        if "plan_run_id" not in str(fetch_with.get("run-id", "")):
            errors.append(f"{name}:apply: must fetch the artifact from the plan_run_id run")
        if fetch.get("continue-on-error") is True:
            errors.append(f"{name}:apply: the provenance fetch must fail the job, never continue-on-error")
    plan_uses = [step.get("uses", "") for step in plan.get("steps", [])]
    if not any(str(u).startswith("actions/upload-artifact@") for u in plan_uses):
        errors.append(f"{name}:plan: must upload the {manifest}.json run artifact")
    if any(str(step.get("uses", "")).startswith("actions/download-artifact@")
           for step in plan.get("steps", [])):
        errors.append(f"{name}:plan: must not fetch artifacts by run id (it produces the manifest)")
    plan_text = str(plan.get("steps", []))
    if manifest not in plan_text:
        errors.append(f"{name}:plan: must name the {manifest} artifact")
    uploads = [step for step in plan.get("steps", [])
               if str(step.get("uses", "")).startswith("actions/upload-artifact@")]
    if len(uploads) != 1 or uploads[0].get("with", {}).get("compression-level") != "0":
        errors.append(f"{name}:plan: must upload the manifest with stored ZIP entries")
    return errors


def staging_migrate_errors(workflow, name="staging-migrate.yml",
                           plan_env="staging-migrate-plan", apply_env="staging-migrate-apply",
                           plan_secret="TWO_BOT_STAGING_PLAN_DATABASE_URL",
                           migrator_secret="TWO_BOT_STAGING_MIGRATOR_DATABASE_URL",
                           host_input="staging_host", db_input="staging_database",
                           manifest="staging-migrate-manifest",
                           target_flag="--target staging", host_flag="--staging-host",
                           db_flag="--staging-database", other_host_flag="--production-host",
                           other_db_flag="--production-database",
                           other_plan_secret="TWO_BOT_PRODUCTION_PLAN_DATABASE_URL",
                           other_migrator_secret="TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL"):
    """Manual staging-only SQLx migration runner. Parametrized by name,
    environments and secret names so the production mirror pins the same shape.
    """
    return migrate_errors(workflow, name=name, plan_env=plan_env, apply_env=apply_env,
                          plan_secret=plan_secret, migrator_secret=migrator_secret,
                          host_input=host_input, db_input=db_input, manifest=manifest,
                          target_flag=target_flag, host_flag=host_flag, db_flag=db_flag,
                          other_host_flag=other_host_flag, other_db_flag=other_db_flag,
                          other_plan_secret=other_plan_secret,
                          other_migrator_secret=other_migrator_secret,
                          other_branch_flag="--production-branch-id")


def production_migrate_errors(workflow):
    """Production mirror of the migration-runner shape.

    Production carries its own PlanetScale branch pin
    (`production_branch_id` / `--production-branch-id`), mirroring staging
    with production names; the staging pin must never appear.
    """
    return migrate_errors(workflow, name="production-migrate.yml",
                          plan_env="production-migrate-plan", apply_env="production-migrate-apply",
                          plan_secret="TWO_BOT_PRODUCTION_PLAN_DATABASE_URL",
                          migrator_secret="TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL",
                          host_input="production_host", db_input="production_database",
                          manifest="production-migrate-manifest",
                          target_flag="--target production", host_flag="--production-host",
                          db_flag="--production-database", other_host_flag="--staging-host",
                          other_db_flag="--staging-database",
                          other_plan_secret="TWO_BOT_STAGING_PLAN_DATABASE_URL",
                          other_migrator_secret="TWO_BOT_STAGING_MIGRATOR_DATABASE_URL",
                          branch_input="production_branch_id",
                          branch_flag="--production-branch-id",
                          branch_label="production",
                          other_branch_flag="--staging-branch-id")


ROLLBACK_DRILL_ENV = {
    "STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}",
    "OWNERSHIP_CONTROL_TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}",
    "CLOUDFLARE_API_TOKEN": "${{ secrets.CLOUDFLARE_API_TOKEN }}",
    "CLOUDFLARE_ACCOUNT_ID": "${{ secrets.CLOUDFLARE_ACCOUNT_ID }}",
    "TARGET_VERSION": "${{ inputs.target_version }}",
}


def rollback_proof_job_errors(job, mode):
    """Pin the dispatcher/observer to reviewed tooling and the smallest token."""
    errors = []
    permissions = {"contents": "read"}
    env = {}
    if mode in ("run", "watch"):
        permissions["actions"] = "write" if mode == "run" else "read"
        env["GH_TOKEN"] = "${{ github.token }}"
    if mode in ("alert", "watch"):
        # The incident issue sink: the job token with issues: write, no secret.
        permissions["issues"] = "write"
        env["GITHUB_TOKEN"] = "${{ github.token }}"
    if job.get("permissions") != permissions or "environment" in job:
        errors.append(f"rollback proof {mode}: least-privilege token/environment changed")
    for key in ("env", "uses", "services", "container", "continue-on-error", "strategy", "concurrency"):
        if key in job:
            errors.append(f"rollback proof {mode}: must not set {key}")
    job_id = {"run": "proof", "watch": "rollback-proof-watch", "alert": "alert"}[mode]
    if not runner_allowed(job_id, job.get("runs-on")):
        errors.append(f"rollback proof {mode}: must use the routed public-hosted runner")
    if str(job.get("timeout-minutes")) != ("80" if mode == "run" else "5"):
        errors.append(f"rollback proof {mode}: bounded job timeout changed")
    steps = job.get("steps") or []
    if len(steps) != (3 if mode == "run" else 2):
        return errors + [f"rollback proof {mode}: unexpected steps"]
    if any("if" in step or "continue-on-error" in step for step in steps):
        errors.append(f"rollback proof {mode}: steps must run and fail closed")
    checkout, command = steps[:2]
    if (not str(checkout.get("uses") or "").startswith("actions/checkout@")
            or (checkout.get("with") or {}).get("persist-credentials") != "false"):
        errors.append(f"rollback proof {mode}: checkout must not persist credentials")
    run = f"python3 scripts/rollback_proof.py {mode}"
    if mode == "run":
        run += ' --evidence "$RUNNER_TEMP/proof.json"'
        upload = steps[2]
        if (not str(upload.get("uses") or "").startswith("actions/upload-artifact@")
                or (upload.get("with") or {}).get("name") != "staging-rollback-proof"
                or (upload.get("with") or {}).get("path") != "${{ runner.temp }}/proof.json"
                or (upload.get("with") or {}).get("if-no-files-found") != "error"):
            errors.append("rollback proof run: must upload the non-empty proof artifact")
    if command.get("run") != run or command.get("env") != env:
        errors.append(f"rollback proof {mode}: only the reviewed script and bindings may run")
    return errors


def staging_rollback_drill_errors(workflow):
    """Daily staging-only auto-rollback proof and optional manual Worker drill.

    The orchestrator must not hold deploy-staging while waiting for its child;
    only the manual Worker drill shares that lane. No production bindings.
    """
    name = "staging-rollback-drill.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch", "schedule"} or on.get("schedule") != [{"cron": "17 3 * * *"}]:
        errors.append(f"{name}: must run daily at 03:17 UTC plus manual dispatch only")
    dispatch = on.get("workflow_dispatch") or {}
    inputs = dispatch.get("inputs") or {}
    if set(dispatch) != {"inputs"} or set(inputs) != {"target_version"}:
        errors.append(f"{name}: workflow_dispatch must carry exactly the target_version input")
    else:
        field = inputs["target_version"] or {}
        if (field.get("type") != "string" or str(field.get("required")).lower() != "false"
                or field.get("default") != ""):
            errors.append(f"{name}: target_version must be optional/empty for the daily automatic proof")
    if workflow.get("concurrency") != {"group": "staging-rollback-proof-${{ github.event_name }}", "cancel-in-progress": "false"}:
        errors.append(f"{name}: scheduled/manual wrappers must have separate groups outside deploy-staging")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"drill", "proof", "alert"}:
        errors.append(f"{name}: jobs must be exactly drill, proof and alert")
        return errors
    errors.extend(rollback_proof_job_errors(jobs["proof"], "run"))
    errors.extend(rollback_proof_job_errors(jobs["alert"], "alert"))
    if jobs["proof"].get("if") != "github.ref == 'refs/heads/main' && inputs.target_version == ''":
        errors.append(f"{name}: proof must run on main with no manual target")
    if jobs["alert"].get("needs") != "proof" or jobs["alert"].get("if") != "${{ !cancelled() && needs.proof.result == 'failure' }}":
        errors.append(f"{name}: alert must run on a failed proof only")
    job = jobs["drill"]
    if job.get("concurrency") != {"group": "deploy-staging", "cancel-in-progress": "false"}:
        errors.append(f"{name}:drill: must share deploy-staging without cancelling")
    if job.get("environment") != "staging":
        errors.append(f"{name}:drill: must stay scoped to the staging environment")
    if job.get("if") != ("github.ref == 'refs/heads/main' && github.event_name == 'workflow_dispatch' "
                         "&& inputs.target_version != ''"):
        errors.append(f"{name}:drill: must run only from main with a manual target")
    if not runner_allowed("drill", job.get("runs-on")):
        errors.append(f"{name}:drill: must use the routed runner expression for job 'drill'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:drill: must not set {key} (bindings stay on the single Run step)")
    text = str(job).lower()
    for marker in ("wrangler", "force", "production", "toJSON(secrets".lower(), "secrets["):
        if marker in text.replace(" ", ""):
            errors.append(f"{name}:drill: must not contain {marker!r} (script-only, unforced, staging-only)")
    steps = job.get("steps") or []
    if len(steps) != 4 or any(key in step for step in steps[:3] for key in ("if", "continue-on-error")):
        errors.append(f"{name}:drill: must run checkout, setup-node, drill and evidence upload")
        return errors
    checkout, node, run, upload = steps
    if (upload.get("if") != "always()" or "continue-on-error" in upload
            or not str(upload.get("uses") or "").startswith("actions/upload-artifact@")
            or (upload.get("with") or {}).get("if-no-files-found") != "error"):
        errors.append(f"{name}:drill: must always upload evidence, failing on missing files")
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false":
        errors.append(f"{name}:drill: checkout must be pinned and must not persist credentials")
    if not str(node.get("uses", "")).startswith("actions/setup-node@"):
        errors.append(f"{name}:drill: second step must be the pinned setup-node")
    command = str(run.get("run", ""))
    if run.get("env") != ROLLBACK_DRILL_ENV:
        errors.append(f"{name}:drill: Run step must bind exactly the four staging bindings plus TARGET_VERSION")
    if ("scripts/staging_rollback_drill.py" not in command or '--target-version "$TARGET_VERSION"' not in command
            or "${{" in command or "uses" in run):
        errors.append(f"{name}:drill: Run step must call the script with the input via the environment only")
    return errors


CONTAINER_DRILL_ENV = {
    "STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}",
    "OWNERSHIP_CONTROL_TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}",
    "CLOUDFLARE_API_TOKEN": "${{ secrets.CLOUDFLARE_API_TOKEN }}",
    "CLOUDFLARE_ACCOUNT_ID": "${{ secrets.CLOUDFLARE_ACCOUNT_ID }}",
    "BACKOUT_SOURCE_SHA": "${{ inputs.backout_source_sha }}",
    "BACKOUT_BUILD_ID": "${{ inputs.backout_build_id }}",
    "BACKOUT_IMAGE": "${{ inputs.backout_image }}",
    "BACKOUT_WORKER_VERSION": "${{ inputs.backout_worker_version }}",
    "BACKOUT_ROLLOUT_ID": "${{ inputs.backout_rollout_id }}",
    "BACKOUT_REVIEW_REF": "${{ inputs.backout_review_ref }}",
    "BACKOUT_STAGING_RUN_ID": "${{ inputs.backout_staging_run_id }}",
    "COMPATIBILITY_NOTE": "${{ inputs.compatibility_note }}",
    "SESSION_ATTESTATION": "${{ inputs.session_attestation }}",
}
# Dispatch input names are the full binding names lowercased
# (BACKOUT_SOURCE_SHA <- inputs.backout_source_sha, and so on).
CONTAINER_DRILL_INPUTS = {key.lower() for key in CONTAINER_DRILL_ENV if key not in
                          ("STAGING_WORKER_URL", "OWNERSHIP_CONTROL_TOKEN",
                           "CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID")}


def staging_container_drill_errors(workflow):
    """Manual staging-only reviewed-image container backout/restore drill.

    Dispatch-only with seven required backout pins, one required compatibility
    note and one optional session attestation. One job runs from main through the `staging`
    environment and shares the deploy-staging concurrency group (never
    cancelling), so a push deploy cannot interleave. Exactly four steps:
    pinned checkout, pinned setup-node, `npm ci` in wrangler/ (the repo-pinned
    wrangler binary the reviewed script drives for the full-container
    deploy), and one Run step that passes every dispatch input through the
    environment (never an inline expression) to the reviewed script. All
    thirteen bindings are scoped to that step alone. No wrangler action, no
    inline deploy command, no force flag: the only deploy route is the
    script's pinned-binary full-container deploy of the validated image pin.
    """
    name = "staging-container-drill.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    dispatch = on.get("workflow_dispatch") or {}
    inputs = dispatch.get("inputs") or {}
    if set(dispatch) != {"inputs"} or set(inputs) != CONTAINER_DRILL_INPUTS:
        errors.append(f"{name}: workflow_dispatch must carry exactly the backout pins, note and attestation")
    else:
        for key, field in inputs.items():
            field = field or {}
            if key == "session_attestation":
                if (field.get("type") != "string" or str(field.get("required")).lower() != "false"
                        or field.get("default") != ""):
                    errors.append(f"{name}: session_attestation must be optional with an empty default")
            elif (field.get("type") != "string" or str(field.get("required")).lower() != "true"
                    or "default" in field):
                errors.append(f"{name}: {key} must be a required string with no default (never latest)")
    if workflow.get("concurrency") != {"group": "deploy-staging", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must share the deploy-staging group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"container-drill"}:
        errors.append(f"{name}: jobs must be exactly container-drill")
        return errors
    job = jobs["container-drill"]
    if job.get("environment") != "staging":
        errors.append(f"{name}:container-drill: must stay scoped to the staging environment")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:container-drill: must run only from main")
    if not runner_allowed("container-drill", job.get("runs-on")):
        errors.append(f"{name}:container-drill: must use the routed runner expression for job 'container-drill'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:container-drill: must not set {key} (bindings stay on the single Run step)")
    steps = job.get("steps") or []
    if len(steps) != 4 or any(key in step for step in steps for key in ("if", "continue-on-error")):
        errors.append(f"{name}:container-drill: must run exactly checkout, setup-node, npm ci and the drill")
        return errors
    checkout, node, install, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false":
        errors.append(f"{name}:container-drill: checkout must be pinned and must not persist credentials")
    if not str(node.get("uses", "")).startswith("actions/setup-node@"):
        errors.append(f"{name}:container-drill: second step must be the pinned setup-node")
    if install.get("run") != "npm ci" or install.get("working-directory") != "wrangler" \
            or "uses" in install:
        errors.append(f"{name}:container-drill: third step must be the repo-pinned wrangler install only")
    command = str(run.get("run", ""))
    if run.get("env") != CONTAINER_DRILL_ENV:
        errors.append(f"{name}:container-drill: Run step must bind exactly the staging bindings plus the pins")
    expected_flags = ['--backout-source-sha "$BACKOUT_SOURCE_SHA"',
                      '--backout-build-id "$BACKOUT_BUILD_ID"',
                      '--backout-image "$BACKOUT_IMAGE"',
                      '--backout-worker-version "$BACKOUT_WORKER_VERSION"',
                      '--backout-rollout-id "$BACKOUT_ROLLOUT_ID"',
                      '--backout-review-ref "$BACKOUT_REVIEW_REF"',
                      '--backout-staging-run-id "$BACKOUT_STAGING_RUN_ID"',
                      '--compatibility-note "$COMPATIBILITY_NOTE"',
                      '--session-attestation "$SESSION_ATTESTATION"']
    if ("scripts/staging_container_drill.py" not in command
            or any(flag not in command for flag in expected_flags)
            or "${{" in command or "uses" in run):
        errors.append(f"{name}:container-drill: Run step must call the script with the inputs via the environment only")
    text = str(steps).lower()
    for marker in ("wrangler-action", "wrangler deploy", "wrangler publish", "force",
                   "production", "tojson(secrets", "secrets["):
        if marker in text.replace(" ", ""):
            errors.append(f"{name}:container-drill: must not contain {marker!r} (script-only, unforced, staging-only)")
    return errors


EVENTS_READ_ENV = {
    "TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL": "${{ secrets.TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL }}",
    "STAGING_EVENTS_READ_EXPECTED_HOST": "${{ secrets.STAGING_EVENTS_READ_EXPECTED_HOST }}",
    "STAGING_FIXTURE_MEMBER_ID": "${{ secrets.STAGING_FIXTURE_MEMBER_ID }}",
    "WINDOW_START": "${{ inputs.window_start }}",
    "WINDOW_END": "${{ inputs.window_end }}",
    "RUN_ID": "${{ github.run_id }}",
}
EVENTS_READ_COMMAND = ('python3 scripts/staging_events_read.py --window-start "$WINDOW_START" '
                       '--window-end "$WINDOW_END" --output "staging-events-read-$RUN_ID.json"')


def staging_events_read_errors(workflow):
    """Manual staging-only read-only events read for the B2 soak.

    Dispatch-only with exactly two required string inputs, the UTC window. The
    fixture member is never an input and never a variable (a step's env block
    prints variable values on the public run page). The read-only database
    login, the independently pinned staging host and the fixture member are
    environment secrets. One job runs only from main
    through the `staging-events-read` environment, on the routed runner for job
    `read`, with its own non-cancelling concurrency group and a 5 minute
    timeout. Exactly three unconditional steps: pinned checkout without
    persisted credentials, one Run step that passes the inputs through the
    environment (never an inline expression) to the reviewed script, and the
    14 day artifact upload of the sanitized file. All three environment
    secrets are scoped to that Run step alone. No wrangler, no
    production path, no `set -x`.
    """
    name = "staging-events-read.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent; secrets belong only on the Run step")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    dispatch = on.get("workflow_dispatch") or {}
    inputs = dispatch.get("inputs") or {}
    if set(dispatch) != {"inputs"} or set(inputs) != {"window_start", "window_end"}:
        errors.append(f"{name}: workflow_dispatch must carry exactly window_start and window_end")
    else:
        for key, field in inputs.items():
            field = field or {}
            if (field.get("type") != "string" or str(field.get("required")).lower() != "true"
                    or "default" in field):
                errors.append(f"{name}: {key} must be a required string with no default")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "staging-events-read", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own staging-events-read group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"read"}:
        errors.append(f"{name}: jobs must be exactly read")
        return errors
    job = jobs["read"]
    if job.get("environment") != "staging-events-read":
        errors.append(f"{name}:read: must read the staging-events-read Environment bindings")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:read: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:read: must keep contents:read only")
    if job.get("timeout-minutes") != "5":
        errors.append(f"{name}:read: timeout must stay 5 minutes")
    if not runner_allowed("read", job.get("runs-on")):
        errors.append(f"{name}:read: must use the routed runner expression for job 'read'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:read: must not set {key} (bindings stay on the single Run step)")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler", "production", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "vars.staging_fixture_member_id", "inputs.member", "inputs.fixture"):
        if marker in text:
            errors.append(f"{name}:read: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"two_bot_staging_events_ro_database_url", "staging_events_read_expected_host",
                   "staging_fixture_member_id"}:
        errors.append(f"{name}:read: must read exactly the three events-read secrets")
    if set(re.findall(r"inputs\.([a-z0-9_]+)", text)) != {"window_start", "window_end"}:
        errors.append(f"{name}:read: may reference only the two window inputs")
    steps = job.get("steps") or []
    if len(steps) != 3 or any(key in step for step in steps for key in ("if", "continue-on-error")):
        errors.append(f"{name}:read: must run exactly checkout, the read and the upload, unconditionally")
        return errors
    checkout, run, upload = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false":
        errors.append(f"{name}:read: checkout must be pinned and must not persist credentials")
    if run.get("env") != EVENTS_READ_ENV:
        errors.append(f"{name}:read: Run step must bind exactly the three secrets, the two windows and the run id")
    if run.get("run") != EVENTS_READ_COMMAND or "uses" in run or "shell" in run:
        errors.append(f"{name}:read: Run step must call the script with the inputs via the environment only")
    if not str(upload.get("uses", "")).startswith("actions/upload-artifact@") or upload.get("with") != {
            "name": "staging-events-read-${{ github.run_id }}",
            "path": "staging-events-read-${{ github.run_id }}.json",
            "if-no-files-found": "error", "retention-days": "14"}:
        errors.append(f"{name}:read: must upload only the run's json for 14 days and fail if absent")
    return errors


PRIVACY_VERIFY_READ_ENV = {
    "TWO_BOT_STAGING_PRIVACY_RO_DATABASE_URL": "${{ secrets.TWO_BOT_STAGING_PRIVACY_RO_DATABASE_URL }}",
    "STAGING_PRIVACY_VERIFY_EXPECTED_HOST": "${{ secrets.STAGING_PRIVACY_VERIFY_EXPECTED_HOST }}",
    "STAGING_FIXTURE_MEMBER_ID": "${{ secrets.STAGING_FIXTURE_MEMBER_ID }}",
    "STAGING_PRIVACY_VERIFY_H_GUILD_ID": "${{ secrets.STAGING_PRIVACY_VERIFY_H_GUILD_ID }}",
    "GUILD": "${{ inputs.guild }}",
    "RUN_ID": "${{ github.run_id }}",
}
PRIVACY_VERIFY_READ_COMMAND = ('python3 scripts/staging_privacy_verify_read.py --guild "$GUILD" '
                               '--output "staging-privacy-verify-read-$RUN_ID.json"')


def staging_privacy_verify_read_errors(workflow):
    """Manual staging-only read-only privacy verification read for one fixture pair.

    Dispatch-only with exactly one required choice input, the guild alias
    (`g` or `h`, no default). The fixture member is never an input and never
    a variable (a step's env block prints variable values on the public run
    page); the H guild pin is likewise a secret, never a variable. The
    read-only database login, the independently pinned staging host, the
    fixture member and the H pin are environment secrets. One job runs only
    from main through the `staging-privacy-verify-read` environment, on the
    routed runner for job `read`, with its own non-cancelling concurrency
    group and a 5 minute timeout. Exactly three unconditional steps: pinned
    checkout without persisted credentials, one Run step that passes the
    input through the environment (never an inline expression) to the
    reviewed script, and the 14 day artifact upload of the sanitized file.
    All four environment secrets are scoped to that Run step alone. No
    wrangler, no production path, no `set -x`.
    """
    name = "staging-privacy-verify-read.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent; secrets belong only on the Run step")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    dispatch = on.get("workflow_dispatch") or {}
    inputs = dispatch.get("inputs") or {}
    if set(dispatch) != {"inputs"} or set(inputs) != {"guild"}:
        errors.append(f"{name}: workflow_dispatch must carry exactly the guild alias input")
    else:
        guild = inputs.get("guild") or {}
        if (guild.get("type") != "choice" or set(guild.get("options") or []) != {"g", "h"}
                or str(guild.get("required")).lower() != "true" or "default" in guild):
            errors.append(f"{name}: guild must be a required g/h choice with no default")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "staging-privacy-verify-read", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own staging-privacy-verify-read group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"read"}:
        errors.append(f"{name}: jobs must be exactly read")
        return errors
    job = jobs["read"]
    if job.get("environment") != "staging-privacy-verify-read":
        errors.append(f"{name}:read: must read the staging-privacy-verify-read Environment bindings")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:read: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:read: must keep contents:read only")
    if job.get("timeout-minutes") != "5":
        errors.append(f"{name}:read: timeout must stay 5 minutes")
    if not runner_allowed("read", job.get("runs-on")):
        errors.append(f"{name}:read: must use the routed runner expression for job 'read'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:read: must not set {key} (bindings stay on the single Run step)")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler", "production", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "vars.staging_fixture_member_id", "vars.staging_privacy_verify_h_guild_id",
                   "inputs.member", "inputs.fixture"):
        if marker in text:
            errors.append(f"{name}:read: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"two_bot_staging_privacy_ro_database_url", "staging_privacy_verify_expected_host",
                   "staging_fixture_member_id", "staging_privacy_verify_h_guild_id"}:
        errors.append(f"{name}:read: must read exactly the four privacy-verify secrets")
    if set(re.findall(r"inputs\.([a-z0-9_]+)", text)) != {"guild"}:
        errors.append(f"{name}:read: may reference only the guild input")
    steps = job.get("steps") or []
    if len(steps) != 3 or any(key in step for step in steps for key in ("if", "continue-on-error")):
        errors.append(f"{name}:read: must run exactly checkout, the read and the upload, unconditionally")
        return errors
    checkout, run, upload = steps
    # Checkout defaults to this workflow's immutable head. No ref, repository,
    # path or other override may replace the reviewed code before secrets bind.
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or checkout.get("with") != {"persist-credentials": "false", "fetch-depth": "1"}:
        errors.append(f"{name}:read: checkout must be shallow, non-persisting and use the workflow head")
    if run.get("env") != PRIVACY_VERIFY_READ_ENV:
        errors.append(f"{name}:read: Run step must bind exactly the four secrets, the guild and the run id")
    if run.get("run") != PRIVACY_VERIFY_READ_COMMAND or "uses" in run or "shell" in run:
        errors.append(f"{name}:read: Run step must call the script with the input via the environment only")
    if not str(upload.get("uses", "")).startswith("actions/upload-artifact@") or upload.get("with") != {
            "name": "staging-privacy-verify-read-${{ github.run_id }}",
            "path": "staging-privacy-verify-read-${{ github.run_id }}.json",
            "if-no-files-found": "error", "retention-days": "14"}:
        errors.append(f"{name}:read: must upload only the run's json for 14 days and fail if absent")
    return errors


CENSUS_ENV = {
    "TWO_BOT_STAGING_PLAN_DATABASE_URL": "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}",
    "TWO_BOT_STAGING_HOST": "${{ vars.TWO_BOT_STAGING_HOST }}",
    "TWO_BOT_STAGING_DATABASE": "${{ vars.TWO_BOT_STAGING_DATABASE }}",
    "TWO_BOT_STAGING_BRANCH_ID": "${{ vars.TWO_BOT_STAGING_BRANCH_ID }}",
}
CENSUS_COMMAND = "set -o pipefail\npython3 scripts/staging_retention_census.py\n"


def staging_retention_census_errors(workflow):
    """Manual staging-only read-only event-store retention census.

    Dispatch-only with no inputs. One job runs only from main through the
    existing no-reviewer `staging-migrate-plan` environment (the plan
    binding's login holds only two_bot_migrator_ro), on the routed runner
    for job `census`, with its own non-cancelling concurrency group and a
    30 minute timeout covering the script's single ~900 s in-job sleep.
    Exactly two unconditional steps: pinned checkout without persisted
    credentials, and one bash Run step that calls the reviewed script with
    the secret plus the three non-secret staging pins via the environment
    only. Log only, no artifact upload: counts, digests and timestamps
    never leave the run log and summary. No wrangler, no production path,
    no apply route, no `set -x`.
    """
    name = "staging-retention-census.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent; bindings belong only on the Run step")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    elif on.get("workflow_dispatch") not in (None, {}, ""):
        errors.append(f"{name}: workflow_dispatch must take no inputs")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "staging-retention-census", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own staging-retention-census group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"census"}:
        errors.append(f"{name}: jobs must be exactly census")
        return errors
    job = jobs["census"]
    if job.get("environment") != "staging-migrate-plan":
        errors.append(f"{name}:census: must read the staging-migrate-plan Environment binding")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:census: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:census: must keep contents:read only")
    if job.get("timeout-minutes") != "30":
        errors.append(f"{name}:census: timeout must stay 30 minutes")
    if not runner_allowed("census", job.get("runs-on")):
        errors.append(f"{name}:census: must use the routed runner expression for job 'census'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:census: must not set {key} (bindings stay on the single Run step)")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler", "production", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "inputs.", "migrator_database_url", "--apply", "upload-artifact",
                   "download-artifact", "createextension"):
        if marker in text:
            errors.append(f"{name}:census: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"two_bot_staging_plan_database_url"}:
        errors.append(f"{name}:census: must read exactly the plan-database secret")
    steps = job.get("steps") or []
    if len(steps) != 2 or any(key in step for step in steps for key in ("if", "continue-on-error")):
        errors.append(f"{name}:census: must run exactly checkout and the census, unconditionally")
        return errors
    checkout, run = steps
    # Scoped to the Run step: the routed runner expression names its own
    # CI_OVERFLOW_* vars, which must not count as census pins.
    if set(re.findall(r"vars\.([a-z0-9_]+)", str(run).lower())) != {"two_bot_staging_host",
                                                                   "two_bot_staging_database",
                                                                   "two_bot_staging_branch_id"}:
        errors.append(f"{name}:census: Run step may reference only the three staging pins")
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "1":
        errors.append(f"{name}:census: checkout must be pinned, shallow and must not persist credentials")
    if run.get("env") != CENSUS_ENV:
        errors.append(f"{name}:census: Run step must bind exactly the plan secret and the three pins")
    if run.get("run") != CENSUS_COMMAND or "uses" in run or run.get("shell") != "bash":
        errors.append(f"{name}:census: Run step must call the script over bash with pipefail")
    return errors


FIXTURE_CENSUS_COMMAND = ("set -o pipefail\n"
                          "python3 scripts/staging_retention_census.py fixture-scope\n")


def staging_fixture_census_errors(workflow):
    """Manual staging-only read-only point-in-time fixture-scope census.

    Dispatch-only with no inputs. One job runs only from main through the
    existing no-reviewer `staging-migrate-plan` environment (the plan
    binding's login holds only two_bot_migrator_ro), on the routed runner
    for job `fixture-census`, with its own non-cancelling concurrency group
    and a 10 minute timeout (no in-job sleep: one observation instant).
    Exactly two unconditional steps: pinned checkout without persisted
    credentials, and one bash Run step that calls the reviewed script in
    fixture-scope mode with the secret plus the three non-secret staging
    pins via the environment only. Log only, no artifact upload: counts,
    digests and the observation timestamp never leave the run log and
    summary. No wrangler, no production path, no apply route, no `set -x`,
    no seed or purge route.
    """
    name = "staging-fixture-census.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent; bindings belong only on the Run step")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    elif on.get("workflow_dispatch") not in (None, {}, ""):
        errors.append(f"{name}: workflow_dispatch must take no inputs")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "staging-fixture-census", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own staging-fixture-census group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"fixture-census"}:
        errors.append(f"{name}: jobs must be exactly fixture-census")
        return errors
    job = jobs["fixture-census"]
    if job.get("environment") != "staging-migrate-plan":
        errors.append(f"{name}:fixture-census: must read the staging-migrate-plan Environment binding")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:fixture-census: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:fixture-census: must keep contents:read only")
    if job.get("timeout-minutes") != "10":
        errors.append(f"{name}:fixture-census: timeout must stay 10 minutes")
    if not runner_allowed("fixture-census", job.get("runs-on")):
        errors.append(f"{name}:fixture-census: must use the routed runner expression for job 'fixture-census'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:fixture-census: must not set {key} (bindings stay on the single Run step)")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler", "production", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "inputs.", "migrator_database_url", "--apply", "upload-artifact",
                   "download-artifact", "createextension"):
        if marker in text:
            errors.append(f"{name}:fixture-census: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"two_bot_staging_plan_database_url"}:
        errors.append(f"{name}:fixture-census: must read exactly the plan-database secret")
    steps = job.get("steps") or []
    if len(steps) != 2 or any(key in step for step in steps for key in ("if", "continue-on-error")):
        errors.append(f"{name}:fixture-census: must run exactly checkout and the census, unconditionally")
        return errors
    checkout, run = steps
    # Scoped to the Run step: the routed runner expression names its own
    # CI_OVERFLOW_* vars, which must not count as census pins.
    if set(re.findall(r"vars\.([a-z0-9_]+)", str(run).lower())) != {"two_bot_staging_host",
                                                                   "two_bot_staging_database",
                                                                   "two_bot_staging_branch_id"}:
        errors.append(f"{name}:fixture-census: Run step may reference only the three staging pins")
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "1":
        errors.append(f"{name}:fixture-census: checkout must be pinned, shallow and must not persist credentials")
    if run.get("env") != CENSUS_ENV:
        errors.append(f"{name}:fixture-census: Run step must bind exactly the plan secret and the three pins")
    if run.get("run") != FIXTURE_CENSUS_COMMAND or "uses" in run or run.get("shell") != "bash":
        errors.append(f"{name}:fixture-census: Run step must call the script in fixture-scope mode over bash with pipefail")
    return errors


MCP_PROVEOUT_ENV = {
    "STAGING_MCP_TOKEN": "${{ secrets.STAGING_MCP_TOKEN }}",
    "STAGING_MCP_URL": "https://two-bot-next-staging.5150.workers.dev/mcp",
    "RUN_ID": "${{ github.run_id }}",
}
MCP_PROVEOUT_COMMAND = ('python3 scripts/staging_mcp_proveout.py '
                        '--output "staging-mcp-proveout-$RUN_ID.json"')


def staging_mcp_proveout_errors(workflow):
    """Manual staging-only read-only authenticated MCP prove-out (TOG-23456).

    Dispatch-only with no inputs. The bearer arrives only as the `staging`
    environment secret STAGING_MCP_TOKEN into one Run step (never echoed,
    stored or uploaded; responses carry no credential). One job runs only
    from main through the `staging` environment, on the routed runner for
    job `proveout`, with its own non-cancelling concurrency group and a 10
    minute timeout. Exactly three steps: pinned checkout without persisted
    credentials, one Run step that calls the reviewed stdlib script with
    the bearer via the environment only, and the 14 day artifact upload of
    the sanitized prove-out file with `if: always()` so failures keep
    their evidence. No wrangler, no production path, no staging Worker URL
    variable, no `set -x`.
    """
    name = "staging-mcp-proveout.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent; secrets belong only on the Run step")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    elif on.get("workflow_dispatch") not in (None, {}, ""):
        errors.append(f"{name}: workflow_dispatch must take no inputs")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "staging-mcp-proveout", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own staging-mcp-proveout group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"proveout"}:
        errors.append(f"{name}: jobs must be exactly proveout")
        return errors
    job = jobs["proveout"]
    if job.get("environment") != "staging":
        errors.append(f"{name}:proveout: must read the staging Environment binding for STAGING_MCP_TOKEN")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:proveout: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:proveout: must keep contents:read only")
    if job.get("timeout-minutes") != "10":
        errors.append(f"{name}:proveout: timeout must stay 10 minutes")
    if not runner_allowed("proveout", job.get("runs-on")):
        errors.append(f"{name}:proveout: must use the routed runner expression for job 'proveout'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:proveout: must not set {key} (bindings stay on the single Run step)")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler", "production", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "cloudflare_api_token", "ownership_control", "staging_worker_url",
                   "production_worker_url", "/health", "/readyz", "vars.staging", "inputs."):
        if marker in text:
            errors.append(f"{name}:proveout: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"staging_mcp_token"}:
        errors.append(f"{name}:proveout: must read exactly the STAGING_MCP_TOKEN secret")
    steps = job.get("steps") or []
    if len(steps) != 3 or any("continue-on-error" in step for step in steps):
        errors.append(f"{name}:proveout: must run exactly checkout, the prove-out and the upload, unconditionally")
        return errors
    checkout, run, upload = steps
    if "if" in checkout or "if" in run:
        errors.append(f"{name}:proveout: only the upload may carry a condition (always())")
        return errors
    if upload.get("if") != "always()":
        errors.append(f"{name}:proveout: upload must run with if: always() so failures keep their artifact")
        return errors
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "1":
        errors.append(f"{name}:proveout: checkout must be pinned, shallow and must not persist credentials")
    if run.get("env") != MCP_PROVEOUT_ENV:
        errors.append(f"{name}:proveout: Run step must bind exactly the bearer, the staging URL and the run id")
    if run.get("run") != MCP_PROVEOUT_COMMAND or "uses" in run or "shell" in run:
        errors.append(f"{name}:proveout: Run step must call the reviewed script with the run id via the environment only")
    if not str(upload.get("uses", "")).startswith("actions/upload-artifact@") or upload.get("with") != {
            "name": "staging-mcp-proveout-${{ github.run_id }}",
            "path": "staging-mcp-proveout-${{ github.run_id }}.json",
            "if-no-files-found": "error", "retention-days": "14"}:
        errors.append(f"{name}:proveout: must upload only the run's json for 14 days and fail if absent")
    return errors


OWNERSHIP_STATUS_ENV = {
    "STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}",
    "OWNERSHIP_CONTROL_TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}",
}


def staging_ownership_status_errors(workflow):
    """Manual staging-only read-only ownership status read.

    Dispatch-only with no inputs. One job runs only from main through the
    `staging` environment, on the routed runner for job `status`, with its
    own non-cancelling concurrency group and a 10 minute timeout. Exactly
    four unconditional steps: pinned checkout without persisted credentials,
    pinned setup-node, the ownership-control preflight, and one Run step that
    calls the reviewed status script with the existing staging pair via the
    environment only and validates the allowlisted owner/timestamp output.
    No deploy, drill, takeover, fence, production or Cloudflare route.
    """
    name = "staging-ownership-status.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent; secrets belong only on the Run steps")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    elif on.get("workflow_dispatch") not in (None, {}, ""):
        errors.append(f"{name}: workflow_dispatch must take no inputs")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "staging-ownership-status", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own staging-ownership-status group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"status"}:
        errors.append(f"{name}: jobs must be exactly status")
        return errors
    job = jobs["status"]
    if job.get("name") != "staging ownership status (read-only)":
        errors.append(f"{name}:status: job name must stay read-only")
    if job.get("environment") != "staging":
        errors.append(f"{name}:status: must read only the staging Environment binding")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:status: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:status: must keep contents:read only")
    if job.get("timeout-minutes") != "10":
        errors.append(f"{name}:status: timeout must stay 10 minutes")
    if job.get("defaults") != {"run": {"working-directory": "wrangler"}}:
        errors.append(f"{name}:status: must run steps from the wrangler directory")
    if not runner_allowed("status", job.get("runs-on")):
        errors.append(f"{name}:status: must use the routed runner expression for job 'status'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:status: must not set {key} (bindings stay on the Run steps)")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler-action", "wranglerdeploy", "wranglerpublish", "production",
                   "cloudflare_api_token", "cloudflare_account_id", "tojson(secrets", "secrets[",
                   "set-x", "setx", "xtrace", "takeover", "fence", "transition", "inputs."):
        if marker in text:
            errors.append(f"{name}:status: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"staging_ownership_control_token"}:
        errors.append(f"{name}:status: must read exactly the staging ownership-control secret")
    if "vars.staging_worker_url" not in text:
        errors.append(f"{name}:status: must read the staging Worker URL variable")
    steps = job.get("steps") or []
    if len(steps) != 4 or any(key in step for step in steps for key in ("if", "continue-on-error")):
        errors.append(f"{name}:status: must run exactly checkout, setup-node, preflight and the read, unconditionally")
        return errors
    checkout, node, preflight, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "1":
        errors.append(f"{name}:status: checkout must be pinned, shallow and must not persist credentials")
    if not str(node.get("uses", "")).startswith("actions/setup-node@"):
        errors.append(f"{name}:status: second step must be the pinned setup-node")
    if preflight.get("env") != OWNERSHIP_STATUS_ENV \
            or str(preflight.get("run", "")).strip() != "node scripts/ownership-control.mjs preflight":
        errors.append(f"{name}:status: preflight must bind exactly the staging pair and run the preflight")
    if run.get("env") != OWNERSHIP_STATUS_ENV or "uses" in run:
        errors.append(f"{name}:status: read step must bind exactly the staging pair via the environment only")
    command = str(run.get("run", ""))
    if "node scripts/staging-ownership-status.mjs" not in command or "${{" in command:
        errors.append(f"{name}:status: read step must call the reviewed status script without inline expressions")
    if '{"owner_id","observed_at"}' not in command.replace(" ", ""):
        errors.append(f"{name}:status: read step must validate the allowlisted owner/timestamp output")
    return errors


# Pinned central gradual-promote action (verified 2026-10-18; re-verify HEAD
# before repinning). Every error-rate input stays explicit so a central
# default change cannot silently widen the gate.
GRADUAL_PROOF_ACTION = ("TogetherWeOwn/.github/actions/gradual-promote"
                        "@89c290be20c0044efaf6916f28341c76e75fff06")
GRADUAL_PROOF_WORKER = "two-bot-next-gradual-proof"
GRADUAL_PROOF_WITH = {
    "worker-name": GRADUAL_PROOF_WORKER,
    "version-tag": "${{ inputs.candidate_tag }}",
    "domain": "${{ inputs.domain }}",
    "api-token": "${{ secrets.CLOUDFLARE_API_TOKEN }}",
    "account-id": "${{ secrets.CLOUDFLARE_ACCOUNT_ID }}",
    "allow-indexable": "false",
    "error-rate-gate": "true",
    "error-rate-threshold": "0.05",
    "error-rate-min-requests": "20",
    "error-rate-window-minutes": "10",
    "error-rate-soak-seconds": "60",
}


def gradual_promote_proof_errors(workflow):
    """Manual gradual-promote proof caller (passive until the fixture lands).

    Dispatch-only with exactly the mode, confirm_worker, candidate_tag and
    domain inputs. One job runs only from main through the `staging`
    environment (staging credentials only; production secret names never
    appear), on the routed runner for job `proof`, with its own
    non-cancelling concurrency group and a 30 minute timeout. Exactly five
    steps: pinned checkout, pinned setup-node, the target-lock step (which
    refuses before any Cloudflare call until the synthetic fixture lands),
    the pinned central action with every error-rate input explicit, and the
    always() evidence upload. No wrangler, no production path, no `set -x`.
    """
    name = "gradual-promote-proof.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    else:
        dispatch = on.get("workflow_dispatch") or {}
        inputs = dispatch.get("inputs") or {}
        if set(dispatch) != {"inputs"} or set(inputs) != {"mode", "confirm_worker", "candidate_tag", "domain"}:
            errors.append(f"{name}: workflow_dispatch must carry exactly mode, confirm_worker, candidate_tag and domain")
        else:
            mode = inputs.get("mode") or {}
            if (mode.get("type") != "choice"
                    or set(mode.get("options") or []) != {"success", "rollback"}
                    or mode.get("default") != "success"):
                errors.append(f"{name}: mode must be success/rollback defaulting to success")
            for key in ("confirm_worker", "candidate_tag", "domain"):
                field = inputs.get(key) or {}
                if (field.get("type") != "string"
                        or str(field.get("required")).lower() != "true"
                        or "default" in field):
                    errors.append(f"{name}: {key} must be a required string with no default")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "gradual-promote-proof", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own gradual-promote-proof group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"proof"}:
        errors.append(f"{name}: jobs must be exactly proof")
        return errors
    job = jobs["proof"]
    if job.get("name") != "gradual-promote-proof":
        errors.append(f"{name}:proof: job name must stay gradual-promote-proof")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:proof: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:proof: must keep contents:read only")
    if job.get("environment") != "staging":
        errors.append(f"{name}:proof: must read only the staging Environment binding")
    if job.get("timeout-minutes") != "30":
        errors.append(f"{name}:proof: timeout must stay 30 minutes")
    if not runner_allowed("proof", job.get("runs-on")):
        errors.append(f"{name}:proof: must use the routed runner expression for job 'proof'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:proof: must not set {key}")
    steps = job.get("steps") or []
    if len(steps) != 5:
        errors.append(f"{name}:proof: must run exactly checkout, setup-node, target lock, the pinned action and the upload")
        return errors
    checkout, node, guard, gradual, upload = steps
    for step, label in ((checkout, "checkout"), (node, "setup-node"),
                        (guard, "target-lock"), (gradual, "central action")):
        if "if" in step:
            errors.append(f"{name}:proof: {label} step must not set if (fail closed)")
        if "continue-on-error" in step:
            errors.append(f"{name}:proof: {label} step must not set continue-on-error (fail closed)")
    if "continue-on-error" in upload:
        errors.append(f"{name}:proof: upload step must not set continue-on-error (fail closed)")
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "1":
        errors.append(f"{name}:proof: checkout must be pinned, shallow and must not persist credentials")
    if not str(node.get("uses", "")).startswith("actions/setup-node@"):
        errors.append(f"{name}:proof: second step must be the pinned setup-node")
    guard_env = guard.get("env") or {}
    if guard_env != {"CONFIRM_WORKER": "${{ inputs.confirm_worker }}",
                     "CANDIDATE_TAG": "${{ inputs.candidate_tag }}",
                     "PROOF_DOMAIN": "${{ inputs.domain }}",
                     "RUN_MODE": "${{ inputs.mode }}"}:
        errors.append(f"{name}:proof: target-lock step must bind exactly the four dispatch inputs via env")
    guard_run = str(guard.get("run") or "")
    if guard.get("shell") != "bash" or "set -euo pipefail" not in guard_run:
        errors.append(f"{name}:proof: target-lock step must run fail-closed bash")
    for needle in (GRADUAL_PROOF_WORKER, "passive", "No Cloudflare call",
                     "${#CANDIDATE_TAG}", "-gt 128",
                     "two-bot-next-gradual-proof.*.*.workers.dev",
                     "single-label synthetic host"):
        if needle not in guard_run:
            errors.append(f"{name}:proof: target-lock step must pin {needle}")
    if "${{" in guard_run:
        errors.append(f"{name}:proof: target-lock step must read inputs via the environment only")
    if gradual.get("id") != "gradual":
        errors.append(f"{name}:proof: the central action step must carry id gradual")
    if gradual.get("uses") != GRADUAL_PROOF_ACTION:
        errors.append(f"{name}:proof: must call the pinned central gradual-promote action")
    if (gradual.get("with") or {}) != GRADUAL_PROOF_WITH:
        errors.append(f"{name}:proof: central action inputs must stay pinned with every error-rate input explicit")
    if upload.get("if") != "always()":
        errors.append(f"{name}:proof: upload must run with if: always() so refusals keep their evidence")
        return errors
    if not str(upload.get("uses", "")).startswith("actions/upload-artifact@") or upload.get("with") != {
            "name": "gradual-promote-proof-${{ github.run_id }}-${{ github.run_attempt }}",
            "path": "proof-evidence/",
            "if-no-files-found": "warn", "retention-days": "14"}:
        errors.append(f"{name}:proof: must upload the run evidence artifact for 14 days and warn if absent")
    text = str(job).lower().replace(" ", "")
    for marker in ("production", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "wrangler", "/health", "/readyz"):
        if marker in text:
            errors.append(f"{name}:proof: must not contain {marker!r}")
    # workers.dev appears only as the synthetic host shape in the guard's
    # domain case pattern (accept plus single-label rejection); anywhere else
    # it would be a deploy route.
    if text.count("workers.dev") != 2 \
            or "two-bot-next-gradual-proof.*.workers.dev" not in guard_run \
            or "two-bot-next-gradual-proof.*.*.workers.dev" not in guard_run:
        errors.append(f"{name}:proof: workers.dev may appear only in the guard domain pattern")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"cloudflare_api_token", "cloudflare_account_id"}:
        errors.append(f"{name}:proof: must read only the staging Cloudflare pair")
    return errors


def release_errors(workflow):
    """The release chain fails closed on an empty dispatch, never as a quiet green.

    release.yml is dispatch-only with the dry_run and release_tag inputs
    (per-promote sha tagging is retired), and every job but the verdict is
    input-guarded: an all-default dispatch skips sbom-target, release-sbom
    and attach-sbom and would read green with nothing published. One verdict
    job runs after all three (no input guard, always() so failures still
    judge): it fails the run with an error naming the skipped jobs and why
    when every input sits at its default, fails on any failed or cancelled
    dependency, and passes otherwise, so the dry_run and release_tag
    dispatches behave exactly as before (their designed skips stay green).
    Every real job carries an explicit timeout: the SBOM plumbing
    (sbom-target, attach-sbom) and the verdict 5 minutes.
    """
    name = "release.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    else:
        dispatch = on.get("workflow_dispatch") or {}
        inputs = (dispatch.get("inputs") or {}) if isinstance(dispatch, dict) else {}
        if set(dispatch) != {"inputs"} or set(inputs) != {"dry_run", "release_tag"}:
            errors.append(f"{name}: workflow_dispatch must carry exactly dry_run and release_tag")
        else:
            dry_run = inputs.get("dry_run") or {}
            release_tag = inputs.get("release_tag") or {}
            if dry_run.get("type") != "boolean" or dry_run.get("default") not in (False, "false"):
                errors.append(f"{name}: dry_run must be an opt-in boolean defaulting to false")
            if release_tag.get("type") != "string" or release_tag.get("default") != "":
                errors.append(f"{name}: release_tag must be a string defaulting to empty")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"sbom-target", "release-sbom", "attach-sbom", "verdict"}:
        errors.append(f"{name}: jobs must be exactly sbom-target, release-sbom, attach-sbom and verdict")
        return errors
    for job_id, expected_timeout in (("sbom-target", "5"), ("attach-sbom", "5")):
        if jobs[job_id].get("timeout-minutes") != expected_timeout:
            errors.append(f"{name}:{job_id}: timeout must stay {expected_timeout} minutes")
    verdict = jobs["verdict"]
    if verdict.get("name") != "release verdict (fail closed on empty dispatch)":
        errors.append(f"{name}:verdict: consumer job identity changed")
    if verdict.get("needs") != ["sbom-target", "release-sbom", "attach-sbom"]:
        errors.append(f"{name}:verdict: must run after sbom-target, release-sbom and attach-sbom")
    if verdict.get("if") != "${{ always() }}":
        errors.append(f"{name}:verdict: must run with if: always() so failures still judge")
    if verdict.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:verdict: must keep contents:read only")
    if not runner_allowed("verdict", verdict.get("runs-on")):
        errors.append(f"{name}:verdict: must use the routed runner expression for job 'verdict'")
    if verdict.get("timeout-minutes") != "5":
        errors.append(f"{name}:verdict: timeout must stay 5 minutes")
    for key in ("uses", "environment", "services", "container",
                "continue-on-error", "strategy", "env"):
        if key in verdict:
            errors.append(f"{name}:verdict: must not set {key} (it judges with inputs and results only)")
    if "secrets." in str(verdict):
        errors.append(f"{name}:verdict: must never read a secret")
    steps = verdict.get("steps", [])
    if len(steps) != 1 or any("if" in step or "continue-on-error" in step for step in steps):
        errors.append(f"{name}:verdict: must run exactly one unconditional verdict step")
        return errors
    step = steps[0]
    if step.get("env") != {"DRY_RUN": "${{ inputs.dry_run }}",
                           "RELEASE_TAG": "${{ inputs.release_tag }}",
                           "SBOM_TARGET_RESULT": "${{ needs.sbom-target.result }}",
                           "RELEASE_SBOM_RESULT": "${{ needs.release-sbom.result }}",
                           "ATTACH_SBOM_RESULT": "${{ needs.attach-sbom.result }}"}:
        errors.append(f"{name}:verdict: step must bind exactly the dispatch inputs and job results "
                      "via the environment")
    run = str(step.get("run") or "")
    for needle in ("DRY_RUN", "RELEASE_TAG", "skipped",
                   "failure", "cancelled", "::error::"):
        if needle not in run:
            errors.append(f"{name}:verdict: missing pin {needle}")
    if run.count("sys.exit(1)") != 1:
        errors.append(f"{name}:verdict: must fail exactly once, on an empty dispatch or a failed dependency")
    if "uses" in step or "${{" in run:
        errors.append(f"{name}:verdict: step must use the environment only, never an inline expression")
    return errors


def daily_digest_errors(workflow):
    """daily-digest cuts at most one vYYYY.MDD.P tag a day and alerts when production moved but no tag was made.

    Schedule-only plus manual dispatch: one daily cron at an off-minute (never
    :00 or :30, never bunched with the deploy-production backstop) and a
    dry_run-only dispatch. The digest job resolves the live production SHA
    (/readyz, falling back to the newest recorded successful production
    Deployment: rows whose description is a deploy ID),
    plans and publishes through the shared CalVer action (both steps pinned to
    one full TogetherWeOwn/.github commit) around the daily-digest.cjs
    adapter, and dispatches the SBOM chain for the fresh or existing release. The alert job consumes owed-digest outputs even
    on a failed publication, or a digest job that failed before classification
    with a proven live SHA, and opens the `daily-digest` incident issue with
    the job token (issues: write), no environment or secret.
    """
    name = "daily-digest.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"schedule", "workflow_dispatch"}:
        errors.append(f"{name}: must run on a daily schedule or manual dispatch only")
    else:
        schedules = on.get("schedule") or []
        if len(schedules) != 1 or not isinstance(schedules[0], dict):
            errors.append(f"{name}: must carry exactly one daily schedule")
        else:
            cron = schedules[0].get("cron", "")
            parts = str(cron).split()
            if len(parts) != 5 or parts[1] == "*" or parts[2] != "*" or parts[3] != "*":
                errors.append(f"{name}: schedule must be daily, not {cron!r}")
            elif parts[0] in ("0", "30", "*/5", "*"):
                errors.append(f"{name}: schedule must use an off-minute, not {parts[0]!r}")
            if cron == "42 7 * * *":
                errors.append(f"{name}: schedule must not bunch with the deploy-production backstop")
        dispatch = on.get("workflow_dispatch") or {}
        inputs = (dispatch.get("inputs") or {}) if isinstance(dispatch, dict) else {}
        if set(dispatch) != {"inputs"} or set(inputs) != {"dry_run"}:
            errors.append(f"{name}: workflow_dispatch must carry exactly dry_run")
        elif inputs["dry_run"].get("type") != "boolean":
            errors.append(f"{name}: dry_run must be an opt-in boolean defaulting to false")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "daily-digest", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must serialize digests at the top level without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"digest", "alert", "rollback-proof-watch"}:
        errors.append(f"{name}: jobs must be exactly digest, alert and rollback-proof-watch")
        return errors
    watcher = jobs["rollback-proof-watch"]
    errors.extend(rollback_proof_job_errors(watcher, "watch"))
    if ("needs" in watcher or watcher.get("if") != "github.ref == 'refs/heads/main' && inputs.dry_run != true"):
        errors.append(f"{name}: rollback proof watcher must be independent of digest success, never dry-run")
    digest = jobs["digest"]
    if digest.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:digest: must run on main only")
    if digest.get("permissions") != {"contents": "write", "deployments": "read", "actions": "write"}:
        errors.append(f"{name}:digest: must tag the release, read Deployments and dispatch SBOM only")
    if not runner_allowed("digest", digest.get("runs-on")):
        errors.append(f"{name}:digest: must use the routed runner expression for job 'digest'")
    digest_text = str(digest)
    for required in ("PRODUCTION_WORKER_URL", "python3 scripts/digest_resolve.py",
                     "daily-digest.cjs", "gh workflow run release.yml --ref main -f release_tag="):
        if required not in digest_text:
            errors.append(f"{name}:digest: missing pin {required}")
    if "release-on-promote" in digest_text or "vX.Y.Z" in digest_text:
        errors.append(f"{name}:digest: must not reference the retired per-promote tagging")
    calver = [step.get("uses", "") for step in digest.get("steps") or []
              if "actions/calver" in str(step.get("uses", ""))]
    calver_pin = re.compile(r"TogetherWeOwn/\.github/actions/calver@[0-9a-f]{40}")
    if len(calver) != 2 or len(set(calver)) != 1 or not all(calver_pin.fullmatch(u) for u in calver):
        errors.append(f"{name}:digest: plan and publish must use the shared CalVer action "
                      "pinned to one full commit SHA")
    outputs = digest.get("outputs") or {}
    for output in ("released", "tag_name", "prod_changed", "live_sha", "prev_tag"):
        if output not in outputs:
            errors.append(f"{name}:digest: must publish the {output} output for the alert gate")
    alert = jobs["alert"]
    if alert.get("needs") != "digest":
        errors.append(f"{name}:alert: must wait for the digest job")
    expected_alert_if = ("${{ !cancelled() && inputs.dry_run != true && "
                         "((needs.digest.outputs.prod_changed == 'true' && "
                         "needs.digest.outputs.released != 'true') || "
                         "(needs.digest.result == 'failure' && "
                         "needs.digest.outputs.prod_changed == '' && "
                         "needs.digest.outputs.live_sha != '')) }}")
    if alert.get("if") != expected_alert_if:
        errors.append(f"{name}:alert: must consume owed-digest outputs even on failure, "
                      "never on a dry run")
    if alert.get("permissions") != {"contents": "read", "issues": "write"}:
        errors.append(f"{name}:alert: must keep contents:read plus issues:write only")
    if "environment" in alert:
        errors.append(f"{name}:alert: the incident issue sink needs no environment")
    if not runner_allowed("alert", alert.get("runs-on")):
        errors.append(f"{name}:alert: must use the routed runner expression for job 'alert'")
    alert_text = str(alert)
    for required in ("${{ secrets.GITHUB_TOKEN }}", "scripts/digest_alert.py",
                     "needs.digest.outputs.live_sha", "needs.digest.outputs.prev_tag"):
        if required not in alert_text:
            errors.append(f"{name}:alert: missing pin {required}")
    return errors


def production_errors(workflow):
    """deploy-production is the one live route: dispatch, green-staging completion or schedule, chained to staging."""
    name = "deploy-production.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch", "workflow_run", "schedule"}:
        errors.append(f"{name}: production must run on dispatch, staging completion or schedule only")
    else:
        run_trigger = (on.get("workflow_run") or {})
        if (run_trigger.get("workflows") != ["deploy-staging", "check"]
                or run_trigger.get("types") != ["completed"]
                or run_trigger.get("branches") != ["main"]):
            errors.append(f"{name}: workflow_run must be a completed deploy-staging or check run on main only")
        if not isinstance(on.get("schedule"), list) or not on.get("schedule"):
            errors.append(f"{name}: schedule must carry the daily backstop")
    jobs = workflow.get("jobs", {})
    production, guard = jobs.get("production", {}), jobs.get("guard", {})
    soak, recheck = jobs.get("soak", {}), jobs.get("recheck", {})
    needs = production.get("needs")
    needs = [needs] if isinstance(needs, str) else (needs or [])
    expected_production_if = (
        "!cancelled() && needs.guard.result == 'success' && needs.guard.outputs.skip == '' && "
        "(needs.soak.result == 'success' || needs.soak.result == 'skipped') && "
        "((needs.guard.outputs.auto != 'true' && needs.recheck.result == 'skipped') || "
        "(needs.recheck.result == 'success' && needs.recheck.outputs.skip == '')) && "
        "(needs.guard.outputs.mode == 'rollback' || needs.guard.outputs.allow_unstaged == 'true' || "
        "(needs.schema-gate.result == 'success' && needs.roles-gate.result == 'success' && "
        "needs.staging-soak-evidence.result == 'success' && needs.staging-soak-evidence.outputs.skip == ''))"
    )
    if production.get("environment") != "production" \
            or set(needs) != {"guard", "soak", "recheck", "schema-gate", "roles-gate", "staging-soak-evidence"}:
        errors.append(f"{name}:production: must wait for the guard, the soak, the recheck, "
                      "the schema gate, the roles gate, the staging soak evidence and the protected Environment")
    if production.get("if") != expected_production_if:
        errors.append(f"{name}:production: rollback may bypass skipped gates; deploy requires both gates, "
                      "a completed soak, a fresh recheck and no skip; cancelled runs never deploy")
    # Only production holds the deploy-production lane: the guard, gates,
    # soak and recheck run outside it, so a sleeping automatic promotion can
    # never delay an operator rollback. The lane keeps the default single
    # pending slot, so the recheck must skip an automatic run while an operator
    # dispatch is active instead of entering the lane behind it.
    if "concurrency" in workflow:
        errors.append(f"{name}: must not hold the deploy lane at workflow level")
    if jobs.get("production", {}).get("concurrency") != {"group": "deploy-production",
                                                         "cancel-in-progress": "false"}:
        errors.append(f"{name}:production: must hold the deploy-production lane without cancelling")
    for job_id in ("guard", "schema-gate", "roles-gate", "soak", "staging-soak-evidence", "recheck"):
        if "concurrency" in jobs.get(job_id, {}):
            errors.append(f"{name}:{job_id}: must not hold the deploy lane")
    expected_soak_if = ("needs.guard.outputs.skip == '' && needs.guard.outputs.mode == 'deploy' "
                        "&& needs.guard.outputs.allow_unstaged != 'true' "
                        "&& needs.guard.outputs.soak_seconds != '0'")
    if soak.get("needs") != "guard":
        errors.append(f"{name}:soak: must wait for the guard (it sleeps the guarded SHA's remainder)")
    if soak.get("if") != expected_soak_if:
        errors.append(f"{name}:soak: must sleep only for ordinary non-skipped deploys with soak remaining")
    soak_script = "\n".join(step.get("run", "") for step in soak.get("steps", []))
    for required in ("SOAK_SECONDS - int(time.time() - green_at)", "time.sleep(remaining)"):
        if required not in soak_script:
            errors.append(f"{name}:soak: must recompute the remainder from the staging-green time")
    # The soak job records the soak-baseline counters before it sleeps, so
    # GREEN_AT may live on the sleep step rather than the first step.
    soak_env = "\n".join(str(step.get("env", "")) for step in soak.get("steps", []))
    if "GREEN_AT" not in soak_env or "needs.guard.outputs.green_at" not in soak_env:
        errors.append(f"{name}:soak: must read the staging-green timestamp from the guard")
    expected_recheck_if = (
        "!cancelled() && needs.guard.result == 'success' && needs.guard.outputs.skip == '' && "
        "needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.auto == 'true' && "
        "(needs.soak.result == 'success' || needs.soak.result == 'skipped') && "
        "needs.schema-gate.result == 'success' && needs.roles-gate.result == 'success' && "
        "needs.staging-soak-evidence.result == 'success' && needs.staging-soak-evidence.outputs.skip == ''"
    )
    if recheck.get("needs") != ["guard", "soak", "schema-gate", "roles-gate", "staging-soak-evidence"]:
        errors.append(f"{name}:recheck: must wait for the guard, the soak and both gates "
                      "(the fresh read happens after the last long build)")
    if recheck.get("if") != expected_recheck_if:
        errors.append(f"{name}:recheck: must recheck only automatic deploys after the soak and both gates")
    # The regression gate's staging leg reads only the staging scrape bearer,
    # writes no deployment record and holds no lane; its skip never deploys.
    evidence_job = jobs.get("staging-soak-evidence", {})
    if evidence_job.get("needs") != ["guard", "soak"] \
            or evidence_job.get("environment") != {"name": "staging", "deployment": "false"} \
            or evidence_job.get("permissions") != {"contents": "read", "actions": "read"}:
        errors.append(f"{name}:staging-soak-evidence: must follow the soak with contents:read plus "
                      "actions:read and the staging Environment (deployment: false)")
    evidence_text = str(evidence_job)
    if re.findall(r"secrets\.[A-Z_]+", evidence_text) != ["secrets.STAGING_METRICS_SCRAPE_TOKEN"] \
            or "PRODUCTION_" in evidence_text:
        errors.append(f"{name}:staging-soak-evidence: must read only the staging scrape bearer")
    if (evidence_job.get("outputs") or {}) != {"skip": "${{ steps.judge.outputs.skip }}",
                                               "evidence": "${{ steps.judge.outputs.evidence }}"}:
        errors.append(f"{name}:staging-soak-evidence: must publish the skip and evidence outputs")
    if (recheck.get("outputs") or {}).get("skip") != "${{ steps.recheck.outputs.skip }}":
        errors.append(f"{name}:recheck: must publish the skip output for production")
    recheck_text = str(recheck)
    for required in ("vars.PRODUCTION_AUTO_PROMOTE_HOLD_UNTIL", "needs.guard.outputs.sha",
                     'skip = "live"', 'skip = "older-than-live"',
                     'skip = "operator-dispatch-active"',
                     'skip = "dispatch-check-unreadable"',
                     "compare/{target}...{live}",
                     "actions/workflows/deploy-production.yml/runs"):
        if required not in recheck_text:
            errors.append(f"{name}:recheck: missing check {required}")
    if (recheck.get("permissions") or {}).get("actions") != "read":
        errors.append(f"{name}:recheck: must read workflow runs to see an active operator dispatch")
    guard_outputs = guard.get("outputs") or {}
    for output in ("auto", "green_at"):
        if output not in guard_outputs:
            errors.append(f"{name}:guard: must publish the {output} output for the soak and recheck jobs")
    expected_gate_if = ("needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true' "
                        "&& needs.guard.outputs.skip == ''")
    for gate_name in ("schema-gate", "roles-gate"):
        if jobs.get(gate_name, {}).get("if") != expected_gate_if:
            errors.append(f"{name}:{gate_name}: must run only for ordinary non-skipped deploys")
    guard_text = str(guard).lower()
    guard_markers = tuple(marker for marker in DEPLOY_MARKERS if marker not in ("production_worker_url", "/readyz"))
    if ("environment" in guard or "uses" in guard or "curl " in guard_text
            or any(marker in guard_text for marker in guard_markers)):
        errors.append(f"{name}:guard: deployment/probe alternative outside the production job")
    guard_script = "\n".join(step.get("run", "") for step in guard.get("steps", []))
    for required in ('urllib.request.Request(',
                     'worker_url + "/readyz"',
                     '"User-Agent": "two-bot-next-production-rollout/1.0"',
                     'urllib.request.urlopen(request, timeout=10)',
                     'live_report.get("build_revision") != sha'):
        if required not in guard_script:
            errors.append(f"{name}:guard: missing live-SHA check {required}")
    if guard_script.count("urllib.request.urlopen(") != 1:
        errors.append(f"{name}:guard: unexpected live production probe")
    if guard_script.count("urllib.request.Request(") != 1:
        errors.append(f"{name}:guard: unexpected live production probe")
    # Lexical regression pins on the guard script. deploy-staging runs
    # execute (TOG-12856), so every production SHA needs a staging deploy
    # run on that SHA that concluded `success` (a `skipped` run does not
    # satisfy the guard).
    script = "\n".join(step.get("run", "") for step in guard.get("steps", []))
    for required in ("actions/workflows/deploy-staging.yml/runs", 'status="success"',
                     'refuse(f"deploy-staging has no successful run on {sha}")',
                     'rule.get("type") == "required_reviewers"',
                     'refuse("the production Environment has no required reviewers")'):
        if required not in script:
            errors.append(f"{name}:guard: missing check {required}")
    # Fault switches are absent from the entire production workflow, not
    # forwarded to a guard that promises to refuse them.
    guard_env = (guard.get("steps") or [{}])[0].get("env", {})
    # A deploy always takes over (10-20 13:31Z: a manual deploy without
    # takeover left production fenced); only the explicit cutover_hold
    # input keeps the fence held.
    inputs = ((on.get("workflow_dispatch") or {}).get("inputs") or {})
    takeover_input = inputs.get("takeover") or {}
    hold_input = inputs.get("cutover_hold") or {}
    if takeover_input.get("type") != "boolean" or takeover_input.get("default") not in (True, "true"):
        errors.append(f"{name}: takeover must be a boolean defaulting to true")
    if hold_input.get("type") != "boolean" or hold_input.get("default") not in (False, "false"):
        errors.append(f"{name}: cutover_hold must be an opt-in boolean defaulting to false")
    if guard_env.get("CUTOVER_HOLD") != "${{ inputs.cutover_hold }}":
        errors.append(f"{name}:guard: must receive cutover_hold as an input env")
    if 'takeover = "false" if cutover_hold == "true" else "true"' not in script:
        errors.append(f"{name}:guard: deploy mode must take over unless cutover_hold")
    # Recovery never waits for a human: a reviewer-less rollback runs only
    # on a verified production rollback snapshot of its exact target.
    for required in ('snapshot_run = verified_rollback_snapshot() if not reviewers and mode == "rollback" else None',
                     'collection(f"repos/{repo}/actions/artifacts"',
                     "target != (version, image, sha)",
                     'receipt.get("provenance_complete") is not True',
                     'PRODUCTION_WORKFLOW = ".github/workflows/deploy-production.yml"',
                     'origin.get("head_repository_id") != origin.get("repository_id")'):
        if required not in script:
            errors.append(f"{name}:guard: missing rollback snapshot check {required!r}")
    return errors


def production_deploy_id_errors(workflow, name="deploy-production.yml"):
    """The production deploy ID is derived in-lane, stamped, gated and recorded.

    The compute step derives `YYYY.MM.DD.N+sha7` from the guarded SHA inside
    the deploy-production lane (N counts today's recorded production deploy
    IDs, so a queued entry cannot mint a duplicate) before the render step stamps it;
    the readyz gate judges it; the record step stores the deploy as a GitHub
    Deployment with environment=production carrying the ID — never a tag or
    Release — with success only when the readyz gate passed and the
    error-rate gate did not fail. It runs after the auto-rollback so
    bookkeeping can never gate recovery. Rollbacks mint a fresh event ID
    and name themselves, since the restored build keeps its original
    stamped ID. No other step may call deploy_id.py.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    production = jobs.get("production") or {}
    steps = production.get("steps") or []

    def find(label, predicate):
        found = [step for step in steps if predicate(step)]
        if len(found) != 1:
            errors.append(f"{name}:production: must carry exactly one {label}")
            return None, -1
        return found[0], steps.index(found[0])

    compute, compute_at = find(
        "deploy-ID compute step",
        lambda step: step.get("name") == "Compute the deploy ID for this production deploy")
    if compute is not None:
        if (compute.get("env") or {}).get("GH_TOKEN") != "${{ github.token }}":
            errors.append(f"{name}:production: compute step must list Deployments with github.token")
        run = str(compute.get("run") or "")
        if ("tooling/scripts/deploy_id.py next --environment production --sha \"$SHA\"" not in run
                or '>> "$GITHUB_ENV"' not in run):
            errors.append(f"{name}:production: compute step must derive the production ID "
                          "from the guarded SHA")
    render, render_at = find(
        "deploy config render step",
        lambda step: step.get("name") == "Render the production deploy config with build identity")
    if render is not None and '--deploy-id "$DEPLOY_ID"' not in str(render.get("run") or ""):
        errors.append(f"{name}:production: render step must stamp the computed deploy ID")
    if compute_at >= 0 and render_at >= 0 and compute_at > render_at:
        errors.append(f"{name}:production: compute step must run before the render step stamps the ID")
    gate, _ = find(
        "readyz gate step",
        lambda step: step.get("name") == "Gate on stable /health + /readyz gateway-ready")
    if gate is not None:
        run = str(gate.get("run") or "")
        # The flag is conditional: real runs judge the computed ID only when
        # the deployed Dockerfile can stamp it, while a caller without an
        # exported ID (the stamping-transition suites run the gate without
        # one) or a pre-stamping guarded SHA judges the legacy shape.
        for required in ('deploy_id_arg=(--deploy-id "$DEPLOY_ID")',
                         '"${deploy_id_arg[@]}"'):
            if required not in run:
                errors.append(f"{name}:production: readyz gate must judge the computed "
                              "deploy ID when one is exported")
    error_gate, error_gate_at = find("promotion error-rate gate step",
                                     lambda step: step.get("id") == "error_rate_gate")
    record, record_at = find(
        "deployment record step",
        lambda step: step.get("name") == "Record the production deployment")
    if record is not None:
        condition = str(record.get("if") or "")
        if ("steps.deploy.outcome == 'success'" not in condition
                or "steps.rollback.outcome == 'success'" not in condition):
            errors.append(f"{name}:production: record step must run whenever a deploy "
                          "or rollback went out")
        env = record.get("env") or {}
        if env.get("GH_TOKEN") != "${{ github.token }}":
            errors.append(f"{name}:production: record step must create the Deployment with github.token")
        if env.get("GATE_OUTCOME") != "${{ steps.gate.outcome }}":
            errors.append(f"{name}:production: record step must judge the readyz gate outcome")
        if env.get("ERROR_GATE_OUTCOME") != "${{ steps.error_rate_gate.outcome }}":
            errors.append(f"{name}:production: record step must judge the error-rate gate outcome")
        run = str(record.get("run") or "")
        if ("tooling/scripts/deploy_id.py record --environment production --sha \"$SHA\"" not in run
                or '--deploy-id "$DEPLOY_ID"' not in run):
            errors.append(f"{name}:production: record step must record environment=production "
                          "for the guarded SHA")
        if '"$MODE" = "rollback"' not in run or "--rollback" not in run:
            errors.append(f"{name}:production: record step must name rollback records")
    if error_gate_at >= 0 and record_at >= 0 and record_at < error_gate_at:
        errors.append(f"{name}:production: record step must run after the error-rate gate judges")
    rollback_at = next((index for index, step in enumerate(steps)
                        if step.get("id") == "auto_rollback"), -1)
    if rollback_at >= 0 and record_at >= 0 and record_at < rollback_at:
        errors.append(f"{name}:production: record step must run after the auto-rollback "
                      "so bookkeeping can never gate recovery")
    if str(production).count("deploy_id.py") != 2:
        errors.append(f"{name}:production: only the compute and record steps may call deploy_id.py")
    return errors


def staging_active_errors(workflow):
    """The deploy job is live (TOG-12856): no static suspension guard.

    The job must carry no `if:` at all — unconditional on both the push/main
    and workflow_dispatch triggers — while keeping the `staging` environment
    scope, the routed runner for job 'deploy', the fenced `release_fence`
    dispatch shape, default-deny top-level permissions and the least-privilege
    per-job grant. A job-level condition would silently skip deploys on some
    SHAs and leave the production guard refusing those SHAs; step-level
    guards are not an equivalent gate, so any job condition fails closed.
    """
    name = "deploy-staging.yml"
    errors = []
    job = (workflow.get("jobs") or {}).get("deploy", {})
    if "if" in job:
        errors.append(f"{name}:deploy: a job condition would silently skip staging deploys")
    if job.get("environment") != "staging":
        errors.append(f"{name}:deploy: must stay scoped to the staging environment")
    if not runner_allowed("deploy", job.get("runs-on")):
        errors.append(f"{name}:deploy: must use the routed runner expression for job 'deploy'")
    for step in job.get("steps", []):
        if step.get("if") == STATIC_FALSE:
            errors.append(f"{name}:deploy: a statically-disabled step would report success without deploying")
            break
    return errors


# Pinned build image shared with the migration plan jobs (same runner
# binary, same toolchain). A digest bump must move all consumers together.
SCHEMA_GATE_IMAGE = ("rust:1.98-trixie@sha256:"
                     "a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546")


def schema_gate_errors(workflow, name="deploy-staging.yml"):
    """Pre-deploy schema-drift gate: read-only plan, fail closed, no new privilege.

    One job runs on every deploy-staging run (no `if:`, `needs: soak-hold` only)
    through the no-reviewer `staging-migrate-plan` environment, builds the
    reviewed `staging-migrate` binary and fails the run with a `schema drift`
    error when `pending_before` is non-empty. The deploy job must `needs` the
    gate, so drift can never deploy silently. Plan-only: no migrator binding,
    no `--apply`, no role/grant route, no wrangler, no secret indexing.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    gate = jobs.get("schema-gate", {})
    if gate.get("environment") != "staging-migrate-plan":
        errors.append(f"{name}:schema-gate: must read only the staging-migrate-plan Environment binding")
    if gate.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:schema-gate: must keep contents:read only")
    if "if" in gate:
        errors.append(f"{name}:schema-gate: a job condition would bypass the stage gate")
    if gate.get("needs") != "soak-hold":
        errors.append(f"{name}:schema-gate: must wait only for the soak hold (it is the wait after the hold)")
    if not runner_allowed("schema-gate", gate.get("runs-on")):
        errors.append(f"{name}:schema-gate: must use the routed runner expression for job 'schema-gate'")
    if gate.get("timeout-minutes") != "30":
        errors.append(f"{name}:schema-gate: timeout must stay 30 minutes")
    if (gate.get("container") or {}).get("image") != SCHEMA_GATE_IMAGE:
        errors.append(f"{name}:schema-gate: must build on the pinned plan-job image")
    deploy = jobs.get("deploy", {})
    needs = deploy.get("needs")
    needs = [needs] if isinstance(needs, str) else (needs or [])
    if "schema-gate" not in needs:
        errors.append(f"{name}:deploy: must needs the schema-gate (drift must block the deploy)")
    steps = gate.get("steps") or []
    if len(steps) != 4:
        errors.append(f"{name}:schema-gate: must run exactly checkout, SHA check, build and the gate")
        return errors
    checkout, sha_check, build, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "0":
        errors.append(f"{name}:schema-gate: checkout must be pinned, credentialless and full-depth")
    if ((sha_check.get("env") or {}).get("SOURCE_SHA") != DEPLOY_SHA_EXPRESSION
            or "git rev-parse HEAD" not in str(sha_check.get("run", ""))):
        errors.append(f"{name}:schema-gate: SHA check must pin the step to the dispatch-or-head SHA")
    if "cargo build -p two-bot-cutover --bin staging-migrate --locked" not in str(build.get("run", "")):
        errors.append(f"{name}:schema-gate: build must compile the reviewed staging-migrate binary locked")
    if (run.get("shell") != "bash" or "set -o pipefail" not in str(run.get("run", ""))
            or "scripts/ci/staging_schema_gate.py" not in str(run.get("run", ""))
            or "--source-sha" not in str(run.get("run", ""))
            or "TWO_BOT_STAGING_PLAN_DATABASE_URL" not in str(run.get("env", ""))
            or (run.get("env") or {}).get("SOURCE_SHA") != DEPLOY_SHA_EXPRESSION):
        errors.append(f"{name}:schema-gate: gate step must run the reviewed script over pipefail "
                      "with the plan binding and the dispatch-or-head SHA")
    # The expected staging identity is pinned independently of the secret
    # binding (vars, never derived from the URL): a wrong-target binding must
    # refuse instead of reading the wrong database.
    env_text = str(run.get("env", ""))
    for pin, var in (("TWO_BOT_STAGING_HOST", "vars.TWO_BOT_STAGING_HOST"),
                     ("TWO_BOT_STAGING_DATABASE", "vars.TWO_BOT_STAGING_DATABASE"),
                     ("TWO_BOT_STAGING_BRANCH_ID", "vars.TWO_BOT_STAGING_BRANCH_ID")):
        if pin not in env_text or var not in env_text:
            errors.append(f"{name}:schema-gate: gate step must pin {pin} from {var}")
    text = str(gate).lower().replace(" ", "")
    for marker in ("wrangler", "--apply", "migrator", "roles", "tojson(secrets", "secrets["):
        if marker in text:
            errors.append(f"{name}:schema-gate: must not contain {marker!r} (plan-only, no apply/grant route)")
    return errors


def production_schema_gate_errors(workflow, name="deploy-production.yml"):
    """Pre-deploy production schema-drift gate: read-only plan, fail closed.

    Ordinary deploys run one post-guard job (it needs the guarded SHA: a
    dispatch SHA is an input, not github.sha) through the no-reviewer
    `production-migrate-plan` environment. The job builds the reviewed
    `staging-migrate` binary and fails with `schema drift` when the guarded
    source is ahead of the production ledger. Rollback and live-SHA takeover
    modes explicitly skip it; the production job accepts those guard-validated
    waivers but otherwise requires this gate to succeed. Plan-only: no
    migrator binding, no `--apply`, no role/grant route, no wrangler, no
    secret indexing.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    gate = jobs.get("schema-gate", {})
    if gate.get("environment") != "production-migrate-plan":
        errors.append(f"{name}:schema-gate: must read only the production-migrate-plan Environment binding")
    if gate.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:schema-gate: must keep contents:read only")
    if gate.get("if") != ("needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true' "
                          "&& needs.guard.outputs.skip == ''"):
        errors.append(f"{name}:gate: must run only for ordinary non-skipped deploys")
    if gate.get("needs") != "guard":
        errors.append(f"{name}:schema-gate: must wait for the guard (it gates the guarded SHA)")
    if not runner_allowed("schema-gate", gate.get("runs-on")):
        errors.append(f"{name}:schema-gate: must use the routed runner expression for job 'schema-gate'")
    if gate.get("timeout-minutes") != "30":
        errors.append(f"{name}:schema-gate: timeout must stay 30 minutes")
    if (gate.get("container") or {}).get("image") != SCHEMA_GATE_IMAGE:
        errors.append(f"{name}:schema-gate: must build on the pinned plan-job image")
    production = jobs.get("production", {})
    needs = production.get("needs")
    needs = [needs] if isinstance(needs, str) else (needs or [])
    if "schema-gate" not in needs or "guard" not in needs:
        errors.append(f"{name}:production: must needs the guard and the schema-gate (drift must block the deploy)")
    steps = gate.get("steps") or []
    if len(steps) != 5:
        errors.append(f"{name}:schema-gate: must run exactly checkout, tooling checkout, SHA check, build and the gate")
        return errors
    checkout, tooling, sha_check, build, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "0" \
            or "needs.guard.outputs.sha" not in str((checkout.get("with") or {}).get("ref", "")):
        errors.append(f"{name}:schema-gate: checkout must be pinned, credentialless, full-depth and on the guarded SHA")
    if not str(tooling.get("uses", "")).startswith("actions/checkout@") \
            or (tooling.get("with") or {}).get("persist-credentials") != "false" \
            or (tooling.get("with") or {}).get("path") != "tooling" \
            or "github.sha" not in str((tooling.get("with") or {}).get("ref", "")):
        errors.append(f"{name}:schema-gate: tooling checkout must be pinned, credentialless and on the workflow commit")
    if ("needs.guard.outputs.sha" not in str(sha_check.get("env", ""))
            or "git rev-parse HEAD" not in str(sha_check.get("run", ""))):
        errors.append(f"{name}:schema-gate: SHA check must pin the step to the guarded SHA")
    if "cargo build -p two-bot-cutover --bin staging-migrate --locked" not in str(build.get("run", "")):
        errors.append(f"{name}:schema-gate: build must compile the reviewed staging-migrate binary locked")
    if (run.get("shell") != "bash" or "set -o pipefail" not in str(run.get("run", ""))
            or "scripts/ci/production_schema_gate.py" not in str(run.get("run", ""))
            or "--source-sha" not in str(run.get("run", ""))
            or "--mode" not in str(run.get("run", ""))
            or "--allow-unstaged" not in str(run.get("run", ""))
            or "TWO_BOT_PRODUCTION_PLAN_DATABASE_URL" not in str(run.get("env", ""))):
        errors.append(f"{name}:schema-gate: gate step must run the reviewed script over pipefail "
                      "with the plan binding, the guarded SHA and the guard-verified waive flags")
    # The expected production identity is pinned independently of the secret
    # binding (vars, never derived from the URL): a wrong-target binding must
    # refuse instead of reading the wrong database. A PlanetScale host also
    # takes the branch pin, exactly as production-migrate does.
    env_text = str(run.get("env", ""))
    for pin, var in (("TWO_BOT_PRODUCTION_HOST", "vars.TWO_BOT_PRODUCTION_HOST"),
                     ("TWO_BOT_PRODUCTION_DATABASE", "vars.TWO_BOT_PRODUCTION_DATABASE"),
                     ("TWO_BOT_PRODUCTION_BRANCH_ID", "vars.TWO_BOT_PRODUCTION_BRANCH_ID")):
        if pin not in env_text or var not in env_text:
            errors.append(f"{name}:schema-gate: gate step must pin {pin} from {var}")
    if "inputs." in str(gate):
        errors.append(f"{name}:schema-gate: must read only guard-verified outputs, never raw inputs")
    text = str(gate).lower().replace(" ", "")
    for marker in ("wrangler", "--apply", "migrator", "roles", "tojson(secrets", "secrets["):
        if marker in text:
            errors.append(f"{name}:schema-gate: must not contain {marker!r} (plan-only, no apply/grant route)")
    return errors


# Markers banned from the roles-gate job text (lowercased, spaces removed).
# `roles` itself is legitimate here (`db roles verify`); instead the ban
# targets the write binding (`migrator_database_url`), the plan-print
# subcommand (`rolesplan`: verify-only, never `db roles plan`), apply and
# wrangler routes, and secret indexing.
ROLES_GATE_MARKERS = ("wrangler", "--apply", "rolesplan", "migrator_database_url",
                      "tojson(secrets", "secrets[")


def roles_gate_errors(workflow, name="deploy-staging.yml"):
    """Pre-deploy role-drift gate: read-only verify, fail closed, no new privilege.

    One job runs on every deploy-staging run (no `if:`, `needs: soak-hold` only)
    through the no-reviewer `staging-migrate-plan` environment, builds the
    reviewed `two-bot` binary and fails the run when `db roles verify` reports
    any finding. The deploy job must `needs` the gate, so drift can never
    deploy silently. Verify-only: no migrator write binding, no `--apply`, no
    `roles plan` subcommand, no wrangler, no secret indexing.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    gate = jobs.get("roles-gate", {})
    if gate.get("environment") != "staging-migrate-plan":
        errors.append(f"{name}:roles-gate: must read only the staging-migrate-plan Environment binding")
    if gate.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:roles-gate: must keep contents:read only")
    if "if" in gate:
        errors.append(f"{name}:roles-gate: a job condition would let drift deploy silently")
    if gate.get("needs") != "soak-hold":
        errors.append(f"{name}:roles-gate: must wait only for the soak hold (it is the wait after the hold)")
    if not runner_allowed("roles-gate", gate.get("runs-on")):
        errors.append(f"{name}:roles-gate: must use the routed runner expression for job 'roles-gate'")
    if gate.get("timeout-minutes") != "30":
        errors.append(f"{name}:roles-gate: timeout must stay 30 minutes")
    if (gate.get("container") or {}).get("image") != SCHEMA_GATE_IMAGE:
        errors.append(f"{name}:roles-gate: must build on the pinned plan-job image")
    deploy = jobs.get("deploy", {})
    needs = deploy.get("needs")
    needs = [needs] if isinstance(needs, str) else (needs or [])
    if "roles-gate" not in needs:
        errors.append(f"{name}:deploy: must needs the roles-gate (drift must block the deploy)")
    steps = gate.get("steps") or []
    if "continue-on-error" in gate:
        errors.append(f"{name}:roles-gate: job must not tolerate failure")
    if any("if" in step or "continue-on-error" in step for step in steps):
        errors.append(f"{name}:roles-gate: gate steps must not skip or tolerate failure")
    if len(steps) != 4:
        errors.append(f"{name}:roles-gate: must run exactly checkout, SHA check, build and the gate")
        return errors
    checkout, sha_check, build, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "0":
        errors.append(f"{name}:roles-gate: checkout must be pinned, credentialless and full-depth")
    if ((sha_check.get("env") or {}).get("SOURCE_SHA") != DEPLOY_SHA_EXPRESSION
            or "git rev-parse HEAD" not in str(sha_check.get("run", ""))):
        errors.append(f"{name}:roles-gate: SHA check must pin the step to the dispatch-or-head SHA")
    if "cargo build -p two-bot --bin two-bot --locked" not in str(build.get("run", "")):
        errors.append(f"{name}:roles-gate: build must compile the reviewed two-bot binary locked")
    expected_run = """set -o pipefail
python3 scripts/ci/roles_gate.py \\
  --binary ./target/debug/two-bot --target staging \\
  --expected-host "$TWO_BOT_STAGING_HOST" --expected-database "$TWO_BOT_STAGING_DATABASE" \\
  --expected-branch-id "$TWO_BOT_STAGING_BRANCH_ID"
"""
    if (run.get("shell") != "bash" or str(run.get("run", "")).strip() != expected_run.strip()
            or (run.get("env") or {}).get("TWO_DATABASE_URL")
            != "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"):
        errors.append(f"{name}:roles-gate: gate step must pin the exact fail-closed staging command and plan binding")
    # The expected staging identity is pinned independently of the secret
    # binding (vars, never derived from the URL): a wrong-target binding must
    # refuse instead of reading the wrong database.
    env_text = str(run.get("env", ""))
    for pin, var in (("TWO_BOT_STAGING_HOST", "vars.TWO_BOT_STAGING_HOST"),
                     ("TWO_BOT_STAGING_DATABASE", "vars.TWO_BOT_STAGING_DATABASE"),
                     ("TWO_BOT_STAGING_BRANCH_ID", "vars.TWO_BOT_STAGING_BRANCH_ID")):
        if pin not in env_text or var not in env_text:
            errors.append(f"{name}:roles-gate: gate step must pin {pin} from {var}")
    text = str(gate).lower().replace(" ", "")
    for marker in ROLES_GATE_MARKERS:
        if marker in text:
            errors.append(f"{name}:roles-gate: must not contain {marker!r} (verify-only, no apply/grant route)")
    return errors


def production_roles_gate_errors(workflow, name="deploy-production.yml"):
    """Pre-deploy production role-drift gate: read-only verify, fail closed.

    Ordinary deploys run one post-guard job (it needs the guarded SHA: a
    dispatch SHA is an input, not github.sha) through the no-reviewer
    `production-migrate-plan` environment. It builds the reviewed `two-bot`
    binary and fails when `db roles verify` reports any finding. Rollback and
    live-SHA takeover modes explicitly skip it; the production job accepts
    those guard-validated waivers but otherwise requires this gate to succeed.
    Verify-only: no migrator write binding, no `--apply`, no `roles plan`
    subcommand, no wrangler, no secret indexing.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    gate = jobs.get("roles-gate", {})
    if gate.get("environment") != "production-migrate-plan":
        errors.append(f"{name}:roles-gate: must read only the production-migrate-plan Environment binding")
    if gate.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:roles-gate: must keep contents:read only")
    if gate.get("if") != ("needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true' "
                          "&& needs.guard.outputs.skip == ''"):
        errors.append(f"{name}:roles-gate: must run only for ordinary non-skipped deploys")
    if gate.get("needs") != "guard":
        errors.append(f"{name}:roles-gate: must wait for the guard (it gates the guarded SHA)")
    if not runner_allowed("roles-gate", gate.get("runs-on")):
        errors.append(f"{name}:roles-gate: must use the routed runner expression for job 'roles-gate'")
    if gate.get("timeout-minutes") != "30":
        errors.append(f"{name}:roles-gate: timeout must stay 30 minutes")
    if (gate.get("container") or {}).get("image") != SCHEMA_GATE_IMAGE:
        errors.append(f"{name}:roles-gate: must build on the pinned plan-job image")
    production = jobs.get("production", {})
    needs = production.get("needs")
    needs = [needs] if isinstance(needs, str) else (needs or [])
    if "roles-gate" not in needs or "guard" not in needs:
        errors.append(f"{name}:production: must needs the guard and the roles-gate (drift must block the deploy)")
    steps = gate.get("steps") or []
    if "continue-on-error" in gate:
        errors.append(f"{name}:roles-gate: job must not tolerate failure")
    if any("if" in step or "continue-on-error" in step for step in steps):
        errors.append(f"{name}:roles-gate: gate steps must not skip or tolerate failure")
    if len(steps) != 5:
        errors.append(f"{name}:roles-gate: must run exactly checkout, tooling checkout, SHA check, build and the gate")
        return errors
    checkout, tooling, sha_check, build, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "0" \
            or "needs.guard.outputs.sha" not in str((checkout.get("with") or {}).get("ref", "")):
        errors.append(f"{name}:roles-gate: checkout must be pinned, credentialless, full-depth and on the guarded SHA")
    if not str(tooling.get("uses", "")).startswith("actions/checkout@") \
            or (tooling.get("with") or {}).get("persist-credentials") != "false" \
            or (tooling.get("with") or {}).get("path") != "tooling" \
            or "github.sha" not in str((tooling.get("with") or {}).get("ref", "")):
        errors.append(f"{name}:roles-gate: tooling checkout must be pinned, credentialless and on the workflow commit")
    if ("needs.guard.outputs.sha" not in str(sha_check.get("env", ""))
            or "git rev-parse HEAD" not in str(sha_check.get("run", ""))):
        errors.append(f"{name}:roles-gate: SHA check must pin the step to the guarded SHA")
    if "cargo build -p two-bot --bin two-bot --locked" not in str(build.get("run", "")):
        errors.append(f"{name}:roles-gate: build must compile the reviewed two-bot binary locked")
    expected_run = """set -o pipefail
python3 tooling/scripts/ci/roles_gate.py \\
  --binary ./target/debug/two-bot --target production \\
  --expected-host "$TWO_BOT_PRODUCTION_HOST" --expected-database "$TWO_BOT_PRODUCTION_DATABASE" \\
  --expected-branch-id "$TWO_BOT_PRODUCTION_BRANCH_ID" \\
  --mode "$MODE" --allow-unstaged "$ALLOW_UNSTAGED"
"""
    run_env = run.get("env") or {}
    if (run.get("shell") != "bash" or str(run.get("run", "")).strip() != expected_run.strip()
            or run_env.get("TWO_DATABASE_URL")
            != "${{ secrets.TWO_BOT_PRODUCTION_PLAN_DATABASE_URL }}"
            or run_env.get("MODE") != "${{ needs.guard.outputs.mode }}"
            or run_env.get("ALLOW_UNSTAGED") != "${{ needs.guard.outputs.allow_unstaged }}"):
        errors.append(f"{name}:roles-gate: gate step must pin the exact production command and guard-verified values")
    # The expected production identity is pinned independently of the secret
    # binding (vars, never derived from the URL): a wrong-target binding must
    # refuse instead of reading the wrong database. A PlanetScale host also
    # takes the branch pin, exactly as production-migrate does.
    env_text = str(run.get("env", ""))
    for pin, var in (("TWO_BOT_PRODUCTION_HOST", "vars.TWO_BOT_PRODUCTION_HOST"),
                     ("TWO_BOT_PRODUCTION_DATABASE", "vars.TWO_BOT_PRODUCTION_DATABASE"),
                     ("TWO_BOT_PRODUCTION_BRANCH_ID", "vars.TWO_BOT_PRODUCTION_BRANCH_ID")):
        if pin not in env_text or var not in env_text:
            errors.append(f"{name}:roles-gate: gate step must pin {pin} from {var}")
    if "inputs." in str(gate):
        errors.append(f"{name}:roles-gate: must read only guard-verified outputs, never raw inputs")
    text = str(gate).lower().replace(" ", "")
    for marker in ROLES_GATE_MARKERS:
        if marker in text:
            errors.append(f"{name}:roles-gate: must not contain {marker!r} (verify-only, no apply/grant route)")
    return errors


def voice_synthetic_errors(workflow):
    """Live staging voice synthetic (docs/voice-synthetic.md).

    Runs on workflow_call (deploy-staging), a 6-hourly schedule and dispatch,
    only from main, through the `voice-synthetic` environment whose single
    secret is the dedicated test bot token, on the routed runner for job
    `synthetic`, with its own non-cancelling concurrency group and a 10 minute
    timeout. Exactly three steps: pinned checkout without persisted
    credentials, one Run step (the only holder of the token) that runs the
    reviewed script, and the evidence upload. No wrangler, no production path,
    no staging Worker URL, no `set -x`.
    """
    name = "voice-synthetic.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_call", "schedule", "workflow_dispatch"}:
        errors.append(f"{name}: triggers must be exactly workflow_call, schedule and workflow_dispatch")
    call = on.get("workflow_call") or {}
    if on.get("workflow_dispatch") or set(call) - {"secrets"} or (
        call.get("secrets") is not None
        and call["secrets"] != {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": {"required": "false"}}
    ):
        errors.append(
            f"{name}: workflow_call takes only the optional TWO_VOICE_SYNTHETIC_BOT_TOKEN secret; "
            "workflow_dispatch takes no inputs"
        )
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "voice-synthetic", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own voice-synthetic group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"synthetic"}:
        errors.append(f"{name}: jobs must be exactly synthetic")
        return errors
    job = jobs["synthetic"]
    if job.get("environment") != "voice-synthetic":
        errors.append(f"{name}:synthetic: must read only the voice-synthetic Environment")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:synthetic: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:synthetic: must keep contents:read only")
    if job.get("timeout-minutes") != "10":
        errors.append(f"{name}:synthetic: timeout must stay 10 minutes")
    if not runner_allowed("synthetic", job.get("runs-on")):
        errors.append(f"{name}:synthetic: must use the routed runner expression for job 'synthetic'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:synthetic: must not set {key}")
    steps = job.get("steps") or []
    runs = [step for step in steps if "run" in step]
    if len(steps) != 3 or len(runs) != 1 or "scripts/staging_voice_synthetic.py" not in runs[0]["run"]:
        errors.append(f"{name}:synthetic: must be checkout, one Run step of the reviewed script, evidence upload")
    for step in steps:
        if "secrets." in str(step) and step is not (runs[0] if runs else None):
            errors.append(f"{name}:synthetic: only the Run step may hold a secret")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler", "production", "staging_worker_url", "tojson(secrets", "secrets[", "set-x",
                   "setx", "xtrace", "cloudflare_api_token", "ownership_control"):
        if marker in text:
            errors.append(f"{name}:synthetic: must not contain {marker!r}")
    return errors


SYNTHETIC_MISSING_BINDING_RUN = {
    "staging": ('echo "::error::Staging synthetic has zero probe samples because '
                'STAGING_WORKER_URL is unset; refusing to report green"\n'
                'exit 1\n'),
    "prod-passive": ('echo "::error::Production synthetic has zero probe samples because '
                     'PRODUCTION_WORKER_URL is unset; refusing to report green"\n'
                     'exit 1\n'),
}


def synthetic_user_flow_errors(workflow):
    """Five-minute user-flow synthetic (docs/synthetic-user-flow.md).

    Runs on a 5-minute schedule and manual dispatch, only from main, with no
    environment or secret beyond the job token. Two jobs: `staging` runs the reviewed
    staging health-contract probe, `prod-passive` runs the reviewed passive
    production probe (observe and alert only, never a deploy gate). Each job
    has an optional target probe, a fail-closed error when its binding is
    absent (zero samples never read green), an always() evidence upload, an
    always() decide step, and a gated post step.
    The decide step reads recent run history with actions:read, alerts after
    two failures, and writes the event to GITHUB_OUTPUT; the post step runs
    only when an event is due so a no-op is skipped (never success), and only
    a successful post counts as delivered. The post step writes the env's
    `incident` issue with issues:write. No wrangler, no deploy route, no
    `set -x`.
    """
    name = "synthetic-user-flow.yml"
    errors = []
    if "defaults" in workflow:
        errors.append(f"{name}: workflow-level defaults must stay absent (fail-closed shell)")
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent")
    on = workflow.get("on") or {}
    if set(on) != {"schedule", "workflow_dispatch"}:
        errors.append(f"{name}: triggers must be exactly schedule and workflow_dispatch")
    else:
        schedules = on.get("schedule") or []
        if schedules != [{"cron": "*/5 * * * *"}]:
            errors.append(f"{name}: schedule must stay every 5 minutes")
        dispatch = on.get("workflow_dispatch") or {}
        inputs = (dispatch.get("inputs") or {}) if isinstance(dispatch, dict) else {}
        if set(dispatch) != {"inputs"} or set(inputs) != {"force_fail"}:
            errors.append(f"{name}: workflow_dispatch must carry only the force_fail input")
        else:
            field = inputs["force_fail"] or {}
            if field.get("type") != "boolean" or field.get("default") not in (False, "false"):
                errors.append(f"{name}: force_fail must be an opt-in boolean defaulting to false")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "synthetic-user-flow", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own synthetic-user-flow group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"staging", "prod-passive"}:
        errors.append(f"{name}: jobs must be exactly staging and prod-passive")
        return errors
    expected_script = {"staging": "scripts/staging_health_contract_probe.py",
                       "prod-passive": "scripts/prod_passive_probe.py"}
    for job_id in ("staging", "prod-passive"):
        job = jobs[job_id]
        if "environment" in job:
            errors.append(f"{name}:{job_id}: the incident issue sink needs no Environment")
        if job.get("if") != "github.ref == 'refs/heads/main'":
            errors.append(f"{name}:{job_id}: must run only from main")
        if job.get("permissions") != {"actions": "read", "contents": "read", "issues": "write"}:
            errors.append(f"{name}:{job_id}: must keep actions:read, contents:read and issues:write only")
        if job.get("timeout-minutes") != "10":
            errors.append(f"{name}:{job_id}: timeout must stay 10 minutes")
        if not runner_allowed(job_id, job.get("runs-on")):
            errors.append(f"{name}:{job_id}: must use the routed runner expression for job '{job_id}'")
        for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy", "defaults"):
            if key in job:
                errors.append(f"{name}:{job_id}: must not set {key}")
        steps = job.get("steps") or []
        if len(steps) != 6:
            errors.append(f"{name}:{job_id}: must run checkout, probe, notice, upload, decide and alert")
            continue
        checkout, probe_step, notice, upload, decide_step, alert_step = steps
        if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
                or (checkout.get("with") or {}).get("persist-credentials") != "false":
            errors.append(f"{name}:{job_id}: checkout must be pinned and must not persist credentials")
        if expected_script[job_id] not in str(probe_step.get("run", "")) or "uses" in probe_step:
            errors.append(f"{name}:{job_id}: probe step must call {expected_script[job_id]}")
        expected_target = "STAGING_WORKER_URL" if job_id == "staging" else "PRODUCTION_WORKER_URL"
        if probe_step.get("id") != "probe" or expected_target not in str(probe_step.get("if", "")):
            errors.append(f"{name}:{job_id}: probe must be skipped when {expected_target} is unset")
        if notice.get("run") != SYNTHETIC_MISSING_BINDING_RUN[job_id]:
            errors.append(f"{name}:{job_id}: missing-binding notice must run exactly the "
                          "reviewed error and exit 1 command (fail closed)")
        if "shell" in notice:
            errors.append(f"{name}:{job_id}: missing-binding notice must not override shell")
        expected_notice_if = (
            f"steps.probe.outcome == 'skipped' && vars.{expected_target} == ''"
            + (" && !inputs.force_fail" if job_id == "staging" else ""))
        if notice.get("if") != expected_notice_if:
            errors.append(f"{name}:{job_id}: missing-binding notice must keep its probe-skipped gate")
        if "continue-on-error" in notice:
            errors.append(f"{name}:{job_id}: missing-binding notice must not set "
                          "continue-on-error (fail closed)")
        if upload.get("if") != "always()":
            errors.append(f"{name}:{job_id}: upload must run with if: always() so failures keep their artifact")
        if not str(upload.get("uses", "")).startswith("actions/upload-artifact@"):
            errors.append(f"{name}:{job_id}: must upload the run evidence artifact")
        if decide_step.get("if") != "always()":
            errors.append(f"{name}:{job_id}: decide step must run with if: always()")
        if "scripts/synthetic_alert.py" not in str(decide_step.get("run", "")) \
                or "--decide-only" not in str(decide_step.get("run", "")):
            errors.append(f"{name}:{job_id}: decide step must call scripts/synthetic_alert.py --decide-only")
        if decide_step.get("id") != "decide":
            errors.append(f"{name}:{job_id}: decide step must expose id decide for the post gate")
        if alert_step.get("if") != "${{ !cancelled() && steps.decide.outputs.event != '' }}":
            errors.append(f"{name}:{job_id}: post step must run only when steps.decide.outputs.event != ''")
        # Without a status function GitHub adds an implicit success() &&, so
        # a failed probe would skip the post step and no ALERT would go out
        # during an outage. Keep this check even if the exact pin above moves.
        if not any(func in str(alert_step.get("if", ""))
                   for func in ("!cancelled()", "always()", "failure()", "success()")):
            errors.append(f"{name}:{job_id}: post step gate must carry a status function so GitHub does not add implicit success()")
        if "scripts/synthetic_alert.py" not in str(alert_step.get("run", "")) \
                or "--post-only" not in str(alert_step.get("run", "")):
            errors.append(f"{name}:{job_id}: alert step must call scripts/synthetic_alert.py --post-only")
        environment = "staging" if job_id == "staging" else "prod"
        job_name = JOB_NAMES[environment]
        probe_name = PROBE_STEPS[environment]
        alert_name = ALERT_STEPS[environment]
        decide_name = DECIDE_STEPS[environment]
        if job.get("name") != job_name:
            errors.append(f"{name}:{job_id}: job name must match synthetic alert history lookup")
        if probe_step.get("name") != probe_name:
            errors.append(f"{name}:{job_id}: probe step name must match synthetic alert history lookup")
        if decide_step.get("name") != decide_name:
            errors.append(f"{name}:{job_id}: decide step name must match synthetic alert history lookup")
        if alert_step.get("name") != alert_name:
            errors.append(f"{name}:{job_id}: alert step name must match synthetic alert history lookup")
        decide_env = decide_step.get("env") or {}
        if (decide_env.get("GITHUB_TOKEN") != "${{ secrets.GITHUB_TOKEN }}"
                or decide_env.get("PROBE_OUTCOME") != "${{ steps.probe.outcome }}"
                or set(decide_env) - {"GITHUB_TOKEN", "PROBE_OUTCOME", "FORCE_FAIL", "RUN_ID"}
                or "secrets." in str(decide_step.get("run", ""))):
            errors.append(f"{name}:{job_id}: decide step must pass token and outcome via env only")
        alert_env = alert_step.get("env") or {}
        if (alert_env.get("GITHUB_TOKEN") != "${{ secrets.GITHUB_TOKEN }}"
                or alert_env.get("ALERT_EVENT") != "${{ steps.decide.outputs.event }}"
                or "PROBE_OUTCOME" in alert_env
                or "secrets." in str(alert_step.get("run", ""))):
            errors.append(f"{name}:{job_id}: post step must pass token and decided event via env only")
        for step in steps:
            if step not in (decide_step, alert_step) and "secrets." in str(step):
                errors.append(f"{name}:{job_id}: only the decide and alert steps may hold a secret")
    text = str(jobs).lower().replace(" ", "")
    for marker in ("wrangler", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "cloudflare_api_token", "ownership_control"):
        if marker in text:
            errors.append(f"{name}: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"github_token"}:
        errors.append(f"{name}: must read only the job-scoped GitHub token")
    return errors


# Live runs (schedule, or a dispatch left at fixture=live) and fixture proofs
# (dispatched fire/clear) are disjoint jobs, so the staging bearer never
# reaches a proof run.
SLOW_BURN_LIVE_IF = "github.ref == 'refs/heads/main' && (github.event.inputs.fixture || 'live') == 'live'"
SLOW_BURN_FIXTURE_IF = ("github.ref == 'refs/heads/main' && github.event_name == 'workflow_dispatch'"
                        " && (github.event.inputs.fixture == 'fire' || github.event.inputs.fixture == 'clear')")
SLOW_BURN_LIVE_ENV = {
    "STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}",
    "METRICS_SCRAPE_TOKEN": "${{ secrets.STAGING_METRICS_SCRAPE_TOKEN }}",
    "GH_TOKEN": "${{ secrets.GITHUB_TOKEN }}",
    "RING_PATH": "slow-burn-ring.json",
    "COVERAGE_PATH": "slow-burn-coverage.json",
    "TRACKER_DRY_RUN": "${{ github.event.inputs.dry_run }}",
}
SLOW_BURN_FIXTURE_ENV = {
    "GH_TOKEN": "${{ secrets.GITHUB_TOKEN }}",
    "TRACKER_FIXTURE": "${{ github.event.inputs.fixture }}",
    "TRACKER_DRY_RUN": "${{ github.event.inputs.dry_run }}",
}
# Run scripts are pinned exactly: the tracker's argparse is last-wins, so an
# appended --label, --title-prefix or --dry-run would silently override the
# pinned flags (a proof filing a real card, or a live run that never writes).
SLOW_BURN_RUN_TAIL = ('if [ "$TRACKER_DRY_RUN" = "true" ]; then\n'
                      '  args+=(--dry-run)\n'
                      'fi\n'
                      'python3 scripts/slow_burn_tracker.py "${args[@]}"\n')
SLOW_BURN_LIVE_RUN = ('set -euo pipefail\n'
                      'args=(--ring "$RING_PATH" --coverage-path "$COVERAGE_PATH" '
                      '--evidence-out slow-burn-tracker-evidence.json '
                      '--exposition-out slow-burn-exposition.prom)\n' + SLOW_BURN_RUN_TAIL)
SLOW_BURN_FIXTURE_RUN = ('set -euo pipefail\n'
                         'args=(--fixture "$TRACKER_FIXTURE" --label slow-burn-test '
                         "--title-prefix '[slow-burn-test]' --evidence-out slow-burn-tracker-evidence.json "
                         '--exposition-out slow-burn-exposition.prom)\n' + SLOW_BURN_RUN_TAIL)


def slow_burn_tracker_errors(workflow):
    """Creator-join slow-burn card filer (docs/runbook.md#alert-voice-join-slow-burn).

    Runs on a 5-minute schedule and manual dispatch, only from main, as two
    disjoint jobs. `tracker` takes the schedule and live dispatches through
    the existing `staging` environment with `deployment: false` (no
    deployment record per scrape; the main-only branch policy still applies)
    and hands STAGING_METRICS_SCRAPE_TOKEN to the script as
    METRICS_SCRAPE_TOKEN. It restores the cached burn ring and coverage
    sidecar, runs the reviewed scripts/slow_burn_tracker.py (staging scrape
    plus the checked-in slow-burn verdict, then exactly one
    file/refresh/resolve issue write), saves both and uploads the evidence
    receipt. `fixture` takes only dispatched fire/clear proofs: no
    environment, no scrape bearer, no staging URL and no ring cache, filing
    under the test label. The 5-minute cadence is load-bearing for the slow
    long-window baseline. Dispatch takes only the fixture choice and the
    dry-run flag.
    """
    name = "slow-burn-tracker.yml"
    errors = []
    if "defaults" in workflow:
        errors.append(f"{name}: workflow-level defaults must stay absent (fail-closed shell)")
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent")
    on = workflow.get("on") or {}
    if set(on) != {"schedule", "workflow_dispatch"}:
        errors.append(f"{name}: triggers must be exactly schedule and workflow_dispatch")
    else:
        schedules = on.get("schedule") or []
        if schedules != [{"cron": "*/5 * * * *"}]:
            errors.append(f"{name}: schedule must stay every 5 minutes")
        dispatch = on.get("workflow_dispatch") or {}
        inputs = (dispatch.get("inputs") or {}) if isinstance(dispatch, dict) else {}
        if set(dispatch) != {"inputs"} or set(inputs) != {"fixture", "dry_run"}:
            errors.append(f"{name}: workflow_dispatch must carry only the fixture and dry_run inputs")
        else:
            fixture = inputs["fixture"] or {}
            if (fixture.get("type") != "choice"
                    or fixture.get("options") != ["live", "fire", "clear"]
                    or fixture.get("default") != "live"):
                errors.append(f"{name}: fixture must be a live/fire/clear choice defaulting to live")
            dry_run = inputs["dry_run"] or {}
            if dry_run.get("type") != "boolean" or dry_run.get("default") not in (False, "false"):
                errors.append(f"{name}: dry_run must be an opt-in boolean defaulting to false")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "slow-burn-tracker", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own slow-burn-tracker group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"tracker", "fixture"}:
        errors.append(f"{name}: jobs must be exactly tracker and fixture")
        return errors
    shapes = (("tracker", "slow-burn tracker", SLOW_BURN_LIVE_IF, SLOW_BURN_LIVE_RUN, 6, 3),
              ("fixture", "slow-burn fixture proof", SLOW_BURN_FIXTURE_IF, SLOW_BURN_FIXTURE_RUN, 4, 2))
    for job_id, title, condition, command, step_count, run_index in shapes:
        job = jobs[job_id]
        if job.get("name") != title:
            errors.append(f"{name}:{job_id}: job name must stay {title}")
        if job.get("if") != condition:
            errors.append(f"{name}:{job_id}: must keep its main-only {job_id} run condition")
        if job.get("permissions") != {"contents": "read", "issues": "write"}:
            errors.append(f"{name}:{job_id}: must keep contents:read plus issues:write only")
        if job.get("timeout-minutes") != "10":
            errors.append(f"{name}:{job_id}: timeout must stay 10 minutes")
        if not runner_allowed(job_id, job.get("runs-on")):
            errors.append(f"{name}:{job_id}: must use the routed runner expression for job '{job_id}'")
        for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy", "defaults"):
            if key in job:
                errors.append(f"{name}:{job_id}: must not set {key}")
        text = str(job).lower().replace(" ", "")
        for marker in ("wrangler", "production", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                       "cloudflare_api_token", "ownership_control"):
            if marker in text:
                errors.append(f"{name}:{job_id}: must not contain {marker!r}")
        steps = job.get("steps") or []
        if len(steps) != step_count:
            errors.append(f"{name}:{job_id}: must keep exactly {step_count} steps")
            continue
        checkout, setup_node, run_step, upload = steps[0], steps[1], steps[run_index], steps[-1]
        if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
                or checkout.get("with") != {"persist-credentials": "false", "fetch-depth": "1"}:
            errors.append(f"{name}:{job_id}: checkout must be pinned, must not persist credentials, "
                          "and must not set ref or repository")
        if not str(setup_node.get("uses", "")).startswith("actions/setup-node@") \
                or (setup_node.get("with") or {}).get("node-version") != "24":
            errors.append(f"{name}:{job_id}: setup-node must be pinned at node 24 for the verdict driver")
        if run_step.get("run") != command or "uses" in run_step:
            errors.append(f"{name}:{job_id}: run step must run exactly the reviewed tracker command, "
                          "with inputs passed via env only")
        if "if" in run_step:
            errors.append(f"{name}:{job_id}: run step must not set if (fail closed)")
        if "shell" in run_step:
            errors.append(f"{name}:{job_id}: run step must not override shell (fail closed)")
        if "continue-on-error" in run_step:
            errors.append(f"{name}:{job_id}: run step must not set continue-on-error (fail closed)")
        if upload.get("if") != "always()":
            errors.append(f"{name}:{job_id}: upload must run with if: always() so failures keep their artifact")
        if not str(upload.get("uses", "")).startswith("actions/upload-artifact@"):
            errors.append(f"{name}:{job_id}: must upload the run evidence artifact")
        for step in steps:
            if step is not run_step and "secrets." in str(step):
                errors.append(f"{name}:{job_id}: only the run step may hold a secret")

    live = jobs["tracker"]
    # The existing staging Environment holds STAGING_METRICS_SCRAPE_TOKEN.
    # deployment: false skips only the deployment record; the branch policy,
    # reviewers and wait timers still gate the job.
    if live.get("environment") != {"name": "staging", "deployment": "false"}:
        errors.append(f"{name}:tracker: must read the existing staging Environment with deployment: false")
    live_secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", str(live).lower()))
    if live_secrets != {"staging_metrics_scrape_token", "github_token"}:
        errors.append(f"{name}:tracker: must read only the staging scrape bearer and the GitHub token")
    live_steps = live.get("steps") or []
    if len(live_steps) == 6:
        restore, track, save = live_steps[2], live_steps[3], live_steps[4]
        # Split restore/save: a full actions/cache step restores in its main
        # phase, so the save step would overwrite the tracker's fresh ring with
        # the stale cached copy before saving it back.
        restore_with = restore.get("with") or {}
        if (not str(restore.get("uses", "")).startswith("actions/cache/restore@")
                or "slow-burn-ring-v1-" not in str(restore_with.get("key", ""))
                or "slow-burn-ring-v1-" not in str(restore_with.get("restore-keys", ""))):
            errors.append(f"{name}:tracker: ring restore must use actions/cache/restore "
                          "with the rotating key and prefix fallback")
        save_with = save.get("with") or {}
        if (not str(save.get("uses", "")).startswith("actions/cache/save@")
                or "slow-burn-ring-v1-" not in str(save_with.get("key", ""))
                or "restore-keys" in save_with):
            errors.append(f"{name}:tracker: ring save must use actions/cache/save "
                          "with the rotating key and no restore")
        if save.get("if") != "${{ !cancelled() }}":
            errors.append(f"{name}:tracker: ring save must run unless the run was cancelled")
        if track.get("env") != SLOW_BURN_LIVE_ENV:
            errors.append(f"{name}:tracker: track step must pass exactly the staging url, the staging "
                          "scrape bearer, the GitHub token, the cache paths and the dry-run flag via env")

    fixture = jobs["fixture"]
    # Proof runs bind no Environment, so they cannot read the staging bearer.
    if "environment" in fixture:
        errors.append(f"{name}:fixture: proof runs must bind no Environment")
    fixture_text = str(fixture).lower().replace(" ", "")
    for marker in ("staging", "metrics_scrape_token", "actions/cache", "--ring", "--coverage-path"):
        if marker in fixture_text:
            errors.append(f"{name}:fixture: proof runs must not reach {marker!r}")
    if set(re.findall(r"secrets\.([a-z0-9_]+)", fixture_text)) != {"github_token"}:
        errors.append(f"{name}:fixture: must read only the GitHub token")
    fixture_steps = fixture.get("steps") or []
    if len(fixture_steps) == 4 and fixture_steps[2].get("env") != SLOW_BURN_FIXTURE_ENV:
        errors.append(f"{name}:fixture: proof step must take only the GitHub token and the "
                      "dispatch inputs via env")
    return errors


FIVE_MINUTE_SCHEDULERS = ("slow-burn-tracker.yml", "synthetic-user-flow.yml")


def scheduled_binding_audit_errors(workflows):
    """Five-minute watchers must fail closed on missing bindings.

    Both `*/5` schedules exist to watch production health every five
    minutes; an unconfigured watcher that warns and exits green reads as a
    clean bill of health forever. The audit pins the `*/5` inventory (a new
    five-minute watcher needs its fail-closed entry here) and each
    watcher's fail-closed unconfigured path: slow-burn's tracker step
    hands both bindings to scripts/slow_burn_tracker.py, which exits 1
    with an ::error:: naming the unset binding (proved below against the
    reviewed script, and pinned per binding in
    scripts/test_slow_burn_tracker.py), and the handoff step itself
    carries no if or continue-on-error; synthetic's missing-binding
    notice keeps its probe-skipped gate and fails the job with an
    ::error:: naming the unset target var, with no continue-on-error.
    """
    name = "scheduled-binding-audit"
    errors = []
    five_min = sorted(
        path for path, workflow in workflows.items()
        for slot in ((workflow.get("on") or {}).get("schedule") or [])
        if isinstance(slot, dict) and slot.get("cron") == "*/5 * * * *")
    if five_min != sorted(FIVE_MINUTE_SCHEDULERS):
        errors.append(
            f"{name}: */5 schedules must stay exactly "
            f"{', '.join(sorted(FIVE_MINUTE_SCHEDULERS))} (a new five-minute "
            "watcher needs its fail-closed audit entry)")
        return errors
    slow_burn_workflow = workflows.get("slow-burn-tracker.yml") or {}
    if "defaults" in slow_burn_workflow:
        errors.append(f"{name}: slow-burn-tracker.yml: workflow-level defaults must "
                      "stay absent (fail-closed shell)")
    tracker = (slow_burn_workflow.get("jobs") or {}).get("tracker") or {}
    if "defaults" in tracker:
        errors.append(f"{name}: slow-burn-tracker.yml:tracker: job-level defaults "
                      "must stay absent (fail-closed shell)")
    runs = [step for step in tracker.get("steps") or []
            if "scripts/slow_burn_tracker.py" in str(step.get("run", ""))]
    if len(runs) != 1:
        errors.append(f"{name}: slow-burn-tracker.yml: tracker job must invoke "
                      "scripts/slow_burn_tracker.py exactly once")
    else:
        handoff = runs[0].get("env") or {}
        if "STAGING_WORKER_URL" not in handoff or "METRICS_SCRAPE_TOKEN" not in handoff:
            errors.append(f"{name}: slow-burn-tracker.yml: tracker step must hand both "
                          "bindings to the tracker script via env")
        if "if" in runs[0]:
            errors.append(f"{name}: slow-burn-tracker.yml: tracker step must not set "
                          "if (fail closed)")
        if "shell" in runs[0]:
            errors.append(f"{name}: slow-burn-tracker.yml: tracker step must not "
                          "override shell")
        if "continue-on-error" in runs[0]:
            errors.append(f"{name}: slow-burn-tracker.yml: tracker step must not set "
                          "continue-on-error (fail closed)")
    with tempfile.TemporaryDirectory() as tmp:
        evidence = str(Path(tmp) / "evidence.json")
        expo = str(Path(tmp) / "expo.prom")
        with patch("sys.stdout") as stdout:
            code = _TRACKER_MODULE.main(
                ["--evidence-out", evidence, "--exposition-out", expo],
                {"GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next",
                 "GITHUB_RUN_ID": "0",
                 "GITHUB_SERVER_URL": "https://github.com",
                 "GH_TOKEN": "audit-token",
                 "STAGING_WORKER_URL": "", "METRICS_SCRAPE_TOKEN": ""})
        writes = stdout.write.call_args_list
        out = writes[0].args[0] if writes else ""
        if (code == 0 or "::error::" not in out
                or "STAGING_WORKER_URL" not in out
                or "METRICS_SCRAPE_TOKEN" not in out):
            errors.append(f"{name}: slow-burn-tracker.yml: an unconfigured tracker run "
                          "must exit non-zero with an ::error:: naming the unset bindings")
    synthetic_workflow = workflows.get("synthetic-user-flow.yml") or {}
    if "defaults" in synthetic_workflow:
        errors.append(f"{name}: synthetic-user-flow.yml: workflow-level defaults must "
                      "stay absent (fail-closed shell)")
    synthetic = synthetic_workflow.get("jobs") or {}
    targets = (("staging", "STAGING_WORKER_URL"),
               ("prod-passive", "PRODUCTION_WORKER_URL"))
    for job_id, target in targets:
        job = synthetic.get(job_id) or {}
        if "defaults" in job:
            errors.append(f"{name}: synthetic-user-flow.yml:{job_id}: job-level defaults "
                          "must stay absent (fail-closed shell)")
        steps = job.get("steps") or []
        notices = [step for step in steps
                   if str(step.get("name", "")).startswith("Fail closed when the ")]
        if len(notices) != 1:
            errors.append(f"{name}: synthetic-user-flow.yml:{job_id}: must keep its "
                          "missing-binding notice step")
            continue
        if notices[0].get("run") != SYNTHETIC_MISSING_BINDING_RUN[job_id]:
            errors.append(f"{name}: synthetic-user-flow.yml:{job_id}: missing-binding "
                          f"notice must run exactly the reviewed ::error:: naming {target} "
                          "and exit 1 command")
        if "shell" in notices[0]:
            errors.append(f"{name}: synthetic-user-flow.yml:{job_id}: missing-binding "
                          "notice must not override shell")
        notice_if = str(notices[0].get("if", ""))
        if "steps.probe.outcome" not in notice_if or target not in notice_if:
            errors.append(f"{name}: synthetic-user-flow.yml:{job_id}: missing-binding "
                          f"notice must keep its probe-skipped gate on {target}")
        if "continue-on-error" in notices[0]:
            errors.append(f"{name}: synthetic-user-flow.yml:{job_id}: missing-binding "
                          "notice must not set continue-on-error (fail closed)")
    return errors


def workflow_policy_errors(workflows):
    errors = []
    errors.extend(staging_gateway_injection_errors(workflows))
    errors.extend(staging_error_rate_gate_errors(workflows))
    errors.extend(staging_fault_injection_errors(workflows))
    errors.extend(scheduled_binding_audit_errors(workflows))
    # Fail closed on new workflows/jobs, including reusable-workflow alternatives.
    # An intentional addition needs an explicit policy change and independent review.
    if set(workflows) != set(JOB_INVENTORY):
        errors.append("workflow inventory changed")
    for name, workflow in workflows.items():
        jobs = workflow.get("jobs", {})
        if set(jobs) != JOB_INVENTORY.get(name, set()):
            errors.append(f"{name}: job inventory changed")
        if name == "deploy-production.yml":
            errors.extend(production_errors(workflow))
            errors.extend(production_schema_gate_errors(workflow))
            errors.extend(production_roles_gate_errors(workflow))
            errors.extend(production_error_rate_gate_errors(workflow))
            errors.extend(production_deploy_id_errors(workflow))
            continue
        if name == "deploy-staging.yml":
            errors.extend(staging_dispatch_errors(workflow))
            errors.extend(staging_active_errors(workflow))
            errors.extend(staging_pinned_sha_errors(workflows))
            errors.extend(staging_deploy_identity_errors(workflow))
            errors.extend(staging_deploy_id_errors(workflow))
            errors.extend(soak_hold_errors(workflow))
            errors.extend(pinned_history_guard_errors(workflow))
            errors.extend(user_flow_synthetic_errors(workflow))
            errors.extend(schema_gate_errors(workflow))
            errors.extend(roles_gate_errors(workflow))
        if name == "release.yml":
            errors.extend(release_errors(workflow))
        if name == "daily-digest.yml":
            # Pinned shape above; the generic environment/marker scan below
            # would flag its production probe URL and Deployment fallback reads.
            errors.extend(daily_digest_errors(workflow))
            continue
        if name == "voice-synthetic.yml":
            # Pinned shape above; the generic environment scan below would
            # flag its voice-synthetic Environment binding.
            errors.extend(voice_synthetic_errors(workflow))
            continue
        if name == "synthetic-user-flow.yml":
            # Pinned shape above; the generic environment/marker scan below
            # would flag its probe paths.
            errors.extend(synthetic_user_flow_errors(workflow))
            continue
        if name == "slow-burn-tracker.yml":
            # Pinned shape above; the generic environment/marker scan below
            # would flag its staging binding and staging probe URL.
            errors.extend(slow_burn_tracker_errors(workflow))
            continue
        if name == "staging-rollback-drill.yml":
            # Manual drill (TOG-14008): pinned shape above; the generic
            # environment/marker scan below would flag its staging binding.
            errors.extend(staging_rollback_drill_errors(workflow))
            continue
        if name == "staging-container-drill.yml":
            # Manual container drill: pinned shape above; the generic
            # environment/marker scan below would flag its staging binding
            # and the wrangler binary path the reviewed script drives.
            errors.extend(staging_container_drill_errors(workflow))
            continue
        if name == "staging-events-read.yml":
            # Manual read-only events read: pinned shape above; the generic
            # environment scan below would flag its Environment binding.
            errors.extend(staging_events_read_errors(workflow))
            continue
        if name == "staging-privacy-verify-read.yml":
            # Manual read-only privacy verification read: pinned shape above;
            # the generic environment scan below would flag its Environment
            # binding.
            errors.extend(staging_privacy_verify_read_errors(workflow))
            continue
        if name == "staging-retention-census.yml":
            # Manual read-only retention census: pinned shape above; the
            # generic environment scan below would flag its Environment
            # binding.
            errors.extend(staging_retention_census_errors(workflow))
            continue
        if name == "staging-fixture-census.yml":
            # Manual read-only fixture census: pinned shape above; the
            # generic environment scan below would flag its Environment
            # binding.
            errors.extend(staging_fixture_census_errors(workflow))
            continue
        if name == "staging-mcp-proveout.yml":
            # Manual read-only authenticated MCP prove-out: pinned shape
            # above; the generic environment scan below would flag its
            # staging Environment binding.
            errors.extend(staging_mcp_proveout_errors(workflow))
            continue
        if name == "gradual-promote-proof.yml":
            # Manual gradual-promote proof caller (passive until the fixture
            # lands): pinned shape above; the generic environment/marker
            # scan below would flag its staging binding and Cloudflare pair.
            errors.extend(gradual_promote_proof_errors(workflow))
            continue
        if name == "staging-migrate.yml":
            # Manual runner: pinned shape above, not the
            # staging-deploy policy. The generic environment/marker scan
            # below would flag its staging-migrate Environment binding.
            errors.extend(staging_migrate_errors(workflow))
            continue
        if name == "production-migrate.yml":
            # Manual production runner: pinned shape above, not the
            # staging-deploy policy. The generic environment/marker scan
            # below would flag its production-migrate Environment binding.
            errors.extend(production_migrate_errors(workflow))
            continue
        if name == "staging-ownership-status.yml":
            # Manual read-only ownership status read: pinned shape above;
            # the generic environment scan below would flag its staging
            # Environment binding.
            errors.extend(staging_ownership_status_errors(workflow))
            continue
        for job_id, job in jobs.items():
            if name == "deploy-staging.yml" and job_id == "schema-gate":
                continue  # pre-deploy drift gate: pinned by schema_gate_errors above;
                # the generic environment scan below would flag its plan binding
            if name == "deploy-staging.yml" and job_id == "roles-gate":
                continue  # pre-deploy role-drift gate: pinned by roles_gate_errors above;
                # the generic environment scan below would flag its plan binding
            if name == "deploy-staging.yml" and job_id == "deploy":
                continue  # the live deploy route: pinned by the staging checks above
            if name == "deploy-staging.yml" and job_id == "soak-hold":
                continue  # push-deferral gate: pinned by soak_hold_errors above
            if name == "deploy-staging.yml" and job_id == "pinned-history-guard":
                continue  # pinned-drill verdict: pinned by pinned_history_guard_errors above
            if name == "deploy-staging.yml" and job_id == "user-flow-synthetic":
                continue  # read-only exact-SHA probe: pinned by user_flow_synthetic_errors above
            else:
                # Retargeting a known CI job must not create an unapproved
                # deploy/probe route outside the deploy workflows.
                text = str(job).lower()
                uses = job.get("uses")
                if ("environment" in job
                        or (uses is not None and REUSABLE_CALLS.get((name, job_id)) != uses)
                        or any(marker in text for marker in DEPLOY_MARKERS)):
                    errors.append(f"{name}:{job_id}: deployment/probe alternative outside approved deploy workflows")
    return errors


class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.workflows = load_workflows()

    def test_merge_queue_reports_required_checks_without_pr_metadata(self):
        for name in ("check.yml", "supply-chain.yml"):
            workflow = self.workflows[name]
            self.assertEqual(workflow["on"]["merge_group"], {"types": ["checks_requested"]})
            self.assertEqual(workflow["concurrency"]["cancel-in-progress"],
                             "${{ github.event_name == 'pull_request' }}")
        gates = self.workflows["supply-chain.yml"]["jobs"]
        self.assertEqual(gates["pr-lint"]["name"], "pr-lint")
        self.assertEqual(gates["gitleaks"]["name"], "gitleaks")
        self.assertEqual(gates["gitleaks"]["if"], "${{ !cancelled() }}")
        steps = gates["pr-lint"]["steps"]
        for name in ("Resolve PR title/body", "Check title, body, branch and commits"):
            step = next(s for s in steps if s.get("name") == name)
            self.assertEqual(step["if"], "github.event_name != 'merge_group'")
        receipt = next(s for s in steps if s.get("name") == "Accept metadata already linted on the PR")
        self.assertEqual(receipt["if"], "github.event_name == 'merge_group'")
        self.assertIn("echo ", receipt["run"])
        self.assertEqual(self.workflows["check.yml"]["jobs"]["ci-ok"]["name"], "ci-ok")
        # Queue validation never becomes a deployment entry point.
        for name in ("deploy-staging.yml", "deploy-production.yml"):
            self.assertNotIn("merge_group", self.workflows[name]["on"])

    def test_pr_lint_feeds_changed_files_to_the_t0_gate(self):
        # The T0 attestation floor in pr_standards.py needs the PR file list.
        # Pin the step and its wiring so the gate cannot be silently starved.
        # The step runs on PR events only: on push there is no PR number and
        # the numeric guard would fail every main commit.
        steps = self.workflows["supply-chain.yml"]["jobs"]["pr-lint"]["steps"]
        listed = next(s for s in steps if s.get("name") == "List changed files")
        self.assertEqual(listed["if"],
                         "github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch'")
        self.assertIn("pulls/", listed["run"])
        self.assertIn("previous_filename", listed["run"])
        check = next(s for s in steps if s.get("name") == "Check title, body, branch and commits")
        self.assertEqual(check["env"]["CHANGED_FILES_FILE"],
                         "${{ steps.changed.outputs.changed_files_file }}")
        self.assertIn("CHANGED_FILES", check["run"])

    def test_quality_gates_diff_merge_checkout_against_its_first_parent(self):
        steps = self.workflows["check.yml"]["jobs"]["check"]["steps"]
        expected_base = "${{ github.event.merge_group.base_sha || (github.event.pull_request && format('{0}^1', github.sha)) || github.event.before || 'origin/main' }}"
        expected_head = "${{ github.sha }}"
        for name in (
            "Floor guard (HARD, CONSTRAINTS.md floor)",
            "Unwrap in request paths (HARD)",
            "Semgrep Rust rules (HARD)",
        ):
            with self.subTest(name=name):
                step = next(step for step in steps if step.get("name") == name)
                base_key = "FLOOR_BASE" if name.startswith("Floor guard") else "QUALITY_BASE"
                head_key = "FLOOR_HEAD" if name.startswith("Floor guard") else "QUALITY_HEAD"
                self.assertEqual(step["env"][base_key], expected_base)
                self.assertEqual(step["env"][head_key], expected_head)

    def test_rust_tests_timeout_leaves_headroom_for_the_suite(self):
        # Suite-only since the HARD patch-coverage measurement split out of
        # this lane: the combined job outgrew 60 minutes (59m54s on main, one
        # 60m07s mid-measurement kill), while the replay alone measured ~30
        # min of a 42m16s job. 45 minutes keeps comfortable headroom for a
        # cache-cold or slow runner without letting a hung suite burn an hour.
        job = self.workflows["check.yml"]["jobs"]["rust-tests"]
        # BaseLoader keeps scalars as strings; "45" is the 45-minute limit.
        self.assertEqual(job["timeout-minutes"], "45")
        # The measurement lives in the sharded coverage jobs now: no step here
        # may re-merge it into this lane.
        names = [step.get("name") for step in job["steps"]]
        self.assertNotIn("Patch coverage (HARD, 80% of added lines)", names)

    def test_coverage_shard_and_merge_timeouts_cover_their_work(self):
        # The old single coverage job needed a 60-minute ceiling: the llvm-cov
        # replay alone measured ~30 min of a 42m16s job, with one 60m07s
        # mid-measurement kill. Shards carry a subset of the legs plus one
        # instrumented build, so 45 minutes keeps headroom for a cache-cold or
        # slow runner; the merge only downloads, unions and gates, so 15 is
        # plenty. A slow runner reports a coverage verdict, not a job timeout.
        jobs = self.workflows["check.yml"]["jobs"]
        # BaseLoader keeps scalars as strings.
        self.assertEqual(jobs["coverage-shard"]["timeout-minutes"], "45")
        self.assertEqual(jobs["patch-coverage"]["timeout-minutes"], "15")

    def test_rust_lane_cargo_cache_entries_are_pinned(self):
        # The `check` entry is written by `rust-tests` and restored by the two
        # ignored-db lanes, which rebuild the same non-instrumented graph. The
        # coverage shards build a unique instrumented graph under
        # `target/llvm-cov-target` that no other lane produces, so each shard
        # owns a separate writable `coverage-shard-N` entry: restore-only
        # would cold-build the instrumented dependencies on every run after a
        # cache rotation. Two writers must never share one entry (no
        # concurrent save race on `check`).
        jobs = self.workflows["check.yml"]["jobs"]
        # (shared-key, save-if); BaseLoader keeps scalars as strings.
        expected = {
            "check": ("lint", "${{ github.event_name == 'push' }}"),
            "rust-tests": ("check", "${{ github.event_name == 'push' }}"),
            "coverage-shard": ("coverage-shard-${{ matrix.shard_index }}",
                               "${{ github.event_name == 'push' }}"),
            "ignored-db-stores": ("check", "false"),
            "ignored-db-runtime": ("check", "false"),
        }
        writers = []
        for job_id, (shared_key, save_if) in expected.items():
            cache = next(
                (step for step in jobs[job_id]["steps"]
                 if step.get("name") == "Restore cargo cache"),
                None,
            )
            with self.subTest(job=job_id):
                self.assertIsNotNone(cache, "Restore cargo cache step is missing")
                self.assertEqual(cache["with"]["shared-key"], shared_key)
                self.assertEqual(cache["with"]["save-if"], save_if)
            if save_if != "false":
                writers.append((job_id, shared_key))
        seen = set()
        for job_id, shared_key in writers:
            self.assertNotIn(
                shared_key, seen,
                f"{job_id} races another writer on shared-key {shared_key}")
            seen.add(shared_key)

    def test_rust_test_container_pins_disposable_database_service_address(self):
        job = self.workflows["check.yml"]["jobs"]["rust-tests"]
        prerequisites = next(
            step for step in job["steps"]
            if step.get("name") == "Job-container prerequisites (loopback forward to agent-testdb)"
        )
        self.assertEqual(prerequisites.get("shell"), "bash")
        self.assert_loopback_forward_is_pinned(prerequisites["run"])

    def test_coverage_shard_container_pins_disposable_database_service_address(self):
        # Same hardening as rust-tests: the coverage legs run long database
        # suites, and a main run lost service-alias DNS mid-suite. The block
        # stays byte-identical to the rust-tests copy: shared setup, not a
        # forked copy that can drift.
        jobs = self.workflows["check.yml"]["jobs"]
        runs = {}
        for job_id in ("rust-tests", "coverage-shard"):
            prerequisites = next(
                step for step in jobs[job_id]["steps"]
                if step.get("name") == "Job-container prerequisites (loopback forward to agent-testdb)"
            )
            self.assertEqual(prerequisites.get("shell"), "bash")
            runs[job_id] = prerequisites["run"]
        self.assert_loopback_forward_is_pinned(runs["coverage-shard"])
        self.assertEqual(runs["rust-tests"], runs["coverage-shard"],
                         "the lanes' loopback-forward blocks drifted apart")

    def assert_loopback_forward_is_pinned(self, run):
        self.assertIn("set -e", run)
        hosts_tmp = run.index('hosts_tmp="${RUNNER_TEMP:?RUNNER_TEMP must be set}/hosts"')
        resolve = run.index("getent ahostsv4 agent-testdb")
        remove_stale_pin = run.index("sed -E 's/(^|[[:space:]])agent-testdb([[:space:]]|$)/ /g' /etc/hosts > \"$hosts_tmp\"")
        self.assertNotIn("sed -i -E", run)
        self.assertLess(hosts_tmp, remove_stale_pin)
        copy_hosts = run.index('cat "$hosts_tmp" > /etc/hosts')
        cleanup_hosts = run.index('rm -f "$hosts_tmp"')
        self.assertLess(remove_stale_pin, copy_hosts)
        self.assertLess(copy_hosts, cleanup_hosts)
        pin = run.index("printf '%s agent-testdb\\n' \"$service_ip\" >> /etc/hosts")
        verify_pin = run.index('getent ahostsv4 agent-testdb | awk \'NR == 1 { print $1; exit }\'')
        forward = run.index("TCP:agent-testdb:5432")
        ready = run.index("pg_isready -q -h 127.0.0.1")
        self.assertIn("for attempt in $(seq 1 30)", run[:resolve])
        self.assertLess(remove_stale_pin, resolve)
        self.assertLess(resolve, pin)
        self.assertLess(pin, verify_pin)
        mismatch_check = run.index('if [ "$pinned_ip" != "$service_ip" ]; then')
        mismatch_exit = run.index("exit 1", mismatch_check)
        self.assertLess(verify_pin, mismatch_check)
        self.assertLess(mismatch_exit, forward)
        self.assertLess(verify_pin, forward)
        self.assertLess(forward, ready)
        self.assertIn("sleep 1", run)
        fail_closed = run.index('if [ -z "$service_ip" ]; then')
        self.assertLess(fail_closed, pin)
        self.assertLess(run.index("exit 1", fail_closed), pin)
        self.assertIn("[[ \"$service_ip\" =~ ^([0-9]{1,3}\\.){3}[0-9]{1,3}$ ]]", run)
        # A failed lookup must empty the result, not kill the step under pipefail.
        self.assertIn('service_ip="$(getent ahostsv4 agent-testdb 2>/dev/null | awk \'NR == 1 { print $1; exit }\')" || service_ip=""', run)
        self.assertIn('echo "agent-testdb did not resolve to an IPv4 address"', run)

    def assert_wal_reload_ordering_is_pinned(self, run, max_wal="512MB", min_wal="64MB"):
        # Order is load-bearing both ways: the ALTERs must precede the
        # reload (ALTER SYSTEM only writes postgresql.auto.conf, which the
        # reload reads), and the reload must precede the first createdb,
        # since the bootstrap databases are created later in this same step.
        reload = run.index("pg_reload_conf()")
        self.assertLess(
            run.index(f"ALTER SYSTEM SET max_wal_size = '{max_wal}'"),
            reload,
            "max_wal_size ALTER must precede pg_reload_conf()",
        )
        self.assertLess(
            run.index(f"ALTER SYSTEM SET min_wal_size = '{min_wal}'"),
            reload,
            "min_wal_size ALTER must precede pg_reload_conf()",
        )
        self.assertLess(
            reload,
            run.index("createdb "),
            "pg_reload_conf() must precede the first createdb",
        )

    def test_rust_test_postgres_wal_budget_is_bounded(self):
        job = self.workflows["check.yml"]["jobs"]["rust-tests"]
        prepare = next(
            step for step in job["steps"]
            if step.get("name") == "Prepare isolated test databases"
        )
        verify = next(
            step for step in job["steps"]
            if step.get("name") == "Verify disposable Postgres defaults"
        )
        # The coverage replay exhausted 1 GiB even with the old 512 MB soft
        # WAL target. Keep a smaller target without changing durability or
        # the fixture's memory ceiling.
        self.assertIn("ALTER SYSTEM SET max_wal_size = '128MB'", prepare["run"])
        self.assertIn("ALTER SYSTEM SET min_wal_size = '32MB'", prepare["run"])
        self.assertIn("pg_reload_conf()", prepare["run"])
        self.assert_wal_reload_ordering_is_pinned(prepare["run"], "128MB", "32MB")
        self.assertIn("'max_wal_size': '128MB'", verify["run"])
        self.assertIn("'min_wal_size': '32MB'", verify["run"])

    def test_tmpfs_postgres_wal_budget_is_bounded_in_every_lane(self):
        # Run 39209332791: only rust-tests bounded WAL while the ignored lanes
        # ran the PG18 default (max_wal_size 1GB) on the same 1 GiB tmpfs, and
        # the stores lane PANICed with ENOSPC mid-suite. Every lane that mounts
        # the bounded service must carry its own budget and verify it.
        # rust-tests and coverage-shard are tighter because the coverage replay
        # exhausted 1 GiB at 512 MB (run 39360867142); rust-tests keeps the
        # budget the replay needed when it ran there. The other lanes stay at
        # 512/64 MB.
        budgets = {
            "rust-tests": ("128MB", "32MB"),
            "coverage-shard": ("128MB", "32MB"),
            "ignored-db-stores": ("512MB", "64MB"),
            "ignored-db-runtime": ("512MB", "64MB"),
        }
        for job_id, (max_wal, min_wal) in budgets.items():
            with self.subTest(job=job_id):
                job = self.workflows["check.yml"]["jobs"][job_id]
                prepare = next(
                    step for step in job["steps"]
                    if step.get("name") == "Prepare isolated test databases"
                )
                verify = next(
                    step for step in job["steps"]
                    if step.get("name") == "Verify disposable Postgres defaults"
                )
                self.assertIn(
                    f"ALTER SYSTEM SET max_wal_size = '{max_wal}'", prepare["run"]
                )
                self.assertIn(
                    f"ALTER SYSTEM SET min_wal_size = '{min_wal}'", prepare["run"]
                )
                self.assertIn("pg_reload_conf()", prepare["run"])
                # Reload-before-ALTER writes auto.conf but never applies it;
                # the verify probe would then fail online, so pin the order.
                run = prepare["run"]
                self.assertLess(
                    run.index("ALTER SYSTEM SET max_wal_size"),
                    run.index("pg_reload_conf()"),
                )
                self.assertLess(
                    run.index("ALTER SYSTEM SET min_wal_size"),
                    run.index("pg_reload_conf()"),
                )
                self.assertIn(f"'max_wal_size': '{max_wal}'", verify["run"])
                self.assertIn(f"'min_wal_size': '{min_wal}'", verify["run"])

    def test_nextest_lane_bounds_tmpfs_wal_and_reports_headroom(self):
        lane = self.workflows["reusable-rust-nextest.yml"]["jobs"]["nextest"]
        steps = lane["steps"]
        names = [step.get("name") for step in steps]
        prepare = next(
            step for step in steps
            if step.get("name") == "Prepare isolated test databases"
        )
        self.assertIn("ALTER SYSTEM SET max_wal_size = '512MB'", prepare["run"])
        self.assertIn("ALTER SYSTEM SET min_wal_size = '64MB'", prepare["run"])
        self.assertIn("pg_reload_conf()", prepare["run"])
        run = prepare["run"]
        self.assertLess(
            run.index("ALTER SYSTEM SET max_wal_size"),
            run.index("pg_reload_conf()"),
        )
        self.assertLess(
            run.index("ALTER SYSTEM SET min_wal_size"),
            run.index("pg_reload_conf()"),
        )
        # The nextest lane must verify the effective settings before tests run:
        # pg_reload_conf() returning false still exits 0 under ON_ERROR_STOP,
        # and the report only prints pg_settings. Without this probe the lane
        # can run its whole suite on the PG18 default 1GB WAL budget.
        self.assertIn("Verify disposable Postgres defaults", names)
        self.assertEqual(
            names.index("Verify disposable Postgres defaults"),
            names.index("Prepare isolated test databases") + 1,
        )
        verify = next(
            step for step in steps
            if step.get("name") == "Verify disposable Postgres defaults"
        )
        self.assertEqual(verify.get("shell"), "bash")
        for key, value in (
            ("data_directory", "/var/lib/postgresql/18/docker"),
            ("fsync", "on"),
            ("full_page_writes", "on"),
            ("synchronous_commit", "on"),
            ("max_wal_size", "512MB"),
            ("min_wal_size", "64MB"),
        ):
            self.assertIn(f"'{key}': '{value}'", verify["run"])
        self.assertIn("current_setting(", verify["run"])
        # Pin the rejection itself, not just the expected values: deleting the
        # comparison makes the step print whatever it fetches and exit 0.
        self.assertIn("if settings != expected:", verify["run"])
        self.assertIn(
            "raise SystemExit('disposable Postgres data path or durability defaults changed')",
            verify["run"],
        )
        self.assertIn("Report Postgres storage headroom", names)
        report = next(
            step for step in steps
            if step.get("name") == "Report Postgres storage headroom"
        )
        self.assertEqual(
            names.index("Report Postgres storage headroom"),
            names.index("Verify disposable Postgres defaults") + 1,
        )
        self.assertIn("ON_ERROR_STOP=1", report["run"])
        self.assertIn("pg_database_size", report["run"])
        self.assertIn("pg_ls_waldir()", report["run"])
        # Post-suite bracket: max_wal_size is a soft target, so the pre-suite
        # report cannot show peak WAL. The post-suite report always runs after
        # the suite for the next ENOSPC triage.
        self.assertIn("Report Postgres storage headroom (post-suite)", names)
        post = next(
            step for step in steps
            if step.get("name") == "Report Postgres storage headroom (post-suite)"
        )
        self.assertEqual(post.get("if"), "always()")
        self.assertGreater(
            names.index("Report Postgres storage headroom (post-suite)"),
            names.index("cargo nextest (unit and binary, retries=1, JUnit)"),
        )
        self.assertIn("ON_ERROR_STOP=1", post["run"])
        self.assertIn("pg_database_size", post["run"])
        self.assertIn("pg_ls_waldir()", post["run"])

    def test_every_rust_lane_guards_runner_disk_headroom(self):
        lanes = [
            (workflow, job_id)
            for workflow, job_id in (
                ("check.yml", "check"),
                ("check.yml", "rust-tests"),
                ("check.yml", "coverage-shard"),
                ("check.yml", "ignored-db-stores"),
                ("check.yml", "ignored-db-runtime"),
                ("reusable-rust-nextest.yml", "nextest"),
            )
        ]
        self.assertEqual(len(lanes), 6)
        guards = []
        for workflow, job_id in lanes:
            with self.subTest(workflow=workflow, job=job_id):
                steps = self.workflows[workflow]["jobs"][job_id]["steps"]
                names = [step.get("name") for step in steps]
                prereq = next(
                    name for name in names
                    if str(name).startswith("Job-container prerequisites")
                )
                self.assertIn("Guard runner disk headroom", names)
                self.assertEqual(
                    names.index("Guard runner disk headroom"),
                    names.index(prereq) + 1,
                )
                guard = next(
                    step for step in steps
                    if step.get("name") == "Guard runner disk headroom"
                )
                self.assertEqual(guard.get("shell"), "bash")
                run = guard["run"]
                self.assertIn("floor=1073741824", run)
                self.assertIn("::error::runner disk exhausted", run)
                self.assertIn("Infra flake: rerun the job.", run)
                # Pin the rejection path, not just the floor literal: a guard
                # that keeps the floor and error text but stops comparing
                # free_bytes must fail offline.
                self.assertIn('for mount in / "$GITHUB_WORKSPACE"', run)
                self.assertIn("df -B1 --output=avail", run)
                self.assertIn('if [ "$free_bytes" -lt "$floor" ]', run)
                self.assertIn("exit 1", run)
                guards.append((workflow, job_id, run))
        # All six copies stay byte-identical: a single-copy exit-1 removal or
        # -lt/-gt inversion must fail offline instead of silently disabling
        # the only fail-fast protection for that lane.
        bodies = [run for _, _, run in guards]
        self.assertEqual(len(set(bodies)), 1, "the lanes' disk guards drifted apart")
        # Execute each guard with a stubbed df: fail when either mount is below
        # the floor, pass when both are at or above it. Follows the stub
        # pattern in scripts/test_ci_postgres_storage.py.
        floor = 1073741824
        scenarios = (
            ("root low", floor - 1, floor, 1),
            ("workspace low", floor, floor - 1, 1),
            ("both low", floor - 1, floor - 1, 1),
            ("both at floor", floor, floor, 0),
            ("both above", floor + 1, floor + 1, 0),
        )
        stubs = (
            'df() { if [ "$1" = "-h" ]; then return 0; fi; '
            'last="${@: -1}"; '
            'if [ "$last" = "/" ]; then printf \'Avail\\n%s\\n\' "$STUB_ROOT_AVAIL"; '
            'else printf \'Avail\\n%s\\n\' "$STUB_WS_AVAIL"; fi; }; '
            "du() { return 0; }; "
        )
        with tempfile.TemporaryDirectory() as tmp:
            workspace = os.path.join(tmp, "ws")
            runner_temp = os.path.join(tmp, "rt")
            os.makedirs(workspace)
            os.makedirs(runner_temp)
            for workflow, job_id, run in guards:
                for label, root_avail, ws_avail, expected in scenarios:
                    with self.subTest(workflow=workflow, job=job_id, case=label):
                        env = dict(
                            os.environ,
                            GITHUB_WORKSPACE=workspace,
                            RUNNER_TEMP=runner_temp,
                            STUB_ROOT_AVAIL=str(root_avail),
                            STUB_WS_AVAIL=str(ws_avail),
                        )
                        result = subprocess.run(
                            ["bash", "-c", stubs + run],
                            env=env,
                            capture_output=True,
                            text=True,
                            check=False,
                        )
                        self.assertEqual(
                            result.returncode, expected,
                            f"{label}: root={root_avail} ws={ws_avail} "
                            f"stdout={result.stdout!r} stderr={result.stderr!r}",
                        )

    def test_postgres_lanes_report_storage_headroom_post_suite(self):
        # max_wal_size is a soft checkpoint target, not a hard ceiling: the
        # pre-suite report cannot show peak WAL. Every Postgres lane brackets
        # its run with an always-runs post-suite report for ENOSPC triage.
        for job_id in ("rust-tests", "coverage-shard", "ignored-db-stores", "ignored-db-runtime"):
            with self.subTest(job=job_id):
                job = self.workflows["check.yml"]["jobs"][job_id]
                steps = job["steps"]
                names = [step.get("name") for step in steps]
                self.assertIn("Report Postgres storage headroom (post-suite)", names)
                post = next(
                    step for step in steps
                    if step.get("name") == "Report Postgres storage headroom (post-suite)"
                )
                self.assertEqual(post.get("if"), "always()")
                # coverage-shard runs no plain `cargo test` step: its suite is
                # the llvm-cov legs inside the Patch coverage legs step.
                test_steps = [
                    index for index, candidate in enumerate(steps)
                    if "cargo test" in str(candidate.get("run", ""))
                    or "Patch coverage" in str(candidate.get("name", ""))
                ]
                self.assertTrue(test_steps)
                self.assertGreater(steps.index(post), max(test_steps))
                self.assertIn("ON_ERROR_STOP=1", post["run"])
                self.assertIn("pg_database_size", post["run"])
                self.assertIn("pg_ls_waldir()", post["run"])
                self.assertIn("max_wal_size", post["run"])
                self.assertIn("min_wal_size", post["run"])

    def test_stores_container_pins_disposable_database_service_address(self):
        # Same hardening as rust-tests: the stores lane runs ~30 sequential
        # database suites, and a main run lost service-alias DNS mid-suite
        # (EAI_AGAIN on agent-testdb while Postgres PANIC-recovered on ENOSPC).
        job = self.workflows["check.yml"]["jobs"]["ignored-db-stores"]
        prerequisites = next(
            step for step in job["steps"]
            if step.get("name") == "Job-container prerequisites (loopback forward to agent-testdb)"
        )
        self.assertEqual(prerequisites.get("shell"), "bash")
        run = prerequisites["run"]
        self.assertIn("set -e", run)
        hosts_tmp = run.index('hosts_tmp="${RUNNER_TEMP:?RUNNER_TEMP must be set}/hosts"')
        resolve = run.index("getent ahostsv4 agent-testdb")
        remove_stale_pin = run.index("sed -E 's/(^|[[:space:]])agent-testdb([[:space:]]|$)/ /g' /etc/hosts > \"$hosts_tmp\"")
        self.assertNotIn("sed -i -E", run)
        self.assertLess(hosts_tmp, remove_stale_pin)
        copy_hosts = run.index('cat "$hosts_tmp" > /etc/hosts')
        cleanup_hosts = run.index('rm -f "$hosts_tmp"')
        self.assertLess(remove_stale_pin, copy_hosts)
        self.assertLess(copy_hosts, cleanup_hosts)
        pin = run.index("printf '%s agent-testdb\\n' \"$service_ip\" >> /etc/hosts")
        verify_pin = run.index('getent ahostsv4 agent-testdb | awk \'NR == 1 { print $1; exit }\'')
        forward = run.index("TCP:agent-testdb:5432")
        ready = run.index("pg_isready -q -h 127.0.0.1")
        self.assertIn("for attempt in $(seq 1 30)", run[:resolve])
        self.assertLess(remove_stale_pin, resolve)
        self.assertLess(resolve, pin)
        self.assertLess(pin, verify_pin)
        mismatch_check = run.index('if [ "$pinned_ip" != "$service_ip" ]; then')
        mismatch_exit = run.index("exit 1", mismatch_check)
        self.assertLess(verify_pin, mismatch_check)
        self.assertLess(mismatch_exit, forward)
        self.assertLess(verify_pin, forward)
        self.assertLess(forward, ready)
        self.assertIn("sleep 1", run)
        fail_closed = run.index('if [ -z "$service_ip" ]; then')
        self.assertLess(fail_closed, pin)
        self.assertLess(run.index("exit 1", fail_closed), pin)
        self.assertIn("[[ \"$service_ip\" =~ ^([0-9]{1,3}\\.){3}[0-9]{1,3}$ ]]", run)
        # A failed lookup must empty the result, not kill the step under pipefail.
        self.assertIn('service_ip="$(getent ahostsv4 agent-testdb 2>/dev/null | awk \'NR == 1 { print $1; exit }\')" || service_ip=""', run)
        self.assertIn('echo "agent-testdb did not resolve to an IPv4 address"', run)

    def test_stores_postgres_wal_budget_is_bounded(self):
        # Keep the broader stores-lane budget from main; the smaller target is
        # specific to the longer rust-tests coverage replay.
        job = self.workflows["check.yml"]["jobs"]["ignored-db-stores"]
        prepare = next(
            step for step in job["steps"]
            if step.get("name") == "Prepare isolated test databases"
        )
        verify = next(
            step for step in job["steps"]
            if step.get("name") == "Verify disposable Postgres defaults"
        )
        self.assertIn("ALTER SYSTEM SET max_wal_size = '512MB'", prepare["run"])
        self.assertIn("ALTER SYSTEM SET min_wal_size = '64MB'", prepare["run"])
        self.assertIn("pg_reload_conf()", prepare["run"])
        self.assert_wal_reload_ordering_is_pinned(prepare["run"])
        self.assertIn("'max_wal_size': '512MB'", verify["run"])
        self.assertIn("'min_wal_size': '64MB'", verify["run"])

    def test_runtime_container_pins_disposable_database_service_address(self):
        # Same hardening as rust-tests and the stores lane: the runtime lane
        # runs 26 sequential database suites, and a long run must not depend
        # on service-alias DNS staying up (EAI_AGAIN on agent-testdb hit a
        # stores run while Postgres PANIC-recovered on ENOSPC).
        # The runtime WAL cap lives in this lane too (see
        # test_runtime_postgres_wal_budget_is_bounded below).
        job = self.workflows["check.yml"]["jobs"]["ignored-db-runtime"]
        prerequisites = next(
            step for step in job["steps"]
            if step.get("name") == "Job-container prerequisites (loopback forward to agent-testdb)"
        )
        self.assertEqual(prerequisites.get("shell"), "bash")
        run = prerequisites["run"]
        self.assertIn("set -e", run)
        hosts_tmp = run.index('hosts_tmp="${RUNNER_TEMP:?RUNNER_TEMP must be set}/hosts"')
        resolve = run.index("getent ahostsv4 agent-testdb")
        remove_stale_pin = run.index("sed -E 's/(^|[[:space:]])agent-testdb([[:space:]]|$)/ /g' /etc/hosts > \"$hosts_tmp\"")
        self.assertNotIn("sed -i -E", run)
        self.assertLess(hosts_tmp, remove_stale_pin)
        copy_hosts = run.index('cat "$hosts_tmp" > /etc/hosts')
        cleanup_hosts = run.index('rm -f "$hosts_tmp"')
        self.assertLess(remove_stale_pin, copy_hosts)
        self.assertLess(copy_hosts, cleanup_hosts)
        pin = run.index("printf '%s agent-testdb\\n' \"$service_ip\" >> /etc/hosts")
        verify_pin = run.index('getent ahostsv4 agent-testdb | awk \'NR == 1 { print $1; exit }\'')
        forward = run.index("TCP:agent-testdb:5432")
        ready = run.index("pg_isready -q -h 127.0.0.1")
        self.assertIn("for attempt in $(seq 1 30)", run[:resolve])
        self.assertLess(remove_stale_pin, resolve)
        self.assertLess(resolve, pin)
        self.assertLess(pin, verify_pin)
        mismatch_check = run.index('if [ "$pinned_ip" != "$service_ip" ]; then')
        mismatch_exit = run.index("exit 1", mismatch_check)
        self.assertLess(verify_pin, mismatch_check)
        self.assertLess(mismatch_exit, forward)
        self.assertLess(verify_pin, forward)
        self.assertLess(forward, ready)
        self.assertIn("sleep 1", run)
        fail_closed = run.index('if [ -z "$service_ip" ]; then')
        self.assertLess(fail_closed, pin)
        self.assertLess(run.index("exit 1", fail_closed), pin)
        self.assertIn("[[ \"$service_ip\" =~ ^([0-9]{1,3}\\.){3}[0-9]{1,3}$ ]]", run)
        # A failed lookup must empty the result, not kill the step under pipefail.
        self.assertIn('service_ip="$(getent ahostsv4 agent-testdb 2>/dev/null | awk \'NR == 1 { print $1; exit }\')" || service_ip=""', run)
        self.assertIn('echo "agent-testdb did not resolve to an IPv4 address"', run)

    def test_lfg_runtime_regressions_receive_disposable_database_url(self):
        job = self.workflows["check.yml"]["jobs"]["ignored-db-stores"]
        step = next(
            step for step in job["steps"]
            if step.get("name") == "LFG actual-router and mock-Discord regressions"
        )
        # The fixture guard rejects any URL whose database is not two_bot_test_*.
        self.assertEqual(
            step["env"]["TWO_TEST_DATABASE_URL"],
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
        )
        # Main may add more targets to this shared step; pin its LFG target and
        # serial ignored-test tail without freezing the entire command line.
        self.assertRegex(
            step["run"],
            r"--test lfg_interactions(?: --test \S+)* -- --ignored --test-threads=1",
        )

    def test_tickets_postgres_acceptance_receives_disposable_database_url(self):
        job = self.workflows["check.yml"]["jobs"]["tickets-postgres"]
        prepare = next(
            step for step in job["steps"]
            if step.get("name") == "Prepare guarded test database"
        )
        self.assertIn("createdb -h agent-testdb -U agent_test two_bot_test_ci", prepare["run"])
        # The rust:bookworm job container ships no PostgreSQL client; a
        # bare createdb fails with exit 127, so the install must precede it.
        self.assertIn("apt-get install -y -qq --no-install-recommends postgresql-client", prepare["run"])
        self.assertLess(
            prepare["run"].index("apt-get install"),
            prepare["run"].index("createdb -h agent-testdb"),
        )
        step = next(
            step for step in job["steps"]
            if step.get("name") == "Ticket shared router and mock Discord lifecycle acceptance"
        )
        # The non-ignored creation-receipt acceptance modules panic in CI
        # without the guarded bootstrap; the fixture guard rejects any URL
        # whose database is not two_bot_test_*.
        self.assertEqual(
            step["env"]["TWO_TEST_DATABASE_URL"],
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
        )
        self.assertLess(job["steps"].index(prepare), job["steps"].index(step))

    def test_runtime_postgres_wal_budget_is_bounded(self):
        # Keep the broader runtime-lane budget from main; the smaller target is
        # specific to the longer rust-tests coverage replay.
        job = self.workflows["check.yml"]["jobs"]["ignored-db-runtime"]
        prepare = next(
            step for step in job["steps"]
            if step.get("name") == "Prepare isolated test databases"
        )
        verify = next(
            step for step in job["steps"]
            if step.get("name") == "Verify disposable Postgres defaults"
        )
        self.assertIn("ALTER SYSTEM SET max_wal_size = '512MB'", prepare["run"])
        self.assertIn("ALTER SYSTEM SET min_wal_size = '64MB'", prepare["run"])
        self.assertIn("pg_reload_conf()", prepare["run"])
        self.assert_wal_reload_ordering_is_pinned(prepare["run"])
        self.assertIn("'max_wal_size': '512MB'", verify["run"])
        self.assertIn("'min_wal_size': '64MB'", verify["run"])

    def test_ops_acceptance_env_is_isolated_from_parallel_sweeps(self):
        suite = "settings_jobs::tests::ops_overrides_acceptance_tests::"
        for job_name in ("rust-tests", "coverage-shard"):
            with self.subTest(job=job_name):
                steps = self.workflows["check.yml"]["jobs"][job_name]["steps"]
                runs = [
                    (step, line)
                    for step in steps
                    for line in shell_logical_lines(step.get("run", ""))
                ]
                broad = [
                    line for _, line in runs
                    if "--workspace --lib --bins" in line
                    and "--ignored" not in line and "property_" not in line
                ]
                self.assertEqual(len(broad), 1)
                self.assertIn(f"--skip {suite}", broad[0])
                isolated = [
                    (step, line) for step, line in runs
                    if suite in line and f"--skip {suite}" not in line
                ]
                self.assertEqual(len(isolated), 1)
                step, command = isolated[0]
                self.assertIn("-p two-bot --bin two-bot", command)
                self.assertIn("--features two-bot-core/db", command)
                self.assertIn("--locked", command)
                self.assertIn("--test-threads=1", command)
                self.assertNotIn("--ignore-run-fail", command)
                self.assertNotIn("--ignored", command)
                self.assertEqual(
                    step["env"]["TWO_TEST_DATABASE_URL"],
                    "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
                )
                if job_name == "coverage-shard":
                    self.assertIn("--no-report", command)
                    self.assertEqual(
                        shlex.split(command),
                        shlex.split(
                            'cov_run "$COV" llvm-cov --no-report -p two-bot --bin two-bot '
                            '--features two-bot-core/db --locked -- '
                            f'{suite} --test-threads=1 --skip regenerate_reference_docs'
                        ),
                    )
                    [block] = re.findall(
                        r"if selected sweep-nonignored; then\n(.*?)\nfi",
                        step["run"], re.DOTALL,
                    )
                    block_runs = [line for line in shell_logical_lines(block)
                                  if line.startswith("cov_run ")]
                    self.assertEqual(block_runs, [broad[0], command])
                    self.assertLess(
                        step["run"].index(command),
                        step["run"].index("export TWO_TEST_DATABASE_URL="),
                    )

    def test_coverage_ops_replay_is_selected_serial_and_fails_closed(self):
        step = next(
            step for step in self.workflows["check.yml"]["jobs"]["coverage-shard"]["steps"]
            if step.get("name") == "Patch coverage legs (shard instrumented run, no report)"
        )
        suite = "settings_jobs::tests::ops_overrides_acceptance_tests::"
        run = step["run"]
        wrapper = run[run.index("cov_run() {"):run.index("# Main sweep")]
        sweeps = run[run.index("if selected sweep-nonignored; then"):
                     run.index("if selected_any core-stream_live_store")]
        bootstrap = "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci"
        cases = (
            ("", False, 0),
            ("sweep-ignored", False, 1),
            ("core-stream_live_store", False, 0),
            ("sweep-nonignored", False, 2),
            ("sweep-nonignored sweep-ignored", False, 3),
            ("sweep-nonignored", True, 2),
            ("sweep-nonignored sweep-ignored", True, 2),
            ("sweep-ignored", True, 1),
        )
        with tempfile.TemporaryDirectory() as directory:
            stub = Path(directory) / "cargo-llvm-cov"
            stub.write_text(
                '#!/bin/bash\nprintf "%s\\t%s\\n" "$TWO_TEST_DATABASE_URL" "$*"\n'
                f'if [ "$FAIL_OPS" = 1 ] && [[ " $* " == *" -- {suite} "* ]]; '
                'then exit 23; fi\n'
            )
            stub.chmod(0o755)
            for legs, fail_ops, expected_count in cases:
                with self.subTest(legs=legs, fail_ops=fail_ops):
                    result = subprocess.run(
                        ["bash", "-c", "set -euo pipefail; " + wrapper + sweeps],
                        env=dict(os.environ, COV=str(stub), SHARD_LEGS=legs,
                                 TWO_TEST_DATABASE_URL=bootstrap,
                                 FAIL_OPS="1" if fail_ops else "0"),
                        capture_output=True, text=True, check=False, timeout=15,
                    )
                    selected = "sweep-nonignored" in legs.split()
                    must_fail = selected and fail_ops
                    self.assertEqual(result.returncode, 1 if must_fail else 0, result.stderr)
                    if must_fail:
                        self.assertIn("patch coverage measurement failed", result.stderr)
                    else:
                        self.assertEqual(result.stderr, "")
                    invocations = result.stdout.splitlines()
                    self.assertEqual(len(invocations), expected_count, result.stdout)
                    commands = []
                    for invocation in invocations:
                        url, command = invocation.split("\t", 1)
                        self.assertEqual(url, bootstrap)
                        commands.append(command)
                    isolated = [command for command in commands
                                if suite in command and f"--skip {suite}" not in command]
                    self.assertEqual(len(isolated), int(selected))
                    if selected:
                        self.assertIn("--workspace --lib --bins --tests", commands[0])
                        self.assertIn(f"--skip {suite}", commands[0])
                        self.assertEqual(commands[1], isolated[0])
                        self.assertEqual(shlex.split(isolated[0]), shlex.split(
                            "llvm-cov --no-report -p two-bot --bin two-bot "
                            "--features two-bot-core/db --locked -- "
                            f"{suite} --test-threads=1 --skip regenerate_reference_docs"
                        ))
                    ignored = [command for command in commands if "-- --ignored" in command]
                    self.assertEqual(len(ignored), int("sweep-ignored" in legs.split()
                                                      and not must_fail))

    def test_invite_saturation_isolation_in_both_lanes(self):
        suite = "invite_uses_saturation_acceptance"
        source = (ROOT / f"crates/store/tests/{suite}.rs").read_text(encoding="utf-8")
        names = set(tokio_test_names(source))
        for job_name in ("rust-tests", "coverage-shard"):
            with self.subTest(job=job_name):
                steps = self.workflows["check.yml"]["jobs"][job_name]["steps"]
                runs = [
                    (step, line)
                    for step in steps
                    for line in shell_logical_lines(step.get("run", ""))
                ]
                sweeps = [
                    line for _, line in runs
                    if "--workspace" in line and ("--tests" in line or "--test '*'" in line)
                    and "--ignored" not in line and "property_" not in line
                ]
                self.assertEqual(len(sweeps), 1)
                skips = set(re.findall(r"--skip (\S+)", sweeps[0]))
                for name in sorted(names):
                    with self.subTest(fixture=name):
                        self.assertIn(name, skips)
                replays = [(step, line) for step, line in runs if f"--test {suite}" in line]
                self.assertEqual(len(replays), 1)
                step, replay = replays[0]
                self.assertIn("-p two-bot-store", replay)
                self.assertIn("--locked", replay)
                self.assertIn("--test-threads=1", replay)
                self.assertNotIn("--ignore-run-fail", replay)
                self.assertNotIn("--ignored", replay)
                # Neither Cargo's positional filter nor libtest's filters
                # may silently narrow the complete serial replay.
                self.assertEqual(
                    replay.split(" -- ", 1)[0],
                    f'cov_run "$COV" llvm-cov --no-report -p two-bot-store --features two-bot-core/db --locked --test {suite}'
                    if job_name == "coverage-shard" else
                    f"cargo test -p two-bot-store --test {suite} --features two-bot-core/db --locked",
                )
                self.assertEqual(
                    replay.split(" -- ", 1)[1].split(),
                    ["--test-threads=1", "--skip", "regenerate_reference_docs"]
                    if job_name == "coverage-shard" else ["--test-threads=1"],
                )
                if job_name == "coverage-shard":
                    self.assertIn("--no-report", replay)
                    exports = [
                        line for line in shell_logical_lines(step["run"].split(replay, 1)[0])
                        if line.startswith("export TWO_TEST_DATABASE_URL=")
                    ]
                    self.assertTrue(exports)
                    self.assertEqual(
                        exports[-1],
                        "export TWO_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test",
                    )
                self.assertEqual(
                    step["env"]["TWO_TEST_DATABASE_URL"],
                    "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
                )

    def test_coverage_shard_runs_with_db_and_includes_ignored_tests(self):
        job = self.workflows["check.yml"]["jobs"]["coverage-shard"]
        step = next(
            step for step in job["steps"]
            if step.get("name") == "Patch coverage legs (shard instrumented run, no report)"
        )
        self.assertEqual(step["env"]["SHARD_LEGS"], "${{ matrix.legs }}")
        self.assertEqual(step["env"]["SHARD_INDEX"], "${{ matrix.shard_index }}")
        self.assertIn("--ignore-run-fail", step["run"])
        self.assertIn("--features two-bot-core/db", step["run"])
        self.assertIn("--lib --bins --tests", step["run"])
        self.assertIn(
            "--no-report -- --ignored --test-threads=1 --skip regenerate_reference_docs",
            step["run"],
        )
        self.assertIn(
            "--locked -- channel_moderation --include-ignored --test-threads=1",
            step["run"],
        )
        self.assertIn('report_path="$GITHUB_WORKSPACE/lcov-shard-$SHARD_INDEX.info"', step["run"])
        self.assertIn('[ ! -s "$report_path" ]', step["run"])
        self.assertIn(
            'llvm-cov report --workspace --lcov --output-path "$report_path"',
            step["run"],
        )
        self.assertIn('retrying via stdout', step["run"])
        self.assertIn(
            'llvm-cov report --workspace --lcov > "$report_path"', step["run"]
        )
        self.assertNotIn("/dev/stdout", step["run"])
        # `report` without --workspace scopes to the current (root) package
        # only and excludes every crates/* member directory from the export;
        # the root library is a single const with no coverable lines, so the
        # merged report comes out empty. Every report invocation must carry
        # --workspace; the bare form must never reappear.
        self.assertNotIn("llvm-cov report --lcov", step["run"])
        # Guarded suites are measured per suite with their accepted URL and
        # merged: --no-report per run, one final report. Selected ignored runs
        # are serial and never execute the reference-docs regen.
        self.assertIn("--no-report", step["run"])
        self.assertIn("llvm-cov report --workspace --lcov", step["run"])
        # Non-ignored tests stay parallel; only the ignored sweep is serial.
        self.assertIn("-- --ignored --test-threads=1", step["run"])
        self.assertIn("--test-threads=1", step["run"])
        self.assertIn("--skip regenerate_reference_docs", step["run"])
        self.assertIn(
            "TWO_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test",
            step["run"],
        )
        self.assertIn(
            "TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test",
            step["run"],
        )
        self.assertIn("MEMBER_TESTDB=agent-testdb", step["run"])
        # The member-moderation guard panics without MEMBER_TESTDB, so the
        # export must precede the ignored sweep: otherwise the suite prints
        # 32 FAILED lines in a tolerated sweep that the flake ledger
        # misattributes as real flakes. The dedicated re-run below stays the
        # fail-closed gate.
        self.assertLess(
            step["run"].index("export MEMBER_TESTDB=agent-testdb"),
            step["run"].index("--no-report -- --ignored --test-threads=1"),
        )
        for target in (
            "channel_moderation_runtime",
            "internal_channel_moderation",
            "feeds_store",
            "legacy_verify_db",
            "staging_migrate_db",
            "internal_events",
            "raid_list_db",
            "reengagement_list_db",
            "member_moderation_db",
            "invite_uses_saturation_acceptance",
        ):
            with self.subTest(target=target):
                self.assertIn(target, step["run"])
        # Per-invocation pins: whole-script substring checks let one surviving
        # occurrence mask a flag dropped from another invocation (e.g.
        # --ignore-run-fail removed from either sweep). Join line
        # continuations, then assert flags on each cov_run separately.
        logical_lines = shell_logical_lines(step["run"])
        cov_runs = [line for line in logical_lines if line.startswith("cov_run ")]
        # cov_run's failure annotation must go to stderr: the stdout report
        # retry redirects cov_run's stdout into the lcov file.
        self.assertIn(
            'echo "::error::patch coverage measurement failed to build or report" >&2',
            step["run"],
        )
        sweeps = [run for run in cov_runs if "--workspace --lib --bins --tests" in run]
        self.assertEqual(len(sweeps), 2)
        for sweep in sweeps:
            with self.subTest(sweep=sweep):
                # A failing test must never suppress coverage collection.
                self.assertIn("--ignore-run-fail", sweep)
                self.assertIn("--no-report", sweep)
                self.assertIn("--features two-bot-core/db", sweep)
                self.assertIn("--locked", sweep)
        ignored_sweeps = [run for run in sweeps if "-- --ignored" in run]
        self.assertEqual(len(ignored_sweeps), 1)
        self.assertIn("--test-threads=1", ignored_sweeps[0])
        self.assertIn("--skip regenerate_reference_docs", ignored_sweeps[0])
        self.assertIn(
            "--skip flags_are_guild_scoped_half_open_deduplicated_and_activity_protected",
            ignored_sweeps[0],
        )
        self.assertIn(
            "--skip list_is_guild_scoped_selector_shaped_and_read_only",
            ignored_sweeps[0],
        )
        # The stream go-live ledger suite, the internal-events DB test, the
        # legacy-verify/TLS-fence/migrate suites and the mutation-429 suite
        # share the same strict agent_test guard: they run only in their
        # dedicated agent_test runs below, never in the bootstrap sweep.
        for skipped in (
            "claim_is_owned_once_then_in_flight",
            "failed_send_releases_the_claim_for_the_retry",
            "delivered_rows_are_never_reclaimed",
            "delivered_rows_expire_only_past_the_horizon_with_the_fence_intact",
            "fresh_claim_cannot_be_reclaimed",
            "stale_unposted_claim_is_taken_over_atomically",
            "started_claim_is_never_reclaimed_even_when_stale",
            "started_send_that_provably_failed_releases_for_the_retry",
            "reclaiming_a_missing_key_claims_it_fresh",
            "database::actions_persist_rows_and_preserve_other_events_and_guilds",
            "parity_mismatch_cursor_and_read_only_acceptance",
            "legacy_verify_policy_gates_the_real_test_endpoints",
            "real_sqlx_runner_cases",
            "post_failure_unreadable_ledger_is_unknown_not_empty",
            "post_failure_readable_ledger_keeps_array_output",
            "post_apply_unreadable_ledger_is_unknown_not_empty",
            "store_failure_keeps_committed_cutover_evidence",
            "connection_count_sees_other_roles_from_plan_login",
            "single_delete_429_releases_attempt_for_same_key_retry",
            "bulk_delete_429_releases_attempt_for_same_key_retry",
            "channel_overwrite_429_releases_attempt_and_retires_only_its_seed",
            "slowmode_429_releases_attempt_for_same_and_distinct_keys",
            "unlock_429_releases_lane_but_preserves_recovery_seed",
            "lockdown_429_preserves_prior_seed_while_releasing_the_lane",
            "slowmode_5xx_and_unexpected_statuses_stay_fenced",
            "slowmode_timeout_stays_fenced",
            "cancelled_lockdown_stays_fenced_with_seed_retained",
        ):
            with self.subTest(skipped=skipped):
                self.assertIn(f"--skip {skipped}", ignored_sweeps[0])
        # The parity skip above keeps its unskipped, correctly ordered dedicated rerun.
        parity_reruns = [run for run in cov_runs if "--test legacy_verify_db" in run]
        self.assertEqual(len(parity_reruns), 1)
        self.assertIn("-- --ignored --test-threads=1", parity_reruns[0])
        self.assertNotIn(
            "--skip parity_mismatch_cursor_and_read_only_acceptance",
            parity_reruns[0],
        )
        # The parity guard accepts only the omitted-password URL. Its export
        # must occur once, before the rerun and before the later URL switch.
        parity_export = "export TWO_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test"
        later_export = "export TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test"
        self.assertEqual(logical_lines.count(parity_export), 1)
        self.assertLess(
            logical_lines.index(parity_export), logical_lines.index(parity_reruns[0])
        )
        self.assertLess(
            logical_lines.index(parity_reruns[0]), logical_lines.index(later_export)
        )
        # One MEMBER_TESTDB export serves both the ignored sweep and the
        # fail-closed member_moderation_db re-run. Pin its count and logical
        # command order so a redundant later export cannot mask a deleted or
        # moved sweep export.
        member_exports = [
            index for index, line in enumerate(logical_lines)
            if line == "export MEMBER_TESTDB=agent-testdb"
        ]
        self.assertEqual(len(member_exports), 1)
        ignored_sweep_index = next(
            index for index, line in enumerate(logical_lines)
            if line.startswith("cov_run ")
            and "--workspace --lib --bins --tests" in line
            and "-- --ignored" in line
        )
        member_rerun_index = next(
            index for index, line in enumerate(logical_lines)
            if "--test member_moderation_db" in line
        )
        self.assertLess(member_exports[0], ignored_sweep_index)
        self.assertLess(ignored_sweep_index, member_rerun_index)
        self.assertEqual(len([run for run in sweeps if "-- --ignored" not in run]), 1)
        main_sweep = [run for run in sweeps if "-- --ignored" not in run][0]
        # Guarded suites panic under the bootstrap URL; they run only in
        # their dedicated agent_test steps below, never in the main sweep.
        self.assertIn(
            "--skip cli_write_failure_and_crash_never_publish_prune_or_upload_a_partial_dump",
            main_sweep,
        )
        self.assertIn(
            "--skip seeded_aggregates_render_exactly_guild_scoped_and_read_only",
            main_sweep,
        )
        # Invite saturation fixtures race the parallel sweep (nanos+pid
        # schema nonce); they run only in the serial re-run below, never in
        # the main sweep. Enumerate from the Rust source so a future sixth
        # fixture cannot silently rejoin.
        saturation_source = (
            ROOT / "crates/store/tests/invite_uses_saturation_acceptance.rs"
        ).read_text()
        saturation_names = set(tokio_test_names(saturation_source))
        self.assertTrue(saturation_names, "saturation fixtures must be discovered")
        main_skips = set(re.findall(r"--skip (\S+)", main_sweep))
        for name in sorted(saturation_names):
            with self.subTest(saturation_skip=name):
                self.assertIn(name, main_skips)
        # Staging fixtures accept only the exact agent_test URL; they run in
        # the dedicated staging_migrate_db re-run below, never in this
        # two_bot_test_ci sweep. Enumerate from the Rust source so a future
        # seventh fixture cannot silently rejoin.
        staging_source = (ROOT / "crates/cutover/tests/staging_migrate_db.rs").read_text()
        staging_names = set(re.findall(r'#\[ignore[^\]]*\]\s*async fn (\w+)\(', staging_source))
        self.assertTrue(staging_names, "guarded ignored fixtures must be discovered")
        # Match complete tokens: a suffixed filter would not skip the fixture.
        broad_skips = set(re.findall(r"--skip (\S+)", ignored_sweeps[0]))
        for name in sorted(staging_names):
            with self.subTest(staging_skip=name):
                self.assertIn(name, broad_skips)
        # Mutation-429 channel fixtures run in the dedicated
        # mutation_429 rerun below, never in this two_bot_test_ci sweep.
        # Enumerate from the Rust source so a future tenth fixture cannot
        # silently rejoin.
        channel_source = (
            ROOT / "crates/discord/tests/mutation_429_proves_no_effect_channel.rs"
        ).read_text()
        channel_names = set(
            re.findall(r'#\[ignore[^\]]*\]\s*async fn (\w+)\(', channel_source)
        )
        self.assertTrue(channel_names, "guarded ignored fixtures must be discovered")
        for name in sorted(channel_names):
            with self.subTest(channel_skip=name):
                self.assertIn(name, broad_skips)
        ignored_sweep = [run for run in sweeps if "-- --ignored" in run][0]
        self.assertIn(
            "--skip list_is_guild_scoped_selector_shaped_and_read_only",
            ignored_sweep,
        )
        guarded = [run for run in cov_runs if "llvm-cov --no-report -p " in run]
        self.assertEqual(len(guarded), 17)
        for run in guarded:
            with self.subTest(guarded=run):
                self.assertIn("--test-threads=1", run)
                self.assertIn("--skip regenerate_reference_docs", run)
                self.assertIn("--locked", run)
                # Deliberate: a guard panic in a guarded suite fails the step.
                self.assertNotIn("--ignore-run-fail", run)
        # needle -> carries `--features db` (core/discord suites need the db
        # feature to build their guarded targets; cutover and store suites
        # do not).
        guarded_needles = {
            "--test stream_live_store": True,
            "--test mutation_429_proves_no_effect_channel": True,
            "-- channel_moderation --include-ignored": True,
            "--test channel_moderation_runtime": True,
            "--test internal_channel_moderation": True,
            "--test feeds_store": True,
            "--test legacy_verify_db": False,
            "--test legacy_tls_fence": False,
            "--test staging_migrate_db": False,
            "--test internal_events": True,
            "--test raid_list_db": False,
            "--test reengagement_list_db": False,
            "--test game_activity_report_db": False,
            "--test member_moderation_db": True,
            "--test invite_uses_saturation_acceptance": False,
            "--test starboard_store": False,
            "--test starboard_api": False,
            "--test starboard_config_updated_at": False,
        }
        for needle, wants_db_feature in guarded_needles.items():
            with self.subTest(needle=needle):
                matches = [run for run in guarded if needle in run]
                self.assertEqual(len(matches), 1)
                self.assertIn("--test-threads=1", matches[0])
                if wants_db_feature:
                    self.assertIn("--features db", matches[0])
                else:
                    self.assertNotIn("--features db", matches[0])
                if needle == "--test staging_migrate_db":
                    for name in sorted(staging_names):
                        with self.subTest(dedicated_no_skip=name):
                            self.assertNotIn(f"--skip {name}", matches[0])
                if needle == "--test mutation_429_proves_no_effect_channel":
                    for name in sorted(channel_names):
                        with self.subTest(dedicated_no_skip=name):
                            self.assertNotIn(f"--skip {name}", matches[0])
        env_wrapped = [
            run for run in cov_runs
            if run.startswith('cov_run env -u PGPASSWORD -u PGPASSFILE -u PGOPTIONS "$COV"')
        ]
        self.assertEqual(len(env_wrapped), 1)
        self.assertIn("--test channel_moderation_runtime", env_wrapped[0])
        reports = [run for run in cov_runs if "llvm-cov report" in run]
        self.assertEqual(len(reports), 2)
        for report in reports:
            with self.subTest(report=report):
                self.assertIn("llvm-cov report --workspace --lcov", report)
        self.assertEqual(
            step["env"]["TWO_TEST_DATABASE_URL"],
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
        )
        self.assertIn("agent-testdb", job["services"])
        self.assertEqual(
            step["if"],
            "always() && !cancelled() && steps.prepare_test_databases.outcome == 'success' "
            "&& steps.verify_disposable_postgres.outcome == 'success'",
        )
        # The measurement rides alone in its own job: no plain `cargo test`
        # step may re-merge the suite signal into this lane.
        self.assertEqual(
            [candidate.get("name") for candidate in job["steps"]
             if "cargo test" in str(candidate.get("run", ""))],
            [],
        )
        self.assertEqual(
            next(candidate for candidate in job["steps"]
                 if candidate.get("name") == "Prepare isolated test databases")["id"],
            "prepare_test_databases",
        )
        self.assertEqual(
            next(candidate for candidate in job["steps"]
                 if candidate.get("name") == "Verify disposable Postgres defaults")["id"],
            "verify_disposable_postgres",
        )
        checkout = next(step for step in job["steps"] if step.get("uses", "").startswith("actions/checkout@"))
        self.assertEqual(checkout["with"]["fetch-depth"], "0")

    def test_ignored_test_names_include_intervening_comments_and_blank_lines(self):
        separators = (
            "",
            "\n \t\n",
            "// first comment\n\n  // second comment\n",
            "/* block comment\n   on two lines */\n",
            "/* inline comment */ ",
            "#[tokio::test]\n\n// between attribute and function\n",
            "// before attribute\n#[tokio::test]\n/* after attribute */\n\n",
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crates/fixture/tests/discovery.rs"
            source.parent.mkdir(parents=True)
            for separator in separators:
                for prefix in ("", "async "):
                    with self.subTest(separator=separator, prefix=prefix):
                        source.write_text(
                            '#[test]\nfn ordinary_before() {}\n'
                            '#[ignore = "requires disposable test database"]\n'
                            + separator + prefix + 'fn selected() {}\n'
                            '#[test]\nfn ordinary_after() {}\n'
                            '#[ignore]\nfn other_ignored() {}\n'
                            'struct NotAFunction;\nfn unrelated() {}\n',
                            encoding="utf-8",
                        )
                        with patch(f"{__name__}.ROOT", root):
                            self.assertEqual(
                                ignored_test_names("discovery"), ["selected", "other_ignored"],
                            )

    def test_ignored_test_names_fail_closed_on_unsupported_forms(self):
        unsupported = (
            ("public function", '#[ignore]\npub async fn omitted() {}\n'),
            ("multiline attribute", '#[ignore]\n#[cfg_attr(\n'
             '    all(feature = "first", feature = "second"),\n'
             '    tokio::test\n)]\nasync fn omitted() {}\n'),
            ("nested block comment", '#[ignore]\n/* outer comment\n'
             '   /* nested comment */\n*/\nasync fn omitted() {}\n'),
            ("non-function item", '#[ignore]\nstruct NotAFunction;\nfn unrelated() {}\n'),
            ("no following function", '#[ignore]\n'),
            ("no final newline", '#[ignore]'),
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crates/fixture/tests/discovery.rs"
            source.parent.mkdir(parents=True)
            for label, declaration in unsupported:
                with self.subTest(form=label):
                    source.write_text(
                        '#[ignore]\nfn discovered() {}\n' + declaration,
                        encoding="utf-8",
                    )
                    with patch(f"{__name__}.ROOT", root):
                        with self.assertRaisesRegex(
                            ValueError, r"discovery\.rs: discovered 1 of 2 ignored test functions",
                        ):
                            ignored_test_names("discovery")

    def test_coverage_shard_ignored_sweep_skips_tests_that_cannot_pass_there(self):
        # The ignored sweep tolerates failures, but the flake ledger reads
        # every FAILED line it prints as a flake. Skip exact-URL opt-ins and
        # explicitly quarantined red tests, not passing DB opt-ins in those
        # backfill suites. Names and quarantine markers come from the sources.
        # Exact-URL suites: a fail-closed run elsewhere still executes them.
        # The stream go-live ledger suite shares the strict agent_test
        # guard; its dedicated replay below stays fail-closed.
        refused_suites = (
            "legacy_tls_fence", "legacy_verify_db", "raid_list_db",
            "reengagement_list_db", "staging_migrate_db", "internal_events",
            "mutation_429_proves_no_effect_channel", "stream_live_store",
        )
        # Red acceptance suites: the store lane skips every one of them too.
        red_suites = (
            "backfill_audit_log", "backfill_bans_invites_onboarding",
            "backfill_event_users_reactions_pins",
        )
        jobs = self.workflows["check.yml"]["jobs"]
        step = next(
            step for step in jobs["coverage-shard"]["steps"]
            if step.get("name") == "Patch coverage legs (shard instrumented run, no report)"
        )
        [sweep] = [
            line for line in shell_logical_lines(step["run"])
            if line.startswith("cov_run ") and "--workspace --lib --bins --tests" in line
            and "-- --ignored" in line
        ]
        sweep_skips = re.findall(r"--skip (\S+)", sweep)
        # Fail-closed command lines only: both tolerated sweeps drop out.
        runs = [
            line
            for job in jobs.values() for candidate in job.get("steps", [])
            for line in shell_logical_lines(str(candidate.get("run", "")))
            if "--ignore-run-fail" not in line
        ]
        expected = set()
        for suite in refused_suites:
            names = ignored_test_names(suite)
            self.assertTrue(names, suite)
            expected.update(names)
            target = re.compile(rf"--test {re.escape(suite)}(?:\s|$)")
            gates = [
                re.findall(r"--skip (\S+)", line) for line in runs
                if target.search(line) and re.search(r"--(?:include-)?ignored\b", line)
            ]
            for name in names:
                with self.subTest(suite=suite, test=name):
                    self.assertTrue(
                        any(
                            not any(skip_blocks_test(skip, name) for skip in skips)
                            for skips in gates
                        ),
                        f"no fail-closed --test {suite} run executes {name}",
                    )
        [store_lane] = [line for line in runs if "--test backfill_audit_log" in line]
        store_skips = re.findall(r"--skip (\S+)", store_lane)
        quarantined = set()
        for suite in red_suites:
            quarantined.update(backfill_quarantine_members(ignored_test_reasons(suite)))
        expected.update(quarantined)
        self.assertEqual(
            set(store_skips), quarantined,
            "store skip set must match quarantine; un-skip graduated tests in the fail-closed lane",
        )
        # The internal-events DB test is skipped by its module-qualified
        # path: the bare function name would also match a same-named test
        # in any other module swept here. The qualified pin in the
        # coverage contract above keeps the narrower filter.
        expected.discard("actions_persist_rows_and_preserve_other_events_and_guilds")
        expected.add("database::actions_persist_rows_and_preserve_other_events_and_guilds")
        self.assertEqual(len(sweep_skips), len(set(sweep_skips)))
        self.assertEqual(set(sweep_skips) - {"regenerate_reference_docs"}, expected)

    def test_backfill_quarantine_tripwire_reports_growth_shrinkage_and_untracked(self):
        pinned = {"alpha": "TOG-1", "beta": "TOG-2"}
        clean = {
            "alpha": '#[ignore = "quarantined red, tracked by TOG-1"]',
            "beta": '#[ignore = "quarantined red, tracked by TOG-2"]',
        }
        self.assertEqual(backfill_quarantine_errors(pinned, clean), [])
        grown = dict(clean, gamma='#[ignore = "quarantined red, tracked by TOG-3"]')
        [growth] = backfill_quarantine_errors(pinned, grown)
        self.assertIn("gamma", growth)
        self.assertIn("grew", growth)
        [shrinkage] = backfill_quarantine_errors(
            pinned, {"alpha": clean["alpha"]},
        )
        self.assertIn("shrank", shrinkage)
        self.assertIn("BACKFILL_QUARANTINE", shrinkage)
        self.assertIn("beta", shrinkage)
        untracked = dict(clean, alpha='#[ignore = "quarantined red, requires authorized TEST_DATABASE_URL"]')
        [missing] = backfill_quarantine_errors(pinned, untracked)
        self.assertIn("alpha", missing)
        self.assertIn("TOG-1", missing)

    def test_fixed_backfill_test_can_keep_database_ignore_after_quarantine_ratchet(self):
        reason = '#[ignore = "requires authorized TEST_DATABASE_URL"]'
        [shrinkage] = backfill_quarantine_errors({"alpha": "TOG-1"}, {"alpha": reason})
        self.assertIn("shrank", shrinkage)
        self.assertIn("alpha", shrinkage)
        self.assertEqual(backfill_quarantine_errors({}, {"alpha": reason}), [])

    def test_fixed_backfill_family_can_leave_coverage_and_store_skip_sets(self):
        reasons = ignored_test_reasons
        for suite in BACKFILL_QUARANTINE_SUITES:
            fixed = set(ignored_test_names(suite))
            current = {}
            for candidate in BACKFILL_QUARANTINE_SUITES:
                current.update(reasons(candidate))
            for name in fixed:
                current[name] = '#[ignore = "requires authorized TEST_DATABASE_URL"]'
            pinned = {name: ref for name, ref in BACKFILL_QUARANTINE.items() if name not in fixed}
            mutated = deepcopy(self.workflows)
            for workflow in ("check.yml", "nightly.yml"):
                for job in mutated[workflow]["jobs"].values():
                    for step in job.get("steps", []):
                        if "run" in step:
                            for name in fixed:
                                step["run"] = step["run"].replace(f"--skip {name}", "")
            store = next(
                step for job in mutated["check.yml"]["jobs"].values()
                for step in job.get("steps", [])
                if "--test backfill_audit_log" in step.get("run", "")
            )
            nightly = next(
                step for step in mutated["nightly.yml"]["jobs"]["sweep"]["steps"]
                if step.get("name") == "Full workspace sweep including ignored tests"
            )
            with patch.object(self, "workflows", mutated), patch(
                f"{__name__}.ignored_test_reasons",
                side_effect=lambda candidate: (
                    {name: current[name] for name in fixed}
                    if candidate == suite else reasons(candidate)
                ),
            ), patch(f"{__name__}.BACKFILL_QUARANTINE", pinned):
                # Proper graduation keeps the DB opt-in and un-skips all three sites.
                with self.subTest(suite=suite, graduation="complete"):
                    self.test_coverage_shard_ignored_sweep_skips_tests_that_cannot_pass_there()
                    self._assert_backfill_quarantine_skip_sites(current)
                    self.assertEqual(backfill_quarantine_errors(pinned, current), [])
                name = sorted(fixed)[0]
                for site, step in (("store", store), ("nightly", nightly)):
                    original = step["run"]
                    # Cargo also accepts substring and module-qualified skips.
                    for token in (name, name[:-1], f"database::{name}"):
                        with self.subTest(suite=suite, site=site, stale_skip=token):
                            step["run"] = original.rstrip() + f" --skip {token}"
                            try:
                                message = (
                                    "store skip set must match quarantine" if site == "store"
                                    else "un-skip graduated backfill tests in the nightly sweep"
                                )
                                with self.assertRaisesRegex(AssertionError, message):
                                    self._assert_backfill_quarantine_skip_sites(current)
                                if site == "store":
                                    with self.assertRaisesRegex(AssertionError, message):
                                        self.test_coverage_shard_ignored_sweep_skips_tests_that_cannot_pass_there()
                            finally:
                                step["run"] = original

    def _assert_backfill_quarantine_skip_sites(self, current):
        # All three skip sites pin every quarantined test: the coverage sweep
        # and the store lane (check.yml) plus the nightly sweep (nightly.yml).
        check_jobs = self.workflows["check.yml"]["jobs"]
        coverage_step = next(
            step for step in check_jobs["coverage-shard"]["steps"]
            if step.get("name") == "Patch coverage legs (shard instrumented run, no report)"
        )
        [coverage_sweep] = [
            line for line in shell_logical_lines(coverage_step["run"])
            if line.startswith("cov_run ") and "--workspace --lib --bins --tests" in line
            and "-- --ignored" in line
        ]
        coverage_skips = set(re.findall(r"--skip (\S+)", coverage_sweep))
        [store_lane] = [
            line
            for job in check_jobs.values() for candidate in job.get("steps", [])
            for line in shell_logical_lines(str(candidate.get("run", "")))
            if "--test backfill_audit_log" in line
        ]
        store_skips = set(re.findall(r"--skip (\S+)", store_lane))
        [nightly_sweep] = [
            line
            for candidate in self.workflows["nightly.yml"]["jobs"]["sweep"]["steps"]
            if candidate.get("name") == "Full workspace sweep including ignored tests"
            for line in shell_logical_lines(str(candidate.get("run", "")))
        ]
        nightly_skips = set(re.findall(r"--skip (\S+)", nightly_sweep))
        self.assertEqual(
            store_skips, set(BACKFILL_QUARANTINE),
            "store skip set must match quarantine; un-skip graduated tests in the fail-closed lane",
        )
        graduated = set(current) - set(backfill_quarantine_members(current))
        blocked = {}
        for name in sorted(graduated):
            # These backfill fns are top-level: unrelated module-only filters
            # such as channel_moderation_store:: cannot block their bare paths.
            matches = sorted(
                skip for skip in nightly_skips
                if skip.rsplit("::", 1)[-1] and skip_blocks_test(skip, name)
            )
            if matches:
                blocked[name] = matches
        self.assertEqual(blocked, {}, "un-skip graduated backfill tests in the nightly sweep")
        for name in sorted(BACKFILL_QUARANTINE):
            with self.subTest(quarantined=name):
                self.assertIn(name, coverage_skips)
                self.assertIn(name, store_skips)
                self.assertIn(name, nightly_skips)

    def test_backfill_quarantine_is_tracked_counted_and_shrink_only(self):
        # TOG-34944: the 15 nowhere-run red backfill tests are quarantined,
        # never silent. Tracked: each ignore reason carries its build card.
        # Counted: the pin asserts 15. Shrink-only: growth fails, and a fix
        # ratchets the pin down in the same PR.
        self.assertEqual(len(BACKFILL_QUARANTINE), 15)
        current = {}
        for suite in BACKFILL_QUARANTINE_SUITES:
            current.update(ignored_test_reasons(suite))
        self.assertEqual(backfill_quarantine_errors(BACKFILL_QUARANTINE, current), [])
        self._assert_backfill_quarantine_skip_sites(current)
        # Both workflow comments restate the quarantine at test granularity
        # with tracking refs: every name and every ref appears in a `#`
        # comment line, so the prose cannot drift from the pin.
        for workflow in ("check.yml", "nightly.yml"):
            comments = "\n".join(
                line
                for line in (ROOT / ".github/workflows" / workflow).read_text().splitlines()
                if line.strip().startswith("#")
            )
            for name in sorted(BACKFILL_QUARANTINE):
                with self.subTest(workflow=workflow, comment=name):
                    self.assertIn(name, comments)
            for ref in sorted(set(BACKFILL_QUARANTINE.values())):
                with self.subTest(workflow=workflow, comment=ref):
                    self.assertIn(ref, comments)

    def test_fail_closed_gate_sees_qualified_skip_tokens(self):
        # Cargo matches --skip against the full test path, so a qualified
        # token on every fail-closed gate must read as blocking even though
        # the bare fn name never contains the `::` qualifier.
        name = "actions_persist_rows_and_preserve_other_events_and_guilds"
        qualified = f"database::{name}"
        self.assertIn(name, ignored_test_names("internal_events"))
        self.assertTrue(skip_blocks_test(qualified, name))
        self.assertTrue(skip_blocks_test(name, name))
        self.assertFalse(skip_blocks_test("regenerate_reference_docs", name))
        # The old `skip in name` check missed the qualified form entirely.
        self.assertNotIn(qualified, name)
        mutated = deepcopy(self.workflows)
        jobs = mutated["check.yml"]["jobs"]
        touched = 0
        for job in jobs.values():
            for candidate in job.get("steps", []):
                run = str(candidate.get("run", ""))
                if "--test internal_events" not in run:
                    continue
                rewritten = []
                for line in shell_logical_lines(run):
                    if ("--test internal_events" in line
                            and "--ignore-run-fail" not in line
                            and re.search(r"--(?:include-)?ignored\b", line)):
                        line = f"{line} --skip {qualified}"
                        touched += 1
                    rewritten.append(line)
                candidate["run"] = "\n".join(rewritten)
        self.assertEqual(touched, 2)
        runs = [
            line
            for job in jobs.values() for candidate in job.get("steps", [])
            for line in shell_logical_lines(str(candidate.get("run", "")))
            if "--ignore-run-fail" not in line
        ]
        target = re.compile(r"--test internal_events(?:\s|$)")
        gates = [
            re.findall(r"--skip (\S+)", line) for line in runs
            if target.search(line) and re.search(r"--(?:include-)?ignored\b", line)
        ]
        self.assertEqual(len(gates), 2)
        self.assertTrue(all(qualified in skips for skips in gates))
        self.assertFalse(
            any(
                not any(skip_blocks_test(skip, name) for skip in skips)
                for skips in gates
            ),
        )

    def test_coverage_replays_strict_agent_test_targets_and_fails_closed(self):
        job = self.workflows["check.yml"]["jobs"]["coverage-shard"]
        step = next(
            step for step in job["steps"]
            if step.get("name") == "Patch coverage legs (shard instrumented run, no report)"
        )
        run = step["run"]
        wrapper = run[run.index("cov_run() {"):run.index("# Main sweep")]
        # The section URL export and its legs are shard-guarded: execute from
        # the section guard opener through the last leg's `fi`, so the block
        # stays syntactically balanced.
        guard = run.index("if selected_any core-stream_live_store")
        export = run.index("export TWO_TEST_DATABASE_URL=postgres://agent_test@")
        self.assertLess(guard, export)
        start = guard
        end = run.index("if selected_any discord-internal_events")
        # Execute the real replay block and failure wrapper, but replace Cargo
        # with a fixture that records its arguments and effective target URL.
        # The wrapper now carries the shard-selection helpers, so run it with
        # a two-leg SHARD_LEGS: only the selected legs execute.
        targets = ("stream_live_store", "mutation_429_proves_no_effect_channel")
        legs = "core-stream_live_store discord-mutation_429_proves_no_effect_channel"
        with tempfile.TemporaryDirectory() as directory:
            stub = Path(directory) / "cargo-llvm-cov"
            stub.write_text(
                '#!/bin/bash\nprintf "%s %s\\n" "$TWO_TEST_DATABASE_URL" "$*"\n'
                'if [ -n "$FAIL_TARGET" ] && [[ " $* " == *" --test $FAIL_TARGET "* ]]; '
                'then exit 23; fi\n'
            )
            stub.chmod(0o755)
            for failure in ("", *targets):
                with self.subTest(failure=failure):
                    result = subprocess.run(
                        ["bash", "-c",
                         f"set -euo pipefail; SHARD_LEGS='{legs}'; " + wrapper + run[start:end]],
                        env=dict(os.environ, COV=str(stub), FAIL_TARGET=failure),
                        capture_output=True, text=True, check=False,
                    )
                    if failure:
                        self.assertEqual(result.returncode, 1)
                        self.assertIn("patch coverage measurement failed", result.stderr)
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        invocations = result.stdout.splitlines()
                        self.assertEqual(len(invocations), len(targets), result.stdout)
                        for target in targets:
                            matches = [
                                line for line in invocations
                                if f"--test {target} " in line
                            ]
                            self.assertEqual(len(matches), 1, target)
                            self.assertTrue(matches[0].startswith(
                                "postgres://agent_test@agent-testdb:5432/agent_test "
                            ))
                            self.assertIn("--no-report", matches[0])
                            self.assertIn("--features db", matches[0])
                            self.assertIn("--locked", matches[0])
                            self.assertIn("--test-threads=1", matches[0])
                            self.assertRegex(matches[0], r"-- --(?:include-)?ignored(?: |$)")
                            self.assertNotIn("--ignore-run-fail", matches[0])

    def test_coverage_shard_rejects_unknown_leg_before_replay(self):
        job = self.workflows["check.yml"]["jobs"]["coverage-shard"]
        step = next(
            step for step in job["steps"]
            if step.get("name") == "Patch coverage legs (shard instrumented run, no report)"
        )
        run = step["run"]
        wrapper = run[run.index("cov_run() {"):run.index("# Main sweep")]
        result = subprocess.run(
            ["bash", "-c", "set -euo pipefail; " + wrapper],
            env=dict(os.environ, SHARD_LEGS="unknown-leg"),
            capture_output=True, text=True, check=False, timeout=15,
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("::error::unknown coverage leg: unknown-leg", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_coverage_selection_oracles_reject_fail_open_mutants(self):
        mutations = (
            ("test_coverage_replays_strict_agent_test_targets_and_fails_closed",
             "*) return 1 ;;", "*) return 0 ;;"),
            ("test_coverage_shard_rejects_unknown_leg_before_replay",
             'echo "::error::unknown coverage leg: $leg" >&2; exit 1 ;;',
             'echo "::error::unknown coverage leg: $leg" >&2; ;;'),
        )
        for method, old, new in mutations:
            with self.subTest(mutant=old):
                mutated = deepcopy(self.workflows)
                step = next(
                    step for step in mutated["check.yml"]["jobs"]["coverage-shard"]["steps"]
                    if step.get("name") == "Patch coverage legs (shard instrumented run, no report)"
                )
                self.assertEqual(step["run"].count(old), 1)
                step["run"] = step["run"].replace(old, new)
                probe = WorkflowTests(method)
                probe.workflows = mutated
                result = unittest.TestResult()
                # A separate TestResult captures failures inside subTest too;
                # the actual workflow file remains untouched.
                with patch.object(probe, "setUp", return_value=None):
                    probe.run(result)
                self.assertEqual(result.errors, [])
                self.assertEqual(len(result.failures), 1, result.failures)
                self.assertEqual(result.testsRun, 1)

    def test_coverage_plan_emits_the_shard_matrix(self):
        # The planner selects the legs (changed crates plus reverse
        # dependents; full set on workspace-wide changes, pushes to main and
        # schedules) and partitions them into shards. The matrix it emits
        # drives the coverage-shard job; empty shards are dropped so every
        # matrix entry runs at least one leg.
        job = self.workflows["check.yml"]["jobs"]["coverage-plan"]
        self.assertEqual(job.get("name"), "coverage plan")
        self.assertEqual(job.get("permissions"), {"contents": "read"})
        self.assertIn("job-inputs", job.get("needs", []))
        self.assertIn("needs.job-inputs.outputs.rust != 'false'", job.get("if", ""))
        self.assertIn("coverage-plan", job.get("runs-on", ""))
        outputs = job.get("outputs", {})
        for key in ("matrix", "expected-shards", "legs", "widened", "reason"):
            self.assertIn(key, outputs)
        self.assertIn("steps.plan.outputs.matrix", outputs["matrix"])
        self.assertIn("steps.plan.outputs.expected-shards", outputs["expected-shards"])
        step = next(step for step in job["steps"] if step.get("id") == "plan")
        self.assertEqual(step.get("name"), "Plan coverage shards")
        self.assertIn("scripts/ci/plan-coverage-shards.py", step["run"])
        self.assertIn("--shards 4", step["run"])
        self.assertIn("--output plan.json", step["run"])
        self.assertIn('"shard_index"', step["run"])
        self.assertIn('"legs"', step["run"])
        self.assertIn("expected-shards", step["run"])
        # Empty shards never reach the matrix: a shard with no legs would
        # export an empty LCOV and fail the merge.
        self.assertIn("if shard", step["run"])

    def test_coverage_shard_matrix_comes_from_the_plan(self):
        job = self.workflows["check.yml"]["jobs"]["coverage-shard"]
        self.assertEqual(job.get("name"), "coverage shard")
        self.assertEqual(job.get("permissions"), {"contents": "read"})
        self.assertIn("job-inputs", job.get("needs", []))
        self.assertIn("coverage-plan", job.get("needs", []))
        self.assertIn("needs.job-inputs.outputs.rust != 'false'", job.get("if", ""))
        strategy = job.get("strategy", {})
        # BaseLoader keeps scalars as strings; "false" disables fail-fast so
        # every shard reports its own verdict and timing.
        self.assertEqual(strategy.get("fail-fast"), "false")
        self.assertIn("needs.coverage-plan.outputs.matrix", strategy.get("matrix", ""))
        self.assertIn("coverage-shard", job.get("runs-on", ""))
        upload = next(
            step for step in job["steps"]
            if step.get("name") == "Upload shard coverage report"
        )
        self.assertIn("actions/upload-artifact@", upload.get("uses", ""))
        self.assertEqual(upload["with"]["name"], "coverage-shard-${{ matrix.shard_index }}")
        self.assertEqual(upload["with"]["path"], "lcov-shard-${{ matrix.shard_index }}.info")
        self.assertEqual(upload["with"]["if-no-files-found"], "error")
        self.assertEqual(upload["with"].get("overwrite"), "true")

    def test_coverage_shard_leg_selection_matches_the_planner(self):
        # Every planner leg id guards exactly one legs block, and the
        # unknown-id validation list matches the planner's FULL_LEGS: a leg
        # the planner can emit but no guard runs would silently
        # under-measure its crates, and an id the guards accept but the
        # planner never emits is dead code.
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "shard_planner", ROOT / "scripts/ci/plan-coverage-shards.py")
        planner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(planner)
        expected = [leg["id"] for leg in planner.FULL_LEGS]
        job = self.workflows["check.yml"]["jobs"]["coverage-shard"]
        step = next(
            step for step in job["steps"]
            if step.get("name") == "Patch coverage legs (shard instrumented run, no report)"
        )
        run = step["run"]
        self.assertIn("selected() {", run)
        self.assertIn("selected_any() {", run)
        self.assertIn("::error::unknown coverage leg", run)
        for leg_id in expected:
            with self.subTest(leg=leg_id):
                self.assertEqual(
                    run.count(f"if selected {leg_id}; then"), 1,
                    f"leg {leg_id} must guard exactly one block")
        guards = set(re.findall(r"if selected ([a-z0-9_-]+); then", run))
        self.assertEqual(guards, set(expected))
        validated = re.search(r'case " ([a-z0-9_ -]+) " in', run)[1].split()
        self.assertEqual(validated, expected)

    def test_coverage_planner_sweep_commands_match_executed_commands_with_every_skip(self):
        spec = importlib.util.spec_from_file_location(
            "sweep_shard_planner", ROOT / "scripts/ci/plan-coverage-shards.py")
        planner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(planner)
        step = next(
            step for step in self.workflows["check.yml"]["jobs"]["coverage-shard"]["steps"]
            if step.get("name") == "Patch coverage legs (shard instrumented run, no report)"
        )
        sweeps = [leg for leg in planner.FULL_LEGS
                  if leg["id"] in ("sweep-nonignored", "sweep-ignored")]
        self.assertEqual([leg["id"] for leg in sweeps], ["sweep-nonignored", "sweep-ignored"])
        for leg in sweeps:
            with self.subTest(leg=leg["id"]):
                [block] = re.findall(
                    rf"if selected {re.escape(leg['id'])}; then\n(.*?)\nfi",
                    step["run"], re.DOTALL,
                )
                commands = [line for line in shell_logical_lines(block)
                            if line.startswith("cov_run ")]
                executed = [
                    ["cargo" if token == "$COV" else token
                     for token in shlex.split(command)[1:]]
                    for command in commands
                ]
                described = [leg["command"], *leg.get("supplemental_commands", [])]
                self.assertEqual([shlex.split(command) for command in described], executed,
                                 f"{leg['id']} must describe every actual command, including all skips")
                supplemental = leg.get("supplemental_commands", [])
                if leg["id"] == "sweep-nonignored":
                    self.assertEqual(supplemental, [
                        "cargo llvm-cov --no-report -p two-bot --bin two-bot "
                        "--features two-bot-core/db --locked -- "
                        "settings_jobs::tests::ops_overrides_acceptance_tests:: "
                        "--test-threads=1 --skip regenerate_reference_docs"
                    ])
                else:
                    self.assertEqual(supplemental, [])
        self.assertEqual(len(planner.FULL_LEGS), 17)
        # The serial invite-saturation replay is its own store leg; its
        # guarded block must run exactly the command the planner describes.
        invite = planner.FULL_LEGS[16]
        self.assertEqual(invite["id"], "store-invite_uses_saturation_acceptance")
        self.assertEqual(invite["crates"], ["two-bot-store"])
        self.assertEqual(
            invite["command"],
            "cargo llvm-cov --no-report -p two-bot-store --features two-bot-core/db "
            "--locked --test invite_uses_saturation_acceptance -- --test-threads=1 "
            "--skip regenerate_reference_docs",
        )
        self.assertEqual(invite.get("supplemental_commands", []), [])
        [invite_block] = re.findall(
            rf"if selected {re.escape(invite['id'])}; then\n(.*?)\nfi",
            step["run"], re.DOTALL,
        )
        self.assertEqual(
            [["cargo" if token == "$COV" else token for token in shlex.split(line)[1:]]
             for line in shell_logical_lines(invite_block) if line.startswith("cov_run ")],
            [shlex.split(invite["command"])],
        )

    def test_coverage_shard_caches_the_instrumented_build(self):
        # Each shard restores its own rust-cache entry and reports sccache
        # stats for the hit rate; the GitHub cache backend is ref-scoped by
        # the platform, so PR writes can never poison main's entries.
        job = self.workflows["check.yml"]["jobs"]["coverage-shard"]
        env = job.get("env", {})
        self.assertEqual(env.get("RUSTC_WRAPPER"), "sccache")
        self.assertEqual(env.get("SCCACHE_GHA_ENABLED"), "true")
        start = next(
            step for step in job["steps"] if step.get("name") == "Start sccache")
        self.assertTrue(start.get("uses", "").startswith("mozilla-actions/sccache-action@"))
        stats = next(
            step for step in job["steps"]
            if step.get("name") == "sccache stats (advisory speed evidence)")
        self.assertIn("sccache --show-stats", stats.get("run", ""))

    def test_patch_coverage_merge_unions_shards_and_gates(self):
        # The merge keeps the required `patch coverage` check name: it
        # downloads every shard report, unions them with strict N-of-N, and
        # applies the unchanged HARD 80%-of-added-lines bar. It runs whenever
        # Rust was selected and planning succeeded, even when a shard failed,
        # so a partial shard set fails here instead of reading as measured.
        job = self.workflows["check.yml"]["jobs"]["patch-coverage"]
        self.assertEqual(job.get("name"), "patch coverage")
        self.assertEqual(job.get("permissions"), {"contents": "read"})
        for needed in ("job-inputs", "coverage-plan", "coverage-shard"):
            self.assertIn(needed, job.get("needs", []))
        condition = job.get("if", "")
        self.assertIn("always()", condition)
        self.assertIn("!cancelled()", condition)
        self.assertIn("needs.job-inputs.outputs.rust != 'false'", condition)
        self.assertIn("needs.coverage-plan.result == 'success'", condition)
        # Download, union and gate only: no toolchain, container or database.
        self.assertNotIn("container", job)
        self.assertNotIn("services", job)
        text = str(job.get("steps", []))
        self.assertNotIn("rust-toolchain@", text)
        self.assertNotIn("cargo llvm-cov", text)
        download = next(
            step for step in job["steps"]
            if step.get("name") == "Download shard coverage reports")
        self.assertTrue(download.get("uses", "").startswith("actions/download-artifact@"))
        self.assertEqual(download["with"]["pattern"], "coverage-shard-*")
        self.assertEqual(download["with"]["merge-multiple"], "true")
        merge = next(
            step for step in job["steps"]
            if step.get("name") == "Merge coverage shards")
        self.assertEqual(
            merge.get("env", {}).get("EXPECTED_SHARDS"),
            "${{ needs.coverage-plan.outputs.expected-shards }}")
        self.assertIn("scripts/ci/merge-coverage-shards.py", merge["run"])
        self.assertIn('--expected-shards "$EXPECTED_SHARDS"', merge["run"])
        self.assertIn("--source-root /__w/two-bot-next/two-bot-next",
                      " ".join(shell_logical_lines(merge["run"])))
        gate = next(
            step for step in job["steps"]
            if step.get("name") == "Patch coverage (HARD, 80% of added lines)"
        )
        expected_base = "${{ github.event.merge_group.base_sha || (github.event.pull_request && format('{0}^1', github.sha)) || github.event.before || 'origin/main' }}"
        self.assertEqual(gate["env"]["QUALITY_BASE"], expected_base)
        self.assertEqual(gate["env"]["QUALITY_HEAD"], "${{ github.sha }}")
        self.assertIn("scripts/ci/check-patch-coverage.py", gate["run"])

    def test_voice_synthetic_shape_is_pinned_and_mutations_fail(self):
        import copy
        synthetic = self.workflows["voice-synthetic.yml"]
        self.assertEqual(voice_synthetic_errors(synthetic), [])
        for mutate in (
            lambda w: w["jobs"]["synthetic"].pop("if"),
            lambda w: w["jobs"]["synthetic"].__setitem__("environment", "staging"),
            lambda w: w["on"].__setitem__("push", None),
            lambda w: w["jobs"]["synthetic"]["steps"][1].__setitem__("run", "wrangler deploy"),
            lambda w: w["jobs"]["synthetic"]["steps"][0].__setitem__("env", {"T": "${{ secrets.TWO_VOICE_SYNTHETIC_BOT_TOKEN }}"}),
        ):
            changed = copy.deepcopy(synthetic)
            mutate(changed)
            self.assertNotEqual(voice_synthetic_errors(changed), [])

    def test_staging_deploy_gates_on_the_voice_synthetic(self):
        job = self.workflows["deploy-staging.yml"]["jobs"]["voice-synthetic"]
        self.assertEqual(job.get("needs"), "deploy")
        self.assertEqual(job.get("uses"), "./.github/workflows/voice-synthetic.yml")
        self.assertNotIn("if", job)
        # An environment secret is empty in a called workflow unless the caller
        # passes it; pass exactly the test bot token, never `inherit`.
        self.assertEqual(
            job.get("secrets"),
            {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": "${{ secrets.TWO_VOICE_SYNTHETIC_BOT_TOKEN }}"},
        )

    def test_schema_gate_shape_is_pinned_and_mutations_fail(self):
        import copy
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(schema_gate_errors(staging), [])
        for mutate in (
            lambda w: w["jobs"]["schema-gate"].__setitem__("environment", "staging-migrate-apply"),
            lambda w: w["jobs"]["schema-gate"].__setitem__("if", "github.ref == 'refs/heads/main'"),
            lambda w: w["jobs"]["schema-gate"].__setitem__("needs", "deploy"),
            lambda w: w["jobs"]["schema-gate"].pop("needs"),
            lambda w: w["jobs"]["deploy"].pop("needs"),
            lambda w: w["jobs"]["schema-gate"]["steps"][3].__setitem__("run", "wrangler deploy"),
            lambda w: w["jobs"]["schema-gate"]["steps"][3].get("env", {}).__setitem__(
                "LEAK", "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"),
        ):
            changed = copy.deepcopy(staging)
            mutate(changed)
            self.assertNotEqual(schema_gate_errors(changed), [])

    def test_staging_deploy_needs_schema_gate(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(schema_gate_errors(staging), [])
        deploy, gate = staging["jobs"]["deploy"], staging["jobs"]["schema-gate"]
        self.assertIn("schema-gate", [deploy["needs"]] if isinstance(deploy["needs"], str) else deploy["needs"])
        self.assertEqual(gate.get("environment"), "staging-migrate-plan")
        self.assertNotIn("if", gate)
        self.assertEqual(gate.get("needs"), "soak-hold")
        # The chain stays ordered: hold first, gates second, deploy third, synthetic last.
        self.assertEqual(staging["jobs"]["voice-synthetic"].get("needs"), "deploy")

    def test_roles_gate_shape_is_pinned_and_mutations_fail(self):
        import copy
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(roles_gate_errors(staging), [])
        for mutate in (
            lambda w: w["jobs"]["roles-gate"].__setitem__("environment", "staging-migrate-apply"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("if", "github.ref == 'refs/heads/main'"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("needs", "deploy"),
            lambda w: w["jobs"]["roles-gate"].pop("needs"),
            lambda w: w["jobs"]["deploy"].pop("needs"),
            lambda w: w["jobs"]["roles-gate"]["steps"][3].__setitem__("run", "wrangler deploy"),
            lambda w: w["jobs"]["roles-gate"]["steps"][3].get("env", {}).__setitem__(
                "LEAK", "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"),
            lambda w: w["jobs"]["roles-gate"]["steps"][3].__setitem__("if", "${{ false }}"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("if", "${{ false }}"),
            lambda w: w["jobs"]["roles-gate"]["steps"][3].__setitem__("continue-on-error", True),
            lambda w: w["jobs"]["roles-gate"].__setitem__("continue-on-error", True),
            lambda w: w["jobs"]["roles-gate"]["steps"][3].__setitem__(
                "run", w["jobs"]["roles-gate"]["steps"][3]["run"] + " || true"),
        ):
            changed = copy.deepcopy(staging)
            mutate(changed)
            self.assertNotEqual(roles_gate_errors(changed), [])

    def test_staging_deploy_needs_roles_gate(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(roles_gate_errors(staging), [])
        deploy, gate = staging["jobs"]["deploy"], staging["jobs"]["roles-gate"]
        needs = [deploy["needs"]] if isinstance(deploy["needs"], str) else deploy["needs"]
        self.assertIn("schema-gate", needs)
        self.assertIn("roles-gate", needs)
        self.assertEqual(gate.get("environment"), "staging-migrate-plan")
        self.assertNotIn("if", gate)
        self.assertEqual(gate.get("needs"), "soak-hold")
        # The chain stays ordered: hold first, gates second, deploy third, synthetic last.
        self.assertEqual(staging["jobs"]["voice-synthetic"].get("needs"), "deploy")

    def test_staging_takeover_fence_hint_is_scoped_to_the_fenced_refusal(self):
        # Execute the actual workflow shell with a stub control client. This
        # catches swapped case bodies as well as an unscoped fence hint.
        import os
        import subprocess
        import tempfile

        steps = self.workflows["deploy-staging.yml"]["jobs"]["deploy"]["steps"]
        takeover = next(step for step in steps if step.get("id") == "takeover")
        run = takeover["run"]
        self.assertIn("deployment-takeover", run)
        self.assertIn("Singleton is intentionally fenced or uninitialized", run)
        self.assertIn("Staging singleton fenced", run)
        self.assertIn("not a fence refusal", run)
        with tempfile.TemporaryDirectory() as temp_dir:
            temp = Path(temp_dir)
            rollout_dir = temp / "rollout"
            rollout_dir.mkdir()
            (rollout_dir / "receipt.json").write_text('{"worker_version":"test-version"}', encoding="utf-8")
            bin_dir = temp / "bin"
            bin_dir.mkdir()
            node = bin_dir / "node"
            node.write_text('#!/bin/sh\nprintf \'%s\\n\' "$MOCK_CONTROL_OUTPUT"\nexit "${MOCK_CONTROL_EXIT:-1}"\n', encoding="utf-8")
            node.chmod(0o755)
            bash_env = temp / "bash-env"
            bash_env.write_text(f'export PATH="{bin_dir}:$PATH"\n', encoding="utf-8")
            base_env = dict(os.environ, ROLLOUT_DIR=str(rollout_dir),
                            PATH=f"{bin_dir}:{os.environ.get('PATH', '')}", BASH_ENV=str(bash_env))
            shell = run
            cases = (
                ("Singleton is intentionally fenced or uninitialized; explicit staging release required",
                 "Staging singleton fenced", True, 1),
                ("HTTP 401 Unauthorized", "Staging ownership takeover failed (not a fence refusal)", False, 1),
                ("", "Staging ownership takeover timed out after 390 seconds", False, 124),
            )
            for output, expected, is_fenced, exit_code in cases:
                with self.subTest(output=output, exit_code=exit_code):
                    env = dict(base_env, MOCK_CONTROL_OUTPUT=output, MOCK_CONTROL_EXIT=str(exit_code))
                    proc = subprocess.run(
                        ["bash", "-c", shell], cwd=ROOT / "wrangler", env=env,
                        capture_output=True, text=True, timeout=10,
                    )
                    output_text = proc.stdout + proc.stderr
                    self.assertEqual(proc.returncode, 1, output_text)
                    self.assertIn(expected, output_text)
                    self.assertEqual("release_fence" in output_text, is_fenced)

    def test_staging_takeover_outer_timeout_exceeds_the_client_retry_window(self):
        # The workflow's `timeout` must never kill the control client inside
        # its own retry window: a final attempt may start at the window edge
        # and still spend one GET plus one POST, each bounded by the client's
        # AbortSignal. Killing it there could terminate it after the POST
        # committed an epoch and roll back a healthy deploy. Pin both sides:
        # above window + one in-flight attempt, yet close enough that a true
        # hang still fails fast instead of fencing staging.
        steps = self.workflows["deploy-staging.yml"]["jobs"]["deploy"]["steps"]
        run = next(step for step in steps if step.get("id") == "takeover")["run"]
        outer = re.search(r"timeout --signal=TERM --kill-after=\d+s (\d+)s", run)
        self.assertIsNotNone(outer, run)
        client = (ROOT / "wrangler/scripts/ownership-control.mjs").read_text(encoding="utf-8")
        window = re.search(r"takeoverWindowMs = (\d+)", client)
        abort = re.search(r"AbortSignal\.timeout\((\d+)\)", client)
        self.assertIsNotNone(window)
        self.assertIsNotNone(abort)
        outer_ms = int(outer.group(1)) * 1000
        minimum_ms = int(window.group(1)) + 2 * int(abort.group(1))
        self.assertGreater(outer_ms, minimum_ms)
        self.assertLess(outer_ms, minimum_ms + 120_000)
        self.assertIn(f"timed out after {outer.group(1)} seconds", run)

    def test_staging_runs_unconditionally_for_push_and_dispatch(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(staging_dispatch_errors(staging), [])
        self.assertEqual(staging_active_errors(staging), [])
        self.assertEqual(schema_gate_errors(staging), [])
        self.assertEqual(workflow_policy_errors(self.workflows), [])
        self.assertEqual(staging["jobs"]["deploy"]["environment"], "staging")
        self.assertNotIn("if", staging["jobs"]["deploy"])

    def test_any_job_condition_on_staging_deploy_fails(self):
        # The live deploy job carries no `if` at all: a job-level condition
        # would silently skip deploys on some SHAs (including the old
        # `if: ${{ false }}` suspension), leaving the production guard
        # refusing those SHAs with no deploy ever running.
        for guard in ("${{ false }}", "true", "${{ true }}", "${{ vars.ENABLE_STAGING }}",
                      "${{ github.ref == 'refs/heads/main' }}", "${{ inputs.deploy }}",
                      "${{ always() }}"):
            with self.subTest(guard=guard):
                workflows = deepcopy(self.workflows)
                workflows["deploy-staging.yml"]["jobs"]["deploy"]["if"] = guard
                self.assertTrue(workflow_policy_errors(workflows))
                self.assertTrue(staging_active_errors(workflows["deploy-staging.yml"]))

    def test_dispatch_inputs_cannot_arm_the_release_fence(self):
        # The workflow_dispatch inputs block must not gain an arming switch:
        # an attacker-readable input the steps could consult would be an
        # unreviewed deploy route outside the pinned dispatch shape. Each
        # mutation below must trip staging_dispatch_errors (surfaced through
        # workflow_policy_errors on the real inventory).
        for dispatch in ({"inputs": {"deploy": {"description": "deploy now", "type": "boolean", "default": False}}},
                         {"inputs": {"release_fence": {"description": "x", "type": "boolean", "default": True}}},
                         {"inputs": {"enable": {"description": "x", "type": "boolean", "default": False}}},
                         {"inputs": {}},
                         "just-a-string"):
            with self.subTest(dispatch=dispatch):
                workflows = deepcopy(self.workflows)
                workflows["deploy-staging.yml"]["on"]["workflow_dispatch"] = dispatch
                self.assertTrue(workflow_policy_errors(workflows))

    def test_gateway_failure_injection_is_dispatch_only_and_never_forwarded(self):
        workflows = self.workflows
        self.assertEqual(staging_gateway_injection_errors(workflows), [])
        self.assertEqual(staging_dispatch_errors(workflows["deploy-staging.yml"]), [])
        steps = workflows["deploy-staging.yml"]["jobs"]["deploy"]["steps"]
        armed = [s for s in steps if "INJECT_GATEWAY_FAILURE" in str(s)]
        self.assertEqual(len(armed), 1)
        index = steps.index(armed[0])
        mutations = {
            # Push could read it / default armed.
            "expression without dispatch guard": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_GATEWAY_FAILURE", "${{ inputs.inject_gateway_failure }}"),
            "expression constant true": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_GATEWAY_FAILURE", "true"),
            "prepare flag unconditional": lambda w: w["jobs"]["deploy"]["steps"][index]
                .__setitem__("run", "python3 ../scripts/staging_rollout.py prepare --inject-gateway-failure"),
            "input defaults true": lambda w: w["on"]["workflow_dispatch"]["inputs"]["inject_gateway_failure"]
                .__setitem__("default", True),
            "input removed": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("inject_gateway_failure"),
            "second reader step": lambda w: w["jobs"]["deploy"]["steps"][0]
                .setdefault("env", {}).__setitem__("INJECT_GATEWAY_FAILURE", "true"),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(
                    staging_gateway_injection_errors(changed)
                    or staging_dispatch_errors(changed["deploy-staging.yml"]),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)
        # Production (or any other workflow) mentioning the switch at all
        # is a forwarding route, whatever the surrounding shape.
        for name in ("deploy-production.yml", "check.yml"):
            with self.subTest(forwarded=name):
                changed = deepcopy(workflows)
                job = next(iter(changed[name]["jobs"].values()))
                job.setdefault("env", {})["FORWARD"] = "${{ inputs.inject_gateway_failure }}"
                self.assertTrue(staging_gateway_injection_errors(changed))
                self.assertTrue(workflow_policy_errors(changed))

    def test_error_rate_gate_is_dispatch_gated_and_never_forwarded(self):
        workflows = self.workflows
        self.assertEqual(staging_error_rate_gate_errors(workflows), [])
        self.assertEqual(staging_dispatch_errors(workflows["deploy-staging.yml"]), [])
        steps = workflows["deploy-staging.yml"]["jobs"]["deploy"]["steps"]
        armed = [s for s in steps if "INJECT_ERROR_RATE_SPIKE" in str(s)]
        self.assertEqual(len(armed), 1)
        self.assertEqual(armed[0].get("id"), "error_rate_gate")
        index = steps.index(armed[0])
        mutations = {
            # Push could read it / default armed.
            "expression without dispatch guard": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_ERROR_RATE_SPIKE", "${{ inputs.inject_error_rate_spike }}"),
            "expression constant true": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_ERROR_RATE_SPIKE", "true"),
            "rehearsal flag unconditional": lambda w: w["jobs"]["deploy"]["steps"][index]
                .__setitem__("run", "node scripts/promotion-error-gate.mjs --rehearsal-firing '[\"rest_5xx_rate\"]'"),
            "input defaults true": lambda w: w["on"]["workflow_dispatch"]["inputs"]["inject_error_rate_spike"]
                .__setitem__("default", True),
            "input removed": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("inject_error_rate_spike"),
            "second reader step": lambda w: w["jobs"]["deploy"]["steps"][0]
                .setdefault("env", {}).__setitem__("INJECT_ERROR_RATE_SPIKE", "true"),
            "switch off the gate step": lambda w: w["jobs"]["deploy"]["steps"][index]
                .__setitem__("id", "unrelated"),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(
                    staging_error_rate_gate_errors(changed)
                    or staging_dispatch_errors(changed["deploy-staging.yml"]),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)
        # Production has no fault input at all. Reintroducing even an unarmed
        # input, forwarding a switch or weakening automatic recovery fails.
        self.assertEqual(production_error_rate_gate_errors(workflows["deploy-production.yml"]), [])
        prod_steps = workflows["deploy-production.yml"]["jobs"]["production"]["steps"]
        self.assertEqual([s for s in prod_steps if "INJECT_ERROR_RATE_SPIKE" in str(s)], [])
        gate = [s for s in prod_steps if s.get("id") == "error_rate_gate"]
        self.assertEqual(len(gate), 1)
        prod_index = prod_steps.index(gate[0])
        prod_mutations = {
            "drill switch re-armed on the gate": lambda w: w["jobs"]["production"]["steps"][prod_index]["env"]
                .__setitem__("INJECT_ERROR_RATE_SPIKE", ERROR_RATE_EXPRESSION),
            "drill switch constant true": lambda w: w["jobs"]["production"]["steps"][prod_index]["env"]
                .__setitem__("INJECT_ERROR_RATE_SPIKE", "true"),
            "rehearsal flag on the gate": lambda w: w["jobs"]["production"]["steps"][prod_index]
                .__setitem__("run", str(w["jobs"]["production"]["steps"][prod_index]["run"])
                             + " --rehearsal-firing '[\"rest_5xx_rate\"]'"),
            "retired input returns": lambda w: w["on"]["workflow_dispatch"]["inputs"].update(
                {"inject_error_rate_spike": {"type": "boolean", "default": False}}),
            "renamed fault input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update(
                {"drill": {"type": "boolean", "default": False}}),
            "another production step reads it": lambda w: w["jobs"]["production"]["steps"][0]
                .setdefault("env", {}).__setitem__("SPIKE", "${{ inputs.inject_error_rate_spike }}"),
            "gate step removed": lambda w: w["jobs"]["production"]["steps"][prod_index]
                .__setitem__("id", "unrelated"),
            "gate without the deploy-mode condition": lambda w: w["jobs"]["production"]["steps"][prod_index]
                .pop("if"),
            "staging scrape token": lambda w: w["jobs"]["production"]["steps"][prod_index]["env"]
                .__setitem__("METRICS_SCRAPE_TOKEN", "${{ secrets.STAGING_METRICS_SCRAPE_TOKEN }}"),
            "tooling from the guarded SHA": lambda w: w["jobs"]["production"]["steps"][prod_index]
                .__setitem__("run", str(w["jobs"]["production"]["steps"][prod_index]["run"])
                             .replace("../tooling/wrangler/scripts/promotion-error-gate.mjs",
                                      "wrangler/scripts/promotion-error-gate.mjs")),
        }
        for label, mutate in prod_mutations.items():
            with self.subTest(label):
                changed = deepcopy(workflows)
                mutate(changed["deploy-production.yml"])
                self.assertTrue(
                    production_error_rate_gate_errors(changed["deploy-production.yml"]),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)
        # Any other workflow mentioning the gate at all is a forwarding
        # route, whatever the surrounding shape.
        for name in ("check.yml",):
            with self.subTest(forwarded=name):
                changed = deepcopy(workflows)
                job = next(iter(changed[name]["jobs"].values()))
                job.setdefault("env", {})["FORWARD"] = "${{ inputs.inject_error_rate_spike }}"
                self.assertTrue(staging_error_rate_gate_errors(changed))
                self.assertTrue(workflow_policy_errors(changed))

    def test_production_fault_inputs_absent_and_recovery_chain_stays_red(self):
        text = (ROOT / ".github/workflows/deploy-production.yml").read_text()
        self.assertNotIn("inject_error_rate_spike", text.lower())
        self.assertNotIn("inject_gateway_failure", text.lower())
        self.assertNotIn("--rehearsal-firing", text)
        original = self.workflows["deploy-production.yml"]
        self.assertEqual(production_error_rate_gate_errors(original), [])

        def step(workflow, name):
            return next(s for s in workflow["jobs"]["production"]["steps"] if s.get("id") == name)

        for label, change in {
            "snapshot after deploy": lambda w: w["jobs"]["production"]["steps"].remove(step(w, "snapshot")),
            "recovery without failure": lambda w: step(w, "auto_rollback").update({"if": "always()"}),
            "recovery without snapshot": lambda w: step(w, "auto_rollback").update({"run": "echo recovered"}),
            "worker-only restore": lambda w: step(w, "auto_rollback").update({"run": "wrangler rollback"}),
            "gate tolerates failure": lambda w: step(w, "gate").update({"continue-on-error": True}),
            "error gate tolerates failure": lambda w: step(w, "error_rate_gate").update({"continue-on-error": True}),
            "job tolerates failure": lambda w: w["jobs"]["production"].update({"continue-on-error": True}),
            "dispatch recovery": lambda w: step(w, "auto_rollback").update({"run": "gh workflow run deploy-production.yml"}),
        }.items():
            changed = deepcopy(original)
            change(changed)
            with self.subTest(label=label):
                self.assertTrue(production_error_rate_gate_errors(changed))

    def test_remediation_fault_injection_is_dispatch_only_and_never_forwarded(self):
        workflows = self.workflows
        self.assertEqual(staging_fault_injection_errors(workflows), [])
        self.assertEqual(staging_dispatch_errors(workflows["deploy-staging.yml"]), [])
        steps = workflows["deploy-staging.yml"]["jobs"]["deploy"]["steps"]
        armed = [s for s in steps if "INJECT_VOICE_STALL" in str(s)]
        self.assertEqual(len(armed), 1)
        index = steps.index(armed[0])
        mutations = {
            # Push could read it / default armed.
            "expression without dispatch guard": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_VOICE_STALL", "${{ inputs.inject_voice_stall }}"),
            "expression constant true": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_UNPAIRED_DISCONNECT", "true"),
            "prepare flag unconditional": lambda w: w["jobs"]["deploy"]["steps"][index]
                .__setitem__("run", "python3 ../scripts/staging_rollout.py prepare --inject-voice-stall"),
            "input defaults true": lambda w: w["on"]["workflow_dispatch"]["inputs"]["inject_voice_stall"]
                .__setitem__("default", True),
            "input removed": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("inject_unpaired_disconnect"),
            "second reader step": lambda w: w["jobs"]["deploy"]["steps"][0]
                .setdefault("env", {}).__setitem__("INJECT_VOICE_STALL", "true"),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(
                    staging_fault_injection_errors(changed)
                    or staging_dispatch_errors(changed["deploy-staging.yml"]),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)
        # Production (or any other workflow) mentioning any fault marker at
        # all is a forwarding route, whatever the surrounding shape.
        for name in ("deploy-production.yml", "check.yml"):
            with self.subTest(forwarded=name):
                changed = deepcopy(workflows)
                job = next(iter(changed[name]["jobs"].values()))
                job.setdefault("env", {})["FORWARD"] = "${{ inputs.inject_voice_stall }}"
                self.assertTrue(staging_fault_injection_errors(changed))
                self.assertTrue(workflow_policy_errors(changed))

    def test_pinned_sha_is_dispatch_only_and_ancestor_checked(self):
        workflows = self.workflows
        staging = workflows["deploy-staging.yml"]
        self.assertEqual(staging_pinned_sha_errors(workflows), [])
        self.assertEqual(staging_dispatch_errors(staging), [])
        for job_id in ("schema-gate", "roles-gate", "deploy"):
            job = staging["jobs"][job_id]
            checkout = next(step for step in job["steps"]
                            if str(step.get("uses", "")).startswith("actions/checkout@"))
            self.assertEqual(checkout["with"]["ref"], DEPLOY_SHA_EXPRESSION)
            self.assertEqual(checkout["with"]["fetch-depth"], "0")
            sha_step = next(step for step in job["steps"]
                            if step.get("name") == "Deploy SHA must be exact and reachable from main")
            self.assertEqual(sha_step["env"]["SOURCE_SHA"], DEPLOY_SHA_EXPRESSION)
        gate_run = next(step for step in staging["jobs"]["schema-gate"]["steps"]
                        if "staging_schema_gate.py" in str(step.get("run", "")))
        self.assertEqual(gate_run["env"]["SOURCE_SHA"], DEPLOY_SHA_EXPRESSION)

        # Deploy step indices: the workflow-head tooling checkout sits at [1],
        # so the SHA step is [2] there (schema-gate and roles-gate are
        # unchanged at [1]).
        def swap_shape_after_equality(workflow):
            run = workflow["jobs"]["deploy"]["steps"][2]["run"]
            lines = run.splitlines()
            case_i = next(i for i, line in enumerate(lines) if line.strip().startswith("case "))
            test_i = next(i for i, line in enumerate(lines) if "git rev-parse HEAD" in line)
            lines[case_i], lines[test_i] = lines[test_i], lines[case_i]
            workflow["jobs"]["deploy"]["steps"][2]["run"] = "\n".join(lines)

        mutations = {
            # A push (or an empty dispatch) must deploy the workflow head, never a constant.
            "checkout ref constant": lambda w: w["jobs"]["deploy"]["steps"][0]["with"]
                .__setitem__("ref", "main"),
            "checkout ref without dispatch guard": lambda w: w["jobs"]["deploy"]["steps"][0]["with"]
                .__setitem__("ref", "${{ inputs.sha }}"),
            "checkout ref removed": lambda w: w["jobs"]["deploy"]["steps"][0]["with"].pop("ref"),
            "deploy checkout shallow": lambda w: w["jobs"]["deploy"]["steps"][0]["with"]
                .__setitem__("fetch-depth", "1"),
            "SHA env constant": lambda w: w["jobs"]["deploy"]["steps"][2]["env"]
                .__setitem__("SOURCE_SHA", "abc"),
            "ancestry check dropped": lambda w: w["jobs"]["deploy"]["steps"][2]
                .__setitem__("run", "\n".join(line for line in
                                             w["jobs"]["deploy"]["steps"][2]["run"].splitlines()
                                             if "merge-base" not in line)),
            "ancestry rejection goes green": lambda w: w["jobs"]["deploy"]["steps"][2]
                .__setitem__("run", w["jobs"]["deploy"]["steps"][2]["run"]
                             .replace("exit 1", "exit 0")),
            "ancestry-only rejection goes green": lambda w: [
                job["steps"][2 if job is w["jobs"]["deploy"] else 1].__setitem__(
                    "run", job["steps"][2 if job is w["jobs"]["deploy"] else 1]["run"].replace(
                        'git merge-base --is-ancestor "$SOURCE_SHA" origin/main || '
                        '{ echo "::error::$SOURCE_SHA is not an ancestor of origin/main" >&2; exit 1; }',
                        'git merge-base --is-ancestor "$SOURCE_SHA" origin/main || '
                        '{ echo "::error::$SOURCE_SHA is not an ancestor of origin/main" >&2; exit 0; }'))
                for job in (w["jobs"]["schema-gate"], w["jobs"]["roles-gate"], w["jobs"]["deploy"])],
            "hex enforcement dropped": lambda w: w["jobs"]["deploy"]["steps"][2]
                .__setitem__("run", "\n".join(line for line in
                                             w["jobs"]["deploy"]["steps"][2]["run"].splitlines()
                                             if "${#SOURCE_SHA}" not in line)),
            "shape check after equality": swap_shape_after_equality,
            "second sha reader": lambda w: w["jobs"]["deploy"]["steps"][3]
                .setdefault("env", {}).__setitem__("EXTRA_SHA", DEPLOY_SHA_EXPRESSION),
            "sha input removed": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("sha"),
            "sha input boolean": lambda w: w["on"]["workflow_dispatch"]["inputs"]["sha"]
                .__setitem__("type", "boolean"),
            "sha default armed": lambda w: w["on"]["workflow_dispatch"]["inputs"]["sha"]
                .__setitem__("default", "abc"),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(
                    staging_pinned_sha_errors(changed)
                    or staging_dispatch_errors(changed["deploy-staging.yml"]),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)
        # Any other workflow mentioning the switch is a forwarding route,
        # except deploy-production and release, which carry their own
        # reviewed sha inputs.
        for name in ("check.yml", "staging-migrate.yml"):
            with self.subTest(forwarded=name):
                changed = deepcopy(workflows)
                job = next(iter(changed[name]["jobs"].values()))
                job.setdefault("env", {})["FORWARD"] = "${{ inputs.sha }}"
                self.assertTrue(staging_pinned_sha_errors(changed))
                self.assertTrue(workflow_policy_errors(changed))

    def test_soak_hold_defers_push_and_never_skips_silently(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(soak_hold_errors(staging), [])
        self.assertEqual(schema_gate_errors(staging), [])
        self.assertEqual(roles_gate_errors(staging), [])
        self.assertEqual(staging["jobs"]["schema-gate"].get("needs"), "soak-hold")
        self.assertEqual(staging["jobs"]["roles-gate"].get("needs"), "soak-hold")
        # A hold never becomes a quiet green: the gates and the deploy carry
        # no `if`, so a failed hold skips them and fails the run red, which
        # the production guard refuses as staging history.
        for job_id in ("soak-hold", "schema-gate", "roles-gate", "deploy"):
            self.assertNotIn("if", staging["jobs"][job_id])
        mutations = {
            "hold job removed": lambda w: w["jobs"].pop("soak-hold"),
            "hold gains a condition": lambda w: w["jobs"]["soak-hold"].__setitem__("if", "true"),
            "hold waits on a gate": lambda w: w["jobs"]["soak-hold"].__setitem__("needs", "deploy"),
            "hold reads a secret": lambda w: w["jobs"]["soak-hold"]["steps"][0]["env"]
                .__setitem__("TOKEN", "${{ secrets.X }}"),
            "hold reads an input": lambda w: w["jobs"]["soak-hold"]["steps"][0]["env"]
                .__setitem__("FLAG", "${{ inputs.sha }}"),
            "deadline var renamed": lambda w: w["jobs"]["soak-hold"]["steps"][0]["env"]
                .__setitem__("HOLD_UNTIL", "${{ vars.OTHER }}"),
            "max window widened": lambda w: w["jobs"]["soak-hold"]["steps"][0]
                .__setitem__("run", w["jobs"]["soak-hold"]["steps"][0]["run"].replace("14400", "99999")),
            "summary dropped": lambda w: w["jobs"]["soak-hold"]["steps"][0]
                .__setitem__("run", "\n".join(line for line in
                                             w["jobs"]["soak-hold"]["steps"][0]["run"].splitlines()
                                             if "GITHUB_STEP_SUMMARY" not in line)),
            "deferral goes green": lambda w: w["jobs"]["soak-hold"]["steps"][0]
                .__setitem__("run", w["jobs"]["soak-hold"]["steps"][0]["run"].replace("exit 1", "exit 0")),
            "gate stops waiting": lambda w: w["jobs"]["schema-gate"].pop("needs"),
            "gate waits on deploy": lambda w: w["jobs"]["schema-gate"].__setitem__("needs", "deploy"),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(self.workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(
                    soak_hold_errors(changed["deploy-staging.yml"])
                    or schema_gate_errors(changed["deploy-staging.yml"])
                    or roles_gate_errors(changed["deploy-staging.yml"]),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)

    def test_user_flow_synthetic_gates_certification_on_the_exact_sha(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(user_flow_synthetic_errors(staging), [])
        probe = lambda w: next(step for step in w["jobs"]["user-flow-synthetic"]["steps"]
                               if "staging_health_contract_probe.py" in str(step.get("run", "")))
        mutations = {
            "renamed": lambda w: w["jobs"]["user-flow-synthetic"].__setitem__("name", "synthetic"),
            "conditional": lambda w: w["jobs"]["user-flow-synthetic"].__setitem__("if", "false"),
            "tolerated": lambda w: probe(w).__setitem__("continue-on-error", "true"),
            "head sha only": lambda w: probe(w)["env"].__setitem__("EXPECTED_SHA", "${{ github.sha }}"),
            "no sha pin": lambda w: probe(w).__setitem__(
                "run", probe(w)["run"].replace('--expected-sha "$EXPECTED_SHA" ', "")),
            "empty origin passes": lambda w: probe(w).__setitem__(
                "run", probe(w)["run"].replace("exit 1", "exit 0")),
            "secret read": lambda w: probe(w)["env"].__setitem__("TOKEN", "${{ secrets.X }}"),
            "write grant": lambda w: w["jobs"]["user-flow-synthetic"].__setitem__(
                "permissions", {"contents": "write"}),
            "certify ignores it": lambda w: w["jobs"]["certify-promotion"].__setitem__(
                "needs", ["deploy", "voice-synthetic", "pinned-history-guard"]),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(self.workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(user_flow_synthetic_errors(changed["deploy-staging.yml"]), label)
                self.assertTrue(workflow_policy_errors(changed), label)

    def test_soak_hold_shell_behaves(self):
        # Execute the actual hold-check shell across its six cases: dispatch
        # and empty bypass, expiry fails open, armed defers, unparsable and
        # over-long fail closed. Deadlines sit minutes away from the real
        # clock, far from any comparison boundary.
        import datetime
        import os
        import subprocess
        import tempfile
        step = self.workflows["deploy-staging.yml"]["jobs"]["soak-hold"]["steps"][0]
        shell = step["run"]
        now = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0)
        stamp = lambda moment: moment.strftime("%Y-%m-%dT%H:%M:%SZ")
        cases = (
            ("dispatch bypasses the hold", "workflow_dispatch", stamp(now + datetime.timedelta(hours=1)), 0, ""),
            ("empty var means no hold", "push", "", 0, ""),
            ("expired hold fails open", "push", stamp(now - datetime.timedelta(minutes=1)), 0, "expired"),
            ("armed hold defers", "push", stamp(now + datetime.timedelta(hours=1)), 1, "deferred"),
            ("unparsable hold fails closed", "push", "not-a-timestamp", 1, ""),
            ("over-long hold fails closed", "push", stamp(now + datetime.timedelta(hours=5)), 1, ""),
        )
        for label, event, hold_until, expected_code, summary_needle in cases:
            with self.subTest(label):
                with tempfile.TemporaryDirectory() as temp_dir:
                    summary = Path(temp_dir) / "summary.md"
                    summary.write_text("", encoding="utf-8")
                    env = dict(os.environ, EVENT=event, HOLD_UNTIL=hold_until,
                               GITHUB_STEP_SUMMARY=str(summary))
                    proc = subprocess.run(
                        ["bash", "-c", shell], cwd=str(ROOT), env=env,
                        capture_output=True, text=True, timeout=10,
                    )
                    output = proc.stdout + proc.stderr
                    self.assertEqual(proc.returncode, expected_code, output)
                    body = summary.read_text(encoding="utf-8")
                    if summary_needle:
                        self.assertIn(summary_needle, body)
                        self.assertIn(hold_until, body)
                    else:
                        self.assertEqual(body, "")
                    if expected_code == 1:
                        self.assertIn("::error::", output)

    def test_pinned_drill_deploy_stays_red(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(pinned_history_guard_errors(staging), [])
        self.assertEqual(staging["jobs"]["pinned-history-guard"].get("needs"), "voice-synthetic")
        self.assertNotIn("if", staging["jobs"]["pinned-history-guard"])
        # The guard judges after the synthetic ran: a pinned drill still
        # deploys and still runs the synthetic; only the run verdict is red,
        # so the pin never reads as staging history for the workflow head.
        mutations = {
            "guard job removed": lambda w: w["jobs"].pop("pinned-history-guard"),
            "guard gains a condition": lambda w: w["jobs"]["pinned-history-guard"]
                .__setitem__("if", "true"),
            "guard stops waiting": lambda w: w["jobs"]["pinned-history-guard"].pop("needs"),
            "guard waits on deploy": lambda w: w["jobs"]["pinned-history-guard"]
                .__setitem__("needs", "deploy"),
            "guard reads a secret": lambda w: w["jobs"]["pinned-history-guard"]["steps"][0]["env"]
                .__setitem__("TOKEN", "${{ secrets.X }}"),
            "guard compares against a constant": lambda w: w["jobs"]["pinned-history-guard"]["steps"][0]["env"]
                .__setitem__("HEAD_SHA", "abc"),
            "head comparison dropped": lambda w: w["jobs"]["pinned-history-guard"]["steps"][0]
                .__setitem__("run", "\n".join(line for line in
                                             w["jobs"]["pinned-history-guard"]["steps"][0]["run"].splitlines()
                                             if '"$HEAD_SHA"' not in line)),
            "verdict goes green": lambda w: w["jobs"]["pinned-history-guard"]["steps"][0]
                .__setitem__("run", w["jobs"]["pinned-history-guard"]["steps"][0]["run"]
                             .replace("exit 1", "exit 0")),
            "raw second reader": lambda w: w["jobs"]["deploy"]["steps"][2]
                .setdefault("env", {}).__setitem__("EXTRA", "${{ inputs.sha }}"),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(self.workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(
                    pinned_history_guard_errors(changed["deploy-staging.yml"])
                    or staging_pinned_sha_errors(changed),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)

    def test_pinned_history_guard_shell_behaves(self):
        # Execute the actual guard shell: an empty pin and a pin equal to the
        # head pass (the deploy served the head), a pin below the head fails
        # with the reason naming both SHAs.
        import os
        import subprocess
        import tempfile
        step = self.workflows["deploy-staging.yml"]["jobs"]["pinned-history-guard"]["steps"][0]
        shell = step["run"]
        head, pin = "a" * 40, "b" * 40
        cases = (
            ("empty pin deploys the head", "", head, 0),
            ("pin equal to the head deploys the head", head, head, 0),
            ("pin below the head stays red", pin, head, 1),
        )
        for label, pinned, head_sha, expected_code in cases:
            with self.subTest(label):
                with tempfile.TemporaryDirectory() as temp_dir:
                    env = dict(os.environ, PINNED_SHA=pinned, HEAD_SHA=head_sha)
                    proc = subprocess.run(
                        ["bash", "-c", shell], cwd=str(ROOT), env=env,
                        capture_output=True, text=True, timeout=10,
                    )
                    output = proc.stdout + proc.stderr
                    self.assertEqual(proc.returncode, expected_code, output)
                    if expected_code == 1:
                        self.assertIn("not staging history", output)
                        self.assertIn(pinned, output)
                        self.assertIn(head_sha, output)

    def test_pinned_sha_shell_behaves(self):
        # Execute the actual SHA-check shells in a scratch git repo: a valid
        # ancestor passes and (in the deploy job) exports DEPLOY_SHA, while a
        # short, uppercase, empty or branch pin, a non-ancestor and a
        # HEAD-mismatch all refuse with their reason. The temp remote's
        # origin/main is the ancestry root; each case checks out its commit
        # first, exactly as the workflow checkout lands on the pin before the
        # SHA step runs. The deploy shell runs from a subdirectory, mirroring
        # the deploy job's wrangler working directory.
        import os
        import subprocess
        import tempfile
        shells = {}
        for job_id in ("schema-gate", "roles-gate", "deploy"):
            job = self.workflows["deploy-staging.yml"]["jobs"][job_id]
            shells[job_id] = next(
                step for step in job["steps"]
                if step.get("name") == "Deploy SHA must be exact and reachable from main")["run"]
        with tempfile.TemporaryDirectory() as temp_dir:
            temp = Path(temp_dir)
            repo, origin = temp / "repo", temp / "origin.git"
            # Hermetic git: caller identity overrides (even empty ones),
            # ambient config paths and shell-startup injections (BASH_ENV,
            # ENV) must not leak into the scratch repo or redirect git.
            base_env = {key: value for key, value in os.environ.items()
                        if not (key.startswith("GIT_CONFIG_KEY_") or key.startswith("GIT_CONFIG_VALUE_"))}
            for key in ("GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL",
                        "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL",
                        "GIT_CONFIG_COUNT", "BASH_ENV", "ENV"):
                base_env.pop(key, None)
            base_env.update(HOME=str(temp),
                            GIT_CONFIG_GLOBAL=str(temp / "gitconfig"),
                            GIT_CONFIG_SYSTEM=str(temp / "gitconfig-system"))
            # The SHA shells start with `git config --global`, which a
            # managed/shimmed git forbids; probe that capability and skip
            # there (the static needle, exit-count and mutation pins above
            # still run everywhere). Real runners execute the shells.
            probe = subprocess.run(
                ["git", "config", "--global", "--add", "paperclip.probe", "1"],
                cwd=str(temp), env=base_env,
                capture_output=True, text=True, timeout=30)
            if probe.returncode != 0:
                self.skipTest("git cannot write global config here; "
                              "SHA shells execute on real runners only")

            def git(*args, cwd):
                return subprocess.run(
                    ["git", "-c", "user.email=test@example.com", "-c", "user.name=test",
                     "-c", "init.defaultBranch=main", *args],
                    cwd=str(cwd), env=base_env,
                    capture_output=True, text=True, timeout=30, check=True)

            git("init", "--bare", origin.name, cwd=temp)
            git("init", repo.name, cwd=temp)
            (repo / "file.txt").write_text("ancestor\n", encoding="utf-8")
            git("add", "file.txt", cwd=repo)
            git("commit", "-m", "ancestor", cwd=repo)
            ancestor = git("rev-parse", "HEAD", cwd=repo).stdout.strip()
            (repo / "file.txt").write_text("head\n", encoding="utf-8")
            git("commit", "-am", "head", cwd=repo)
            head = git("rev-parse", "HEAD", cwd=repo).stdout.strip()
            git("remote", "add", "origin", str(origin), cwd=repo)
            git("push", "origin", "main", cwd=repo)
            git("checkout", "-b", "feature", ancestor, cwd=repo)
            (repo / "feature.txt").write_text("unmerged\n", encoding="utf-8")
            git("add", "feature.txt", cwd=repo)
            git("commit", "-m", "unmerged", cwd=repo)
            unmerged = git("rev-parse", "HEAD", cwd=repo).stdout.strip()
            (repo / "wrangler").mkdir()
            cases = (
                ("valid ancestor passes", head, head, 0, ""),
                ("short pin refuses", head, "abc123", 1, "full 40-hex"),
                ("uppercase pin refuses", head, head.upper(), 1, "full 40-hex"),
                ("empty pin refuses", head, "", 1, "full 40-hex"),
                ("branch pin refuses", head, "main", 1, "full 40-hex"),
                ("non-ancestor refuses", unmerged, unmerged, 1, "not an ancestor"),
                ("head mismatch refuses", head, ancestor, 1, "did not land"),
            )
            for label, checkout_ref, source_sha, expected_code, needle in cases:
                with self.subTest(label):
                    git("checkout", checkout_ref, cwd=repo)
                    for job_id, shell in shells.items():
                        with self.subTest(job=job_id):
                            exported = temp / f"github-env-{job_id}"
                            exported.write_text("", encoding="utf-8")
                            env = dict(base_env, GITHUB_WORKSPACE=str(repo),
                                       SOURCE_SHA=source_sha, GITHUB_ENV=str(exported))
                            workdir = repo / "wrangler" if job_id == "deploy" else repo
                            proc = subprocess.run(
                                ["bash", "-e", "-c", shell], cwd=str(workdir), env=env,
                                capture_output=True, text=True, timeout=30,
                            )
                            output = proc.stdout + proc.stderr
                            self.assertEqual(proc.returncode, expected_code, output)
                            if expected_code == 1:
                                self.assertIn("::error::", output)
                                self.assertIn(needle, output)
                                self.assertEqual(exported.read_text(encoding="utf-8"), "")
                            elif job_id == "deploy":
                                self.assertEqual(exported.read_text(encoding="utf-8"),
                                                 f"DEPLOY_SHA={source_sha}\n")
                            else:
                                self.assertEqual(exported.read_text(encoding="utf-8"), "")

    def test_deploy_identity_follows_the_pin(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(staging_deploy_identity_errors(staging), [])

        def sha_step(w):
            return next(step for step in w["jobs"]["deploy"]["steps"]
                        if step.get("name") == "Deploy SHA must be exact and reachable from main")

        def prepare_step(w):
            return next(step for step in w["jobs"]["deploy"]["steps"]
                        if step.get("name") == "Snapshot staging identity and generate per-deploy image provenance")

        def step_by_id(w, step_id):
            return next(step for step in w["jobs"]["deploy"]["steps"]
                        if step.get("id") == step_id)

        mutations = {
            "identity export dropped": lambda w: sha_step(w)
                .__setitem__("run", "\n".join(line for line in sha_step(w)["run"].splitlines()
                                             if "DEPLOY_SHA" not in line)),
            "prepare stamps the head": lambda w: prepare_step(w)["env"]
                .__setitem__("GITHUB_SHA", "${{ github.sha }}"),
            "deploy message stamps the head": lambda w: step_by_id(w, "deploy")["with"]
                .__setitem__("command", step_by_id(w, "deploy")["with"]["command"]
                             .replace("${{ env.DEPLOY_SHA }}", "${{ github.sha }}")),
            "ownership actor stamps the head": lambda w: step_by_id(w, "takeover")["env"]
                .__setitem__("OWNERSHIP_ACTOR", "github-actions:${{ github.run_id }}:${{ github.sha }}"),
            "promotion gate stamps the head": lambda w: step_by_id(w, "error_rate_gate")["env"]
                .__setitem__("SHA", "${{ github.sha }}"),
            "rollback actor stamps the head": lambda w: step_by_id(w, "auto_rollback")["env"]
                .__setitem__("ROLLBACK_ACTOR", "github-actions:${{ github.run_id }}:${{ github.sha }}"),
            "second head reader": lambda w: prepare_step(w)["env"]
                .__setitem__("EXTRA_SHA", "${{ github.sha }}"),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(self.workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(
                    staging_deploy_identity_errors(changed["deploy-staging.yml"])
                    or staging_pinned_sha_errors(changed),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)

    def test_docs_only_push_skips_staging_but_runtime_paths_deploy(self):
        # A docs-only merge to main must start no run (the soak needs a stable
        # gateway), while every runtime path still deploys. Pinned against
        # sample paths with GitHub's filter semantics, not just the pattern text.
        push = self.workflows["deploy-staging.yml"]["on"]["push"]
        self.assertEqual(push["branches"], ["main"])
        self.assertNotIn("paths", push)
        for path in NON_RUNTIME_PATHS:
            with self.subTest(skipped=path):
                self.assertTrue(any(filter_pattern_matches(p, path) for p in push["paths-ignore"]))
        for path in RUNTIME_PATHS:
            with self.subTest(deploys=path):
                self.assertFalse(any(filter_pattern_matches(p, path) for p in push["paths-ignore"]))
        # Manual dispatch is unfiltered.
        self.assertEqual(set(self.workflows["deploy-staging.yml"]["on"]["workflow_dispatch"]), {"inputs"})

    def test_push_filter_cannot_widen_into_runtime_or_become_an_allow_list(self):
        ignore = self.workflows["deploy-staging.yml"]["on"]["push"]["paths-ignore"]
        mutations = {
            "allow-list": {"branches": ["main"], "paths": ["docs/**"]},
            "no filter list": {"branches": ["main"], "paths-ignore": []},
            "other branches": {"branches": ["main", "release/**"], "paths-ignore": ignore},
            "tags": {"branches": ["main"], "tags": ["v*"], "paths-ignore": ignore},
        }
        for widened in ("**", "*", "*.json", "**/*.md", "crates/**", "wrangler/**", "scripts/**",
                        ".github/**", ".github/workflows/**", "sql/**", "Cargo.*", "Dockerfile*",
                        "src/**", "deploy/**", "tests/**"):
            mutations[f"ignore {widened}"] = {"branches": ["main"], "paths-ignore": ignore + [widened]}
        for label, push in mutations.items():
            with self.subTest(label):
                workflows = deepcopy(self.workflows)
                workflows["deploy-staging.yml"]["on"]["push"] = push
                self.assertTrue(staging_dispatch_errors(workflows["deploy-staging.yml"]))
                self.assertTrue(workflow_policy_errors(workflows))

    def test_step_level_skip_is_not_a_deploy(self):
        # Neutering the live job step-by-step would report success without
        # deploying, which the production guard would then accept as a
        # successful staging run. Any step-level static disable fails closed.
        workflows = deepcopy(self.workflows)
        job = workflows["deploy-staging.yml"]["jobs"]["deploy"]
        for step in job["steps"]:
            step["if"] = STATIC_FALSE
        self.assertTrue(workflow_policy_errors(workflows))

    def test_new_job_or_workflow_is_not_an_unapproved_deploy_route(self):
        for name in self.workflows:
            with self.subTest(name=name):
                workflows = deepcopy(self.workflows)
                workflows[name]["jobs"]["alternate"] = {"uses": "./.github/workflows/deploy-staging.yml"}
                self.assertTrue(workflow_policy_errors(workflows))
        workflows = deepcopy(self.workflows)
        workflows["alternate.yml"] = {"jobs": {"deploy": {"run": "wrangler deploy"}}}
        self.assertTrue(workflow_policy_errors(workflows))

    def test_retargeting_known_job_fails(self):
        for change in ({"environment": "staging"}, {"uses": "./.github/workflows/deploy-staging.yml"},
                       {"steps": [{"run": 'curl "$STAGING_WORKER_URL/readyz"'}]},
                       {"steps": [{"uses": "cloudflare/wrangler-action@pinned"}]}):
            with self.subTest(change=change):
                workflows = deepcopy(self.workflows)
                workflows["check.yml"]["jobs"]["worker"].update(change)
                self.assertTrue(workflow_policy_errors(workflows))

    def test_reusable_call_cannot_be_retargeted(self):
        for target in ("./.github/workflows/deploy-staging.yml", "./.github/workflows/deploy-production.yml",
                       "org/repo/.github/workflows/pipeline-benchmark.yml@main"):
            with self.subTest(target=target):
                workflows = deepcopy(self.workflows)
                workflows["nightly.yml"]["jobs"]["pipeline-benchmark"]["uses"] = target
                self.assertTrue(workflow_policy_errors(workflows))

    def test_production_dispatch_safety_rules_are_pinned(self):
        # 10-20 incidents: a manual deploy without takeover left production
        # fenced (13:31Z), the rollback then refused for want of a human
        # reviewer, and a forced drill degraded production (20:54Z).
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_errors(production), [])
        guard_step = production["jobs"]["guard"]["steps"][0]

        def without(pin):
            def change(w):
                step = w["jobs"]["guard"]["steps"][0]
                self.assertIn(pin, step["run"])
                step["run"] = step["run"].replace(pin, "")
            return change

        mutations = {
            "takeover defaults false": lambda w: w["on"]["workflow_dispatch"]["inputs"]["takeover"]
                .__setitem__("default", False),
            "cutover_hold removed": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("cutover_hold"),
            "cutover_hold defaults true": lambda w: w["on"]["workflow_dispatch"]["inputs"]["cutover_hold"]
                .__setitem__("default", True),
            "cutover_hold not wired": lambda w: w["jobs"]["guard"]["steps"][0]["env"].pop("CUTOVER_HOLD"),
            "deploy takeover default dropped":
                without('takeover = "false" if cutover_hold == "true" else "true"'),
            "snapshot workflow binding dropped":
                without('PRODUCTION_WORKFLOW = ".github/workflows/deploy-production.yml"'),
            "snapshot path dropped":
                without('snapshot_run = verified_rollback_snapshot() if not reviewers and mode == "rollback" else None'),
            "snapshot pagination dropped":
                without('collection(f"repos/{repo}/actions/artifacts"'),
            "snapshot target binding dropped": without("target != (version, image, sha)"),
            "snapshot provenance dropped": without('receipt.get("provenance_complete") is not True'),
            "snapshot fork filter dropped":
                without('origin.get("head_repository_id") != origin.get("repository_id")'),
        }
        self.assertIn("inputs.cutover_hold", str(guard_step))
        for label, change in mutations.items():
            with self.subTest(label):
                workflow = deepcopy(production)
                change(workflow)
                self.assertTrue(production_errors(workflow), label)

    def test_production_route_is_automatic_or_dispatch_and_gated(self):
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_errors(production), [])
        self.assertEqual(production["jobs"]["production"]["environment"], "production")

        def mutated(change):
            workflow = deepcopy(production)
            change(workflow)
            return production_errors(workflow)

        for trigger in ("push", "pull_request", "schedule", "workflow_run", "workflow_call"):
            with self.subTest(trigger=trigger):
                self.assertTrue(mutated(lambda w, t=trigger: w["on"].update({t: ""})))
        for key, value in (("environment", None), ("environment", "staging"), ("needs", None),
                           ("needs", "other"), ("if", "${{ always() }}")):
            with self.subTest(job_key=key, value=value):
                def change(w, key=key, value=value):
                    job = w["jobs"]["production"]
                    if value is None:
                        job.pop(key, None)
                    else:
                        job[key] = value
                self.assertTrue(mutated(change))
        for step in ({"run": "npx wrangler deploy"}, {"uses": "cloudflare/wrangler-action@pinned"},
                     {"run": 'curl "$PRODUCTION_WORKER_URL/readyz"'}):
            with self.subTest(guard_step=step):
                self.assertTrue(mutated(lambda w, s=step: w["jobs"]["guard"]["steps"].append(s)))
        for label, change in {
            "workflow-level concurrency": lambda w: w.update(
                {"concurrency": {"group": "deploy-production", "cancel-in-progress": False}}),
            "production without the lane": lambda w: w["jobs"]["production"].pop("concurrency"),
            "soak holding the lane": lambda w: w["jobs"]["soak"].update(
                {"concurrency": {"group": "deploy-production", "cancel-in-progress": False}}),
            "production skipping the recheck": lambda w: w["jobs"]["production"].__setitem__(
                "needs", ["guard", "soak", "schema-gate", "roles-gate"]),
            "recheck without the soak": lambda w: w["jobs"]["recheck"].__setitem__("needs", "guard"),
            "recheck before the gates": lambda w: w["jobs"]["recheck"].__setitem__(
                "needs", ["guard", "soak"]),
            "production deploying past a failed soak": lambda w: w["jobs"]["production"].__setitem__(
                "if", w["jobs"]["production"]["if"].replace(
                    " && (needs.soak.result == 'success' || needs.soak.result == 'skipped')", "")),
            "automatic run without a fresh recheck": lambda w: w["jobs"]["production"].__setitem__(
                "if", w["jobs"]["production"]["if"].replace(
                    "(needs.guard.outputs.auto != 'true' && needs.recheck.result == 'skipped')",
                    "needs.recheck.result == 'skipped'")),
            "production deploying after cancel": lambda w: w["jobs"]["production"].__setitem__(
                "if", w["jobs"]["production"]["if"].replace("!cancelled()", "always()")),
            "recheck ignoring operator dispatches": lambda w: w["jobs"]["recheck"]["steps"][0].__setitem__(
                "run", w["jobs"]["recheck"]["steps"][0]["run"].replace(
                    "actions/workflows/deploy-production.yml/runs", "")),
            "recheck without the runs permission": lambda w: w["jobs"]["recheck"]["permissions"].pop(
                "actions"),
            "soak without the guard": lambda w: w["jobs"]["soak"].__setitem__("needs", "recheck"),
            "guard without the auto output": lambda w: w["jobs"]["guard"]["outputs"].pop("auto"),
            "guard loses cutover hold": lambda w: w["jobs"]["guard"]["steps"][0]["env"].pop(
                "CUTOVER_HOLD"),
        }.items():
            with self.subTest(lane=label):
                self.assertTrue(mutated(change))
        with self.subTest(guard="environment"):
            self.assertTrue(mutated(lambda w: w["jobs"]["guard"].update({"environment": "production"})))
        for pin in ('status="success"', "deploy-staging.yml/runs", '"required_reviewers"',
                      '"User-Agent": "two-bot-next-production-rollout/1.0"',
                      "urllib.request.Request("):
            with self.subTest(dropped=pin):
                def drop(w, pin=pin):
                    for step in w["jobs"]["guard"]["steps"]:
                        if "run" in step:
                            step["run"] = step["run"].replace(pin, "")
                self.assertTrue(mutated(drop))

    def test_production_schema_gate_shape_is_pinned_and_mutations_fail(self):
        import copy
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_schema_gate_errors(production), [])
        self.assertEqual(production_errors(production), [])
        for mutate in (
            lambda w: w["jobs"]["schema-gate"].__setitem__("environment", "production-migrate-apply"),
            lambda w: w["jobs"]["schema-gate"].__setitem__("if", "github.ref == 'refs/heads/main'"),
            lambda w: w["jobs"]["schema-gate"].__setitem__("needs", "production"),
            lambda w: w["jobs"]["production"].__setitem__("needs", "guard"),
            lambda w: w["jobs"]["production"].__setitem__("needs", ["schema-gate"]),
            lambda w: w["jobs"]["schema-gate"]["steps"][4].__setitem__("run", "wrangler deploy"),
            lambda w: w["jobs"]["schema-gate"]["steps"][4].get("env", {}).__setitem__(
                "LEAK", "${{ secrets.TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL }}"),
            lambda w: w["jobs"]["schema-gate"]["steps"][4].get("env", {}).__setitem__(
                "TWO_BOT_PRODUCTION_BRANCH_ID", ""),
        ):
            changed = copy.deepcopy(production)
            mutate(changed)
            self.assertNotEqual(production_schema_gate_errors(changed), [])

    def test_production_deploy_needs_prod_schema_gate(self):
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_schema_gate_errors(production), [])
        needs = production["jobs"]["production"]["needs"]
        needs = [needs] if isinstance(needs, str) else needs
        self.assertIn("guard", needs)
        self.assertIn("schema-gate", needs)
        self.assertEqual(production["jobs"]["schema-gate"].get("needs"), "guard")
        self.assertEqual(production["jobs"]["schema-gate"].get("environment"),
                         "production-migrate-plan")
        self.assertEqual(production["jobs"]["schema-gate"].get("if"),
                         "needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true'"
                         " && needs.guard.outputs.skip == ''")

    def test_production_roles_gate_shape_is_pinned_and_mutations_fail(self):
        import copy
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_roles_gate_errors(production), [])
        self.assertEqual(production_errors(production), [])
        for mutate in (
            lambda w: w["jobs"]["roles-gate"].__setitem__("environment", "production-migrate-apply"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("if", "github.ref == 'refs/heads/main'"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("needs", "production"),
            lambda w: w["jobs"]["production"].__setitem__("needs", ["guard", "schema-gate"]),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].__setitem__("run", "wrangler deploy"),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].get("env", {}).__setitem__(
                "LEAK", "${{ secrets.TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL }}"),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].get("env", {}).__setitem__(
                "TWO_BOT_PRODUCTION_BRANCH_ID", ""),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].__setitem__("if", "${{ false }}"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("if", "${{ false }}"),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].__setitem__("continue-on-error", True),
            lambda w: w["jobs"]["roles-gate"].__setitem__("continue-on-error", True),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].__setitem__(
                "run", w["jobs"]["roles-gate"]["steps"][4]["run"] + " || true"),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].get("env", {}).__setitem__(
                "MODE", "rollback"),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].get("env", {}).__setitem__(
                "ALLOW_UNSTAGED", "true"),
        ):
            changed = copy.deepcopy(production)
            mutate(changed)
            self.assertNotEqual(production_roles_gate_errors(changed), [])

    def test_production_deploy_needs_prod_roles_gate(self):
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_roles_gate_errors(production), [])
        needs = production["jobs"]["production"]["needs"]
        needs = [needs] if isinstance(needs, str) else needs
        self.assertIn("guard", needs)
        self.assertIn("schema-gate", needs)
        self.assertIn("roles-gate", needs)
        self.assertEqual(production["jobs"]["roles-gate"].get("needs"), "guard")
        self.assertEqual(production["jobs"]["roles-gate"].get("environment"),
                         "production-migrate-plan")
        self.assertEqual(production["jobs"]["roles-gate"].get("if"),
                         "needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true'"
                         " && needs.guard.outputs.skip == ''")

    def test_staging_migrate_runner_shape_is_pinned(self):
        # TOG-11572: the slice's own workflow must satisfy its carve-out, and
        # every widening (extra trigger, dropped input, lost environment,
        # retargeted runner, wrangler/probe step) must fail closed here.
        migrate = self.workflows["staging-migrate.yml"]
        self.assertEqual(staging_migrate_errors(migrate), [])
        self.assertEqual(workflow_policy_errors(self.workflows), [])

        def mutated(change):
            workflow = deepcopy(migrate)
            change(workflow)
            return staging_migrate_errors(workflow)

        for trigger in ("push", "pull_request", "schedule", "workflow_call"):
            with self.subTest(trigger=trigger):
                self.assertTrue(mutated(lambda w, t=trigger: w["on"].update({t: ""})))
        for missing in ("acl_plan_ref", "plan_manifest_sha256", "plan_run_id"):
            with self.subTest(missing=missing):
                def drop(w, missing=missing):
                    del w["on"]["workflow_dispatch"]["inputs"][missing]
                self.assertTrue(mutated(drop))
        with self.subTest(widened="plan-required"):
            def require(w):
                w["on"]["workflow_dispatch"]["inputs"]["plan_manifest_sha256"]["required"] = True
            self.assertTrue(mutated(require))
        with self.subTest(mode="apply-default"):
            def widen(w):
                mode = w["on"]["workflow_dispatch"]["inputs"]["mode"]
                mode["default"] = "apply"
            self.assertTrue(mutated(widen))
        with self.subTest(missing="apply-job"):
            def drop(w):
                del w["jobs"]["apply"]
            self.assertTrue(mutated(drop))
        for job_id in ("plan", "apply"):
            for key, value in (("environment", None), ("environment", "production"),
                               ("if", None), ("if", "${{ always() }}"),
                               ("runs-on", "ubuntu-latest")):
                with self.subTest(job=job_id, job_key=key, value=value):
                    def change(w, key=key, value=value, job_id=job_id):
                        job = w["jobs"][job_id]
                        if value is None:
                            job.pop(key, None)
                        else:
                            job[key] = value
                    self.assertTrue(mutated(change))
        for job_id in ("plan", "apply"):
            for step in ({"run": "npx wrangler deploy"},
                         {"run": 'curl "$STAGING_WORKER_URL/readyz"'}):
                with self.subTest(job=job_id, deploy_step=step):
                    def add(w, s=step, job_id=job_id):
                        w["jobs"][job_id]["steps"].append(s)
                    self.assertTrue(mutated(add))
        # Credential split holds for the whole job mapping, not just step env:
        # a binding exported at job level (or smuggled through any other key,
        # in any case) reaches every step of the wrong job.
        migrator_secret = "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"
        plan_secret = "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"
        for job_id, wrong_binding, wrong_secret in (
                ("plan", "TWO_BOT_STAGING_MIGRATOR_DATABASE_URL", migrator_secret),
                ("apply", "TWO_BOT_STAGING_PLAN_DATABASE_URL", plan_secret)):
            with self.subTest(job=job_id, wrong_binding="job-level-env"):
                def leak(w, job_id=job_id, wrong_binding=wrong_binding, wrong_secret=wrong_secret):
                    w["jobs"][job_id].setdefault("env", {})[wrong_binding] = wrong_secret
                self.assertTrue(mutated(leak))
            with self.subTest(job=job_id, wrong_binding="lowercase-job-level-env"):
                def leak(w, job_id=job_id, wrong_binding=wrong_binding, wrong_secret=wrong_secret):
                    w["jobs"][job_id].setdefault("env", {})["DB"] = wrong_secret.lower()
                self.assertTrue(mutated(leak))
            with self.subTest(job=job_id, wrong_binding="step-with"):
                def leak(w, job_id=job_id, wrong_secret=wrong_secret):
                    w["jobs"][job_id]["steps"].append(
                        {"uses": "actions/cache@pinned", "with": {"key": wrong_secret}})
                self.assertTrue(mutated(leak))
            with self.subTest(job=job_id, wrong_binding="step-run"):
                def leak(w, job_id=job_id, wrong_secret=wrong_secret):
                    w["jobs"][job_id]["steps"].append({"run": f"echo {wrong_secret}"})
                self.assertTrue(mutated(leak))
            for blanket in ("${{ toJSON(secrets) }}", "${{ secrets['TWO_BOT_STAGING_X'] }}"):
                with self.subTest(job=job_id, blanket=blanket):
                    def leak(w, job_id=job_id, blanket=blanket):
                        w["jobs"][job_id].setdefault("env", {})["ALL"] = blanket
                    self.assertTrue(mutated(leak))
        with self.subTest(plan="own-binding-at-job-level"):
            # Positive control: relocating the job's own binding to job `env`
            # is still that job reading only its own credential.
            def relocate(w):
                job = w["jobs"]["plan"]
                job["env"] = {"TWO_BOT_STAGING_PLAN_DATABASE_URL": plan_secret}
                for step in job["steps"]:
                    step.get("env", {}).pop("TWO_BOT_STAGING_PLAN_DATABASE_URL", None)
            self.assertEqual(mutated(relocate), [])
        with self.subTest(plan="no-own-binding"):
            def drop(w):
                job = w["jobs"]["plan"]
                for step in job["steps"]:
                    step.get("env", {}).pop("TWO_BOT_STAGING_PLAN_DATABASE_URL", None)
            self.assertTrue(mutated(drop))
        with self.subTest(apply="no-needs"):
            def drop(w):
                del w["jobs"]["apply"]["needs"]
            self.assertTrue(mutated(drop))
        with self.subTest(apply="always-runs"):
            def widen(w):
                w["jobs"]["apply"]["if"] = "github.ref == 'refs/heads/main'"
            self.assertTrue(mutated(widen))
        with self.subTest(environments="swapped"):
            def swap(w):
                w["jobs"]["plan"]["environment"] = "staging-migrate-apply"
                w["jobs"]["apply"]["environment"] = "staging-migrate-plan"
            self.assertTrue(mutated(swap))
        # The plan hash binds apply to the reviewed plan: dropping either
        # runner flag from apply, or the plan manifest upload, must fail.
        with self.subTest(apply="no-plan-hash-flag"):
            def drop_hash(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "--plan-manifest-sha256" in str(step.get("run", "")):
                        step["run"] = step["run"].replace("--plan-manifest-sha256 \"$PLAN_MANIFEST_SHA256\" ", "")
            self.assertTrue(mutated(drop_hash))
        with self.subTest(apply="no-plan-run-flag"):
            def drop_run(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "--plan-run-id" in str(step.get("run", "")):
                        step["run"] = step["run"].replace("--plan-run-id \"$PLAN_RUN_ID\" ", "")
            self.assertTrue(mutated(drop_run))
        with self.subTest(plan="hash-flags"):
            def widen(w):
                for step in w["jobs"]["plan"]["steps"]:
                    if "--expected-pending" in str(step.get("run", "")):
                        step["run"] = step["run"].replace(
                            "--expected-pending \"$EXPECTED_PENDING\"",
                            "--expected-pending \"$EXPECTED_PENDING\" --plan-manifest-sha256 \"$PLAN_MANIFEST_SHA256\"")
            self.assertTrue(mutated(widen))
        with self.subTest(plan="no-manifest-upload"):
            def drop_upload(w):
                w["jobs"]["plan"]["steps"] = [
                    step for step in w["jobs"]["plan"]["steps"]
                    if "upload-artifact" not in str(step.get("uses", ""))
                ]
            self.assertTrue(mutated(drop_upload))
        # Provenance anchor (TOG-15157 gap 2): dropping the producing-run
        # fetch, its run-id binding, its failure-closed posture or the
        # runner's manifest-path flag must fail; widening plan with either
        # end of the anchor, or widening the apply token grant, must fail.
        with self.subTest(apply="no-provenance-fetch"):
            def drop_fetch(w):
                w["jobs"]["apply"]["steps"] = [
                    step for step in w["jobs"]["apply"]["steps"]
                    if "download-artifact" not in str(step.get("uses", ""))
                ]
            self.assertTrue(mutated(drop_fetch))
        with self.subTest(apply="no-provenance-run-id"):
            def drop_run_id(w):
                for step in w["jobs"]["apply"]["steps"]:
                    with_ = step.get("with", {})
                    if "download-artifact" in str(step.get("uses", "")) and "run-id" in with_:
                        del with_["run-id"]
            self.assertTrue(mutated(drop_run_id))
        with self.subTest(apply="provenance-fetch-continues-on-error"):
            def soften(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "download-artifact" in str(step.get("uses", "")):
                        step["continue-on-error"] = True
            self.assertTrue(mutated(soften))
        with self.subTest(apply="no-provenance-path-flag"):
            def drop_path(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "--plan-manifest-path" in str(step.get("run", "")):
                        step["run"] = step["run"].replace(
                            " --plan-manifest-path producing-plan/staging-migrate-manifest.json", "")
            self.assertTrue(mutated(drop_path))
        with self.subTest(plan="provenance-fetch"):
            def widen(w):
                w["jobs"]["plan"]["steps"].append(
                    {"uses": "actions/download-artifact@pinned",
                     "with": {"name": "staging-migrate-manifest"}})
            self.assertTrue(mutated(widen))
        with self.subTest(plan="provenance-path-flag"):
            def widen(w):
                for step in w["jobs"]["plan"]["steps"]:
                    if "--expected-pending" in str(step.get("run", "")):
                        step["run"] = step["run"].replace(
                            "--expected-pending \"$EXPECTED_PENDING\"",
                            "--expected-pending \"$EXPECTED_PENDING\" "
                            "--plan-manifest-path producing-plan/staging-migrate-manifest.json")
            self.assertTrue(mutated(widen))
        # Each mutation must differ from that job's pinned grant: plan keeps
        # contents:read only, apply carries exactly contents:read plus
        # actions:read. Narrowing apply (losing the provenance fetch) or
        # widening either job must fail.
        for job_id, permissions in (
                ("plan", {"contents": "read", "actions": "read"}),
                ("plan", {}),
                ("apply", {"contents": "read"}),
                ("apply", {"contents": "read", "actions": "read", "checks": "read"}),
                ("apply", {"contents": "write", "actions": "read"})):
            with self.subTest(job=job_id, permissions=permissions):
                def change(w, job_id=job_id, permissions=permissions):
                    w["jobs"][job_id]["permissions"] = permissions
                self.assertTrue(mutated(change))

    def test_production_migrate_runner_shape_is_pinned(self):
        # Production mirror of the staging runner: same three jobs, same pinned
        # actions, routed runner, pipefail and provenance gates, with
        # production names, environments, secrets and --target production.
        migrate = self.workflows["production-migrate.yml"]
        self.assertEqual(production_migrate_errors(migrate), [])
        self.assertEqual(staging_migrate_errors(self.workflows["staging-migrate.yml"]), [])
        self.assertEqual(workflow_policy_errors(self.workflows), [])

        def mutated(change):
            workflow = deepcopy(migrate)
            change(workflow)
            return production_migrate_errors(workflow)

        for trigger in ("push", "pull_request", "schedule", "workflow_call"):
            with self.subTest(trigger=trigger):
                self.assertTrue(mutated(lambda w, t=trigger: w["on"].update({t: ""})))
        for missing in ("acl_plan_ref", "plan_manifest_sha256", "plan_run_id",
                        "production_host", "production_database",
                        "production_branch_id"):
            with self.subTest(missing=missing):
                def drop(w, missing=missing):
                    del w["on"]["workflow_dispatch"]["inputs"][missing]
                self.assertTrue(mutated(drop))
        # Staging inputs must not appear on the production workflow.
        for staging_input in ("staging_host", "staging_database", "staging_branch_id"):
            with self.subTest(staging_input=staging_input):
                def add(w, staging_input=staging_input):
                    w["on"]["workflow_dispatch"]["inputs"][staging_input] = {"required": True}
                self.assertTrue(mutated(add))
        with self.subTest(mode="apply-default"):
            def widen(w):
                w["on"]["workflow_dispatch"]["inputs"]["mode"]["default"] = "apply"
            self.assertTrue(mutated(widen))
        with self.subTest(missing="apply-job"):
            def drop(w):
                del w["jobs"]["apply"]
            self.assertTrue(mutated(drop))
        for job_id in ("plan", "apply"):
            for key, value in (("environment", None),
                               ("environment", "staging-migrate-plan"),
                               ("environment", "staging-migrate-apply"),
                               ("if", None), ("if", "${{ always() }}"),
                               ("runs-on", "ubuntu-latest")):
                with self.subTest(job=job_id, job_key=key, value=value):
                    def change(w, key=key, value=value, job_id=job_id):
                        job = w["jobs"][job_id]
                        if value is None:
                            job.pop(key, None)
                        else:
                            job[key] = value
                    self.assertTrue(mutated(change))
        # Target isolation: the runner flag and host/database flags must be
        # production, never staging; dropping them or swapping them fails.
        with self.subTest(target="missing"):
            def drop_target(w):
                for job in w["jobs"].values():
                    for step in job.get("steps", []):
                        if "--target production" in str(step.get("run", "")):
                            step["run"] = step["run"].replace("--target production", "")
            self.assertTrue(mutated(drop_target))
        for wrong in ("--target staging", "--staging-host", "--staging-database",
                      "--staging-branch-id"):
            with self.subTest(wrong=wrong):
                def widen(w, wrong=wrong):
                    for step in w["jobs"]["plan"]["steps"]:
                        if "tee" in str(step.get("run", "")):
                            step["run"] = step["run"].replace("--production-host", wrong) \
                                if "host" in wrong else step["run"].replace("--target production", wrong)
                self.assertTrue(mutated(widen))
        # Credential split holds for the whole job mapping, including
        # cross-target leaks: a production job never reads a staging binding.
        prod_migrator = "${{ secrets.TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL }}"
        prod_plan = "${{ secrets.TWO_BOT_PRODUCTION_PLAN_DATABASE_URL }}"
        staging_migrator = "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"
        staging_plan = "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"
        for job_id, wrong_secret in (("plan", prod_migrator), ("apply", prod_plan),
                                     ("plan", staging_migrator), ("plan", staging_plan),
                                     ("apply", staging_migrator), ("apply", staging_plan)):
            with self.subTest(job=job_id, wrong_binding="leak"):
                def leak(w, job_id=job_id, wrong_secret=wrong_secret):
                    w["jobs"][job_id].setdefault("env", {})["LEAK"] = wrong_secret
                self.assertTrue(mutated(leak))
        with self.subTest(plan="no-own-binding"):
            def drop(w):
                job = w["jobs"]["plan"]
                for step in job["steps"]:
                    step.get("env", {}).pop("TWO_BOT_PRODUCTION_PLAN_DATABASE_URL", None)
            self.assertTrue(mutated(drop))
        with self.subTest(apply="no-needs"):
            def drop(w):
                del w["jobs"]["apply"]["needs"]
            self.assertTrue(mutated(drop))
        with self.subTest(environments="swapped"):
            def swap(w):
                w["jobs"]["plan"]["environment"] = "production-migrate-apply"
                w["jobs"]["apply"]["environment"] = "production-migrate-plan"
            self.assertTrue(mutated(swap))
        with self.subTest(apply="no-plan-hash-flag"):
            def drop_hash(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "--plan-manifest-sha256" in str(step.get("run", "")):
                        step["run"] = step["run"].replace("--plan-manifest-sha256 \"$PLAN_MANIFEST_SHA256\" ", "")
            self.assertTrue(mutated(drop_hash))
        with self.subTest(apply="no-provenance-fetch"):
            def drop_fetch(w):
                w["jobs"]["apply"]["steps"] = [
                    step for step in w["jobs"]["apply"]["steps"]
                    if "download-artifact" not in str(step.get("uses", ""))
                ]
            self.assertTrue(mutated(drop_fetch))
        with self.subTest(apply="no-provenance-path-flag"):
            def drop_path(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "--plan-manifest-path" in str(step.get("run", "")):
                        step["run"] = step["run"].replace(
                            " --plan-manifest-path producing-plan/production-migrate-manifest.json", "")
            self.assertTrue(mutated(drop_path))
        # Production and staging stay mirrors: same job ids, same step counts,
        # same pinned actions and container, differing only by the documented
        # production prefix (names, envs, secrets, artifacts, flags).
        staging = self.workflows["staging-migrate.yml"]
        self.assertEqual(set(migrate["jobs"]), set(staging["jobs"]))
        for job_id in ("plan", "claim", "apply"):
            with self.subTest(mirror=job_id):
                prod_job, stag_job = migrate["jobs"][job_id], staging["jobs"][job_id]
                self.assertEqual(len(prod_job.get("steps", [])), len(stag_job.get("steps", [])))
                self.assertEqual(prod_job.get("permissions"), stag_job.get("permissions"))
                self.assertEqual(prod_job.get("if"), stag_job.get("if"))
                self.assertEqual(prod_job.get("runs-on"), stag_job.get("runs-on").replace("staging", "production") if "staging" in str(stag_job.get("runs-on")) else stag_job.get("runs-on"))

    def test_staging_rollback_drill_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_rollback_drill_errors(workflows["staging-rollback-drill.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-rollback-drill.yml"])
            change(copy)
            return staging_rollback_drill_errors(copy)

        def job(copy):
            return copy["jobs"]["drill"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "extra input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update({"force": {"type": "boolean"}}),
            "target default": lambda w: w["on"]["workflow_dispatch"]["inputs"]["target_version"].update(
                {"default": "latest"}),
            "required target blocks daily proof": lambda w: w["on"]["workflow_dispatch"]["inputs"]["target_version"].update(
                {"required": "true"}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "own concurrency group": lambda w: w["concurrency"].update({"group": "staging-rollback-drill"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(w["jobs"]["drill"])}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "any branch": lambda w: job(w).update({"if": "github.ref != ''"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "job-level secret": lambda w: job(w).update({"env": {"T": "${{ secrets.CLOUDFLARE_API_TOKEN }}"}}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                2, {"uses": "cloudflare/wrangler-action@pinned"}),
            "step condition": lambda w: job(w)["steps"][2].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][2].update({"continue-on-error": "true"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update({"persist-credentials": "true"}),
            "extra binding": lambda w: job(w)["steps"][2]["env"].update(
                {"X": "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"}),
            "inline expression": lambda w: job(w)["steps"][2].update(
                {"run": 'python3 scripts/staging_rollback_drill.py --target-version "${{ inputs.target_version }}"'}),
            "forced rollback": lambda w: job(w)["steps"][2].update(
                {"run": 'python3 scripts/staging_rollback_drill.py --target-version "$TARGET_VERSION" --force'}),
            "other script": lambda w: job(w)["steps"][2].update(
                {"run": 'python3 scripts/other.py --target-version "$TARGET_VERSION"'}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-rollback-drill.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_daily_rollback_proof_and_independent_finding_only_watch_are_pinned(self):
        workflow = self.workflows["staging-rollback-drill.yml"]
        self.assertEqual(staging_rollback_drill_errors(workflow), [])
        for label, change in {
            "missing daily schedule": lambda w: w["on"].pop("schedule"),
            "manual replaces pending schedule": lambda w: w["concurrency"].update({"group": "staging-rollback-proof"}),
            "weekly only": lambda w: w["on"].update({"schedule": [{"cron": "17 3 * * 1"}]}),
            "dispatcher deadlocks staging lane": lambda w: w["jobs"]["proof"].update(
                {"concurrency": {"group": "deploy-staging", "cancel-in-progress": False}}),
            "dispatcher reads production secrets": lambda w: w["jobs"]["proof"].update({"environment": "production"}),
            "dispatcher extra bindings": lambda w: w["jobs"]["proof"]["steps"][1]["env"].update({"URL": "production"}),
            "missing evidence": lambda w: w["jobs"]["proof"]["steps"].pop(),
            "alert on success": lambda w: w["jobs"]["alert"].update({"if": "always()"}),
            "alert cannot write the incident issue": lambda w: w["jobs"]["alert"]["permissions"].pop("issues"),
            "alert reads an environment": lambda w: w["jobs"]["alert"].update({"environment": "synthetic-alerts"}),
            "manual drill loses lane": lambda w: w["jobs"]["drill"].pop("concurrency"),
        }.items():
            changed = deepcopy(workflow)
            change(changed)
            with self.subTest(label=label):
                self.assertTrue(staging_rollback_drill_errors(changed))
        watcher = self.workflows["daily-digest.yml"]["jobs"]["rollback-proof-watch"]
        self.assertNotIn("needs", watcher)
        self.assertEqual(rollback_proof_job_errors(watcher, "watch"), [])
        for change in (lambda w: w["permissions"].update({"actions": "write"}),
                       lambda w: w["steps"][1].update({"run": "python3 scripts/rollback_proof.py run"}),
                       lambda w: w["steps"][1].update({"continue-on-error": True})):
            changed = deepcopy(watcher)
            change(changed)
            self.assertTrue(rollback_proof_job_errors(changed, "watch"))
        spec = importlib.util.spec_from_file_location("rollback_proof", ROOT / "scripts/rollback_proof.py")
        proof = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(proof)
        deploy = self.workflows["deploy-staging.yml"]["jobs"]["deploy"]
        self.assertEqual(deploy["name"], "wrangler deploy + intended rollout gate")
        expected_steps = {
            "Snapshot the pre-deploy rollback state": "success",
            "Deploy to staging": "success",
            "Verify exact new rollout and serving Worker/container readiness": "success",
            "Gate promotion on REST 5xx error rate": "failure",
            "Auto-roll back Worker version and container image": "success",
            "Upload the rollout evidence": "success",
        }
        self.assertEqual(proof.REQUIRED_STEPS, expected_steps)
        self.assertTrue(set(expected_steps) <= {s.get("name") for s in deploy["steps"]})
        worker_steps = self.workflows["check.yml"]["jobs"]["worker"]["steps"]
        covering = [s for s in worker_steps if "test_rollback_proof.py" in str(s.get("run") or "")]
        self.assertEqual(len(covering), 1)
        self.assertNotIn("if", covering[0])

    def test_staging_container_drill_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_container_drill_errors(workflows["staging-container-drill.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-container-drill.yml"])
            change(copy)
            return staging_container_drill_errors(copy)

        def job(copy):
            return copy["jobs"]["container-drill"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "extra input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update({"force": {"type": "boolean"}}),
            "pin default": lambda w: w["on"]["workflow_dispatch"]["inputs"]["backout_image"].update(
                {"default": "latest"}),
            "optional pin": lambda w: w["on"]["workflow_dispatch"]["inputs"]["backout_image"].update(
                {"required": "false"}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "own concurrency group": lambda w: w["concurrency"].update({"group": "staging-container-drill"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(w["jobs"]["container-drill"])}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "any branch": lambda w: job(w).update({"if": "github.ref != ''"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "job-level secret": lambda w: job(w).update({"env": {"T": "${{ secrets.CLOUDFLARE_API_TOKEN }}"}}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                2, {"uses": "cloudflare/wrangler-action@pinned"}),
            "step condition": lambda w: job(w)["steps"][3].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][3].update({"continue-on-error": "true"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update({"persist-credentials": "true"}),
            "extra binding": lambda w: job(w)["steps"][3]["env"].update(
                {"X": "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"}),
            "inline expression": lambda w: job(w)["steps"][3].update(
                {"run": 'python3 scripts/staging_container_drill.py --backout-image "${{ inputs.backout_image }}"'}),
            "other script": lambda w: job(w)["steps"][3].update(
                {"run": 'python3 scripts/other.py --backout-image "$BACKOUT_IMAGE"'}),
            "dropped npm ci": lambda w: job(w)["steps"].__delitem__(2),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-container-drill.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_staging_events_read_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_events_read_errors(workflows["staging-events-read.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-events-read.yml"])
            change(copy)
            return staging_events_read_errors(copy)

        def job(copy):
            return copy["jobs"]["read"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "schedule trigger": lambda w: w["on"].update({"schedule": [{"cron": "0 * * * *"}]}),
            "member input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update(
                {"fixture_member": {"type": "string", "required": "true"}}),
            "guild input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update(
                {"guild": {"type": "string", "required": "true"}}),
            "dropped end input": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("window_end"),
            "window default": lambda w: w["on"]["workflow_dispatch"]["inputs"]["window_start"].update(
                {"default": "2026-10-07T00:00:00Z"}),
            "optional window": lambda w: w["on"]["workflow_dispatch"]["inputs"]["window_end"].update(
                {"required": "false"}),
            "top-level permissions": lambda w: w.update({"permissions": {"contents": "read"}}),
            "workflow-level secret": lambda w: w.update(
                {"env": {"T": "${{ secrets.TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL }}"}}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "shared concurrency group": lambda w: w["concurrency"].update({"group": "deploy-staging"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(job(w))}),
            "wrong environment": lambda w: job(w).update({"environment": "staging"}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "any branch": lambda w: job(w).update({"if": "github.ref != ''"}),
            "write grant": lambda w: job(w).update({"permissions": {"contents": "write"}}),
            "longer timeout": lambda w: job(w).update({"timeout-minutes": "30"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "other job's routing": lambda w: job(w).update(
                {"runs-on": ROUTED_RUNNER.format(job="check")}),
            "job-level secret": lambda w: job(w).update(
                {"env": {"T": "${{ secrets.TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL }}"}}),
            "container": lambda w: job(w).update({"container": {"image": "postgres"}}),
            "step condition": lambda w: job(w)["steps"][1].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][1].update({"continue-on-error": "true"}),
            "extra step": lambda w: job(w)["steps"].append({"run": "echo done"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update(
                {"persist-credentials": "true"}),
            "extra secret": lambda w: job(w)["steps"][1]["env"].update(
                {"X": "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"}),
            "dropped login secret": lambda w: job(w)["steps"][1]["env"].pop(
                "TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL"),
            "dropped host pin": lambda w: job(w)["steps"][1]["env"].pop(
                "STAGING_EVENTS_READ_EXPECTED_HOST"),
            "blanket secrets": lambda w: job(w)["steps"][1]["env"].update({"ALL": "${{ toJSON(secrets) }}"}),
            "member as variable": lambda w: job(w)["steps"][1]["env"].update(
                {"STAGING_FIXTURE_MEMBER_ID": "${{ vars.STAGING_FIXTURE_MEMBER_ID }}"}),
            "member as input": lambda w: job(w)["steps"][1]["env"].update(
                {"STAGING_FIXTURE_MEMBER_ID": "${{ inputs.member }}"}),
            "inline expression": lambda w: job(w)["steps"][1].update(
                {"run": 'python3 scripts/staging_events_read.py --window-start "${{ inputs.window_start }}" '
                        '--window-end "$WINDOW_END" --output "staging-events-read-$RUN_ID.json"'}),
            "shell trace": lambda w: job(w)["steps"][1].update({"run": "set -x\n" + EVENTS_READ_COMMAND}),
            "other script": lambda w: job(w)["steps"][1].update(
                {"run": EVENTS_READ_COMMAND.replace("staging_events_read", "other")}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                1, {"uses": "cloudflare/wrangler-action@pinned"}),
            "upload other file": lambda w: job(w)["steps"][2]["with"].update({"path": "*.json"}),
            "upload without retention": lambda w: job(w)["steps"][2]["with"].pop("retention-days"),
            "upload longer retention": lambda w: job(w)["steps"][2]["with"].update({"retention-days": "90"}),
            "upload may be empty": lambda w: job(w)["steps"][2]["with"].update({"if-no-files-found": "warn"}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-events-read.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_staging_privacy_verify_checkout_cannot_retarget_the_reviewed_head(self):
        self.assertEqual(workflow_policy_errors(self.workflows), [])
        for key, value in (("ref", "unreviewed-branch"),
                           ("repository", "example/unreviewed"),
                           ("path", "other-checkout"),
                           ("sparse-checkout", "unreviewed"),
                           ("fetch-depth", "0")):
            with self.subTest(override=key):
                workflows = deepcopy(self.workflows)
                checkout = workflows["staging-privacy-verify-read.yml"]["jobs"]["read"]["steps"][0]
                checkout["with"][key] = value
                self.assertTrue(workflow_policy_errors(workflows),
                                "the secret-bearing read must use the reviewed workflow head")

    def test_staging_privacy_verify_suite_is_required_in_worker(self):
        jobs = self.workflows["check.yml"]["jobs"]
        command = "PYTHONDONTWRITEBYTECODE=1 python3 ../scripts/test_staging_privacy_verify_read.py -v"
        steps = [step for step in jobs["worker"]["steps"] if step.get("run") == command]
        self.assertEqual(len(steps), 1, "worker must run the collector suite exactly once")
        self.assertNotIn("if", steps[0])
        self.assertNotIn("continue-on-error", steps[0])
        self.assertIn("worker", jobs["ci-ok"]["needs"])
        aggregate = next(step for step in jobs["ci-ok"]["steps"]
                         if step.get("name") == "Aggregate CI verdict")
        self.assertEqual(aggregate["env"]["WORKER_RESULT"], "${{ needs.worker.result }}")
        self.assertIn('(\"worker\", \"WORKER_RESULT\", \"WORKER_SELECTED\")', aggregate["run"])

    def test_staging_privacy_verify_read_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_privacy_verify_read_errors(workflows["staging-privacy-verify-read.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-privacy-verify-read.yml"])
            change(copy)
            return staging_privacy_verify_read_errors(copy)

        def job(copy):
            return copy["jobs"]["read"]

        def guild(copy):
            return copy["on"]["workflow_dispatch"]["inputs"]["guild"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "schedule trigger": lambda w: w["on"].update({"schedule": [{"cron": "0 * * * *"}]}),
            "member input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update(
                {"fixture_member": {"type": "string", "required": "true"}}),
            "window input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update(
                {"window_start": {"type": "string", "required": "true"}}),
            "dropped guild input": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("guild"),
            "guild string type": lambda w: guild(w).update({"type": "string"}),
            "guild raw id option": lambda w: guild(w).update({"options": ["g", "h", "1545644954272137297"]}),
            "guild dropped h": lambda w: guild(w).update({"options": ["g"]}),
            "guild default": lambda w: guild(w).update({"default": "g"}),
            "optional guild": lambda w: guild(w).update({"required": "false"}),
            "top-level permissions": lambda w: w.update({"permissions": {"contents": "read"}}),
            "workflow-level secret": lambda w: w.update(
                {"env": {"T": "${{ secrets.TWO_BOT_STAGING_PRIVACY_RO_DATABASE_URL }}"}}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "shared concurrency group": lambda w: w["concurrency"].update({"group": "deploy-staging"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(job(w))}),
            "wrong environment": lambda w: job(w).update({"environment": "staging"}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "any branch": lambda w: job(w).update({"if": "github.ref != ''"}),
            "write grant": lambda w: job(w).update({"permissions": {"contents": "write"}}),
            "longer timeout": lambda w: job(w).update({"timeout-minutes": "30"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "other job's routing": lambda w: job(w).update(
                {"runs-on": ROUTED_RUNNER.format(job="check")}),
            "job-level secret": lambda w: job(w).update(
                {"env": {"T": "${{ secrets.TWO_BOT_STAGING_PRIVACY_RO_DATABASE_URL }}"}}),
            "container": lambda w: job(w).update({"container": {"image": "postgres"}}),
            "step condition": lambda w: job(w)["steps"][1].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][1].update({"continue-on-error": "true"}),
            "extra step": lambda w: job(w)["steps"].append({"run": "echo done"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update(
                {"persist-credentials": "true"}),
            "extra secret": lambda w: job(w)["steps"][1]["env"].update(
                {"X": "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"}),
            "dropped login secret": lambda w: job(w)["steps"][1]["env"].pop(
                "TWO_BOT_STAGING_PRIVACY_RO_DATABASE_URL"),
            "dropped host pin": lambda w: job(w)["steps"][1]["env"].pop(
                "STAGING_PRIVACY_VERIFY_EXPECTED_HOST"),
            "dropped h pin": lambda w: job(w)["steps"][1]["env"].pop(
                "STAGING_PRIVACY_VERIFY_H_GUILD_ID"),
            "blanket secrets": lambda w: job(w)["steps"][1]["env"].update({"ALL": "${{ toJSON(secrets) }}"}),
            "member as variable": lambda w: job(w)["steps"][1]["env"].update(
                {"STAGING_FIXTURE_MEMBER_ID": "${{ vars.STAGING_FIXTURE_MEMBER_ID }}"}),
            "member as input": lambda w: job(w)["steps"][1]["env"].update(
                {"STAGING_FIXTURE_MEMBER_ID": "${{ inputs.member }}"}),
            "h pin as variable": lambda w: job(w)["steps"][1]["env"].update(
                {"STAGING_PRIVACY_VERIFY_H_GUILD_ID": "${{ vars.STAGING_PRIVACY_VERIFY_H_GUILD_ID }}"}),
            "inline expression": lambda w: job(w)["steps"][1].update(
                {"run": 'python3 scripts/staging_privacy_verify_read.py --guild "${{ inputs.guild }}" '
                        '--output "staging-privacy-verify-read-$RUN_ID.json"'}),
            "shell trace": lambda w: job(w)["steps"][1].update({"run": "set -x\n" + PRIVACY_VERIFY_READ_COMMAND}),
            "other script": lambda w: job(w)["steps"][1].update(
                {"run": PRIVACY_VERIFY_READ_COMMAND.replace("staging_privacy_verify_read", "other")}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                1, {"uses": "cloudflare/wrangler-action@pinned"}),
            "upload other file": lambda w: job(w)["steps"][2]["with"].update({"path": "*.json"}),
            "upload without retention": lambda w: job(w)["steps"][2]["with"].pop("retention-days"),
            "upload longer retention": lambda w: job(w)["steps"][2]["with"].update({"retention-days": "90"}),
            "upload may be empty": lambda w: job(w)["steps"][2]["with"].update({"if-no-files-found": "warn"}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-privacy-verify-read.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_staging_retention_census_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_retention_census_errors(workflows["staging-retention-census.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-retention-census.yml"])
            change(copy)
            return staging_retention_census_errors(copy)

        def job(copy):
            return copy["jobs"]["census"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "schedule trigger": lambda w: w["on"].update({"schedule": [{"cron": "0 * * * *"}]}),
            "dispatch input": lambda w: w["on"].update(
                {"workflow_dispatch": {"inputs": {"mode": {"type": "string"}}}}),
            "top-level permissions": lambda w: w.update({"permissions": {"contents": "read"}}),
            "workflow-level secret": lambda w: w.update(
                {"env": {"T": "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"}}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "shared concurrency group": lambda w: w["concurrency"].update({"group": "deploy-staging"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(job(w))}),
            "wrong environment": lambda w: job(w).update({"environment": "staging"}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "any branch": lambda w: job(w).update({"if": "github.ref != ''"}),
            "write grant": lambda w: job(w).update({"permissions": {"contents": "write"}}),
            "shorter timeout": lambda w: job(w).update({"timeout-minutes": "5"}),
            "longer timeout": lambda w: job(w).update({"timeout-minutes": "60"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "other job's routing": lambda w: job(w).update(
                {"runs-on": ROUTED_RUNNER.format(job="check")}),
            "job-level secret": lambda w: job(w).update(
                {"env": {"T": "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"}}),
            "container": lambda w: job(w).update({"container": {"image": "postgres"}}),
            "step condition": lambda w: job(w)["steps"][1].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][1].update({"continue-on-error": "true"}),
            "artifact upload": lambda w: job(w)["steps"].append(
                {"uses": "actions/upload-artifact@pinned"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update(
                {"persist-credentials": "true"}),
            "extra secret": lambda w: job(w)["steps"][1]["env"].update(
                {"X": "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"}),
            "dropped login secret": lambda w: job(w)["steps"][1]["env"].pop(
                "TWO_BOT_STAGING_PLAN_DATABASE_URL"),
            "dropped host pin": lambda w: job(w)["steps"][1]["env"].pop(
                "TWO_BOT_STAGING_HOST"),
            "blanket secrets": lambda w: job(w)["steps"][1]["env"].update({"ALL": "${{ toJSON(secrets) }}"}),
            "pin as input": lambda w: job(w)["steps"][1]["env"].update(
                {"TWO_BOT_STAGING_HOST": "${{ inputs.host }}"}),
            "inline expression": lambda w: job(w)["steps"][1].update(
                {"run": "set -o pipefail\npython3 scripts/staging_retention_census.py ${{ vars.X }}\n"}),
            "missing pipefail": lambda w: job(w)["steps"][1].update(
                {"run": "python3 scripts/staging_retention_census.py\n"}),
            "missing shell": lambda w: job(w)["steps"][1].pop("shell"),
            "shell trace": lambda w: job(w)["steps"][1].update({"run": "set -x\n" + CENSUS_COMMAND}),
            "other script": lambda w: job(w)["steps"][1].update(
                {"run": CENSUS_COMMAND.replace("staging_retention_census", "other")}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                1, {"uses": "cloudflare/wrangler-action@pinned"}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-retention-census.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_staging_fixture_census_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_fixture_census_errors(workflows["staging-fixture-census.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-fixture-census.yml"])
            change(copy)
            return staging_fixture_census_errors(copy)

        def job(copy):
            return copy["jobs"]["fixture-census"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "schedule trigger": lambda w: w["on"].update({"schedule": [{"cron": "0 * * * *"}]}),
            "dispatch input": lambda w: w["on"].update(
                {"workflow_dispatch": {"inputs": {"mode": {"type": "string"}}}}),
            "top-level permissions": lambda w: w.update({"permissions": {"contents": "read"}}),
            "workflow-level secret": lambda w: w.update(
                {"env": {"T": "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"}}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "shared concurrency group": lambda w: w["concurrency"].update({"group": "deploy-staging"}),
            "sibling concurrency group": lambda w: w["concurrency"].update({"group": "staging-retention-census"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(job(w))}),
            "wrong environment": lambda w: job(w).update({"environment": "staging"}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "any branch": lambda w: job(w).update({"if": "github.ref != ''"}),
            "write grant": lambda w: job(w).update({"permissions": {"contents": "write"}}),
            "shorter timeout": lambda w: job(w).update({"timeout-minutes": "5"}),
            "longer timeout": lambda w: job(w).update({"timeout-minutes": "60"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "other job's routing": lambda w: job(w).update(
                {"runs-on": ROUTED_RUNNER.format(job="check")}),
            "sibling routing": lambda w: job(w).update(
                {"runs-on": ROUTED_RUNNER.format(job="census")}),
            "job-level secret": lambda w: job(w).update(
                {"env": {"T": "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"}}),
            "container": lambda w: job(w).update({"container": {"image": "postgres"}}),
            "step condition": lambda w: job(w)["steps"][1].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][1].update({"continue-on-error": "true"}),
            "artifact upload": lambda w: job(w)["steps"].append(
                {"uses": "actions/upload-artifact@pinned"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update(
                {"persist-credentials": "true"}),
            "extra secret": lambda w: job(w)["steps"][1]["env"].update(
                {"X": "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"}),
            "dropped login secret": lambda w: job(w)["steps"][1]["env"].pop(
                "TWO_BOT_STAGING_PLAN_DATABASE_URL"),
            "dropped host pin": lambda w: job(w)["steps"][1]["env"].pop(
                "TWO_BOT_STAGING_HOST"),
            "blanket secrets": lambda w: job(w)["steps"][1]["env"].update({"ALL": "${{ toJSON(secrets) }}"}),
            "pin as input": lambda w: job(w)["steps"][1]["env"].update(
                {"TWO_BOT_STAGING_HOST": "${{ inputs.host }}"}),
            "inline expression": lambda w: job(w)["steps"][1].update(
                {"run": FIXTURE_CENSUS_COMMAND + "echo ${{ vars.X }}\n"}),
            "missing pipefail": lambda w: job(w)["steps"][1].update(
                {"run": "python3 scripts/staging_retention_census.py fixture-scope\n"}),
            "missing mode": lambda w: job(w)["steps"][1].update(
                {"run": CENSUS_COMMAND}),
            "missing shell": lambda w: job(w)["steps"][1].pop("shell"),
            "shell trace": lambda w: job(w)["steps"][1].update({"run": "set -x\n" + FIXTURE_CENSUS_COMMAND}),
            "other script": lambda w: job(w)["steps"][1].update(
                {"run": FIXTURE_CENSUS_COMMAND.replace("staging_retention_census", "other")}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                1, {"uses": "cloudflare/wrangler-action@pinned"}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-fixture-census.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_staging_ownership_status_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_ownership_status_errors(workflows["staging-ownership-status.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-ownership-status.yml"])
            change(copy)
            return staging_ownership_status_errors(copy)

        def job(copy):
            return copy["jobs"]["status"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "schedule trigger": lambda w: w["on"].update({"schedule": [{"cron": "0 * * * *"}]}),
            "with inputs": lambda w: w["on"].update({"workflow_dispatch": {"inputs": {"force": {"type": "boolean"}}}}),
            "top-level permissions": lambda w: w.update({"permissions": {"contents": "read"}}),
            "workflow-level secret": lambda w: w.update(
                {"env": {"T": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}"}}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "shared concurrency group": lambda w: w["concurrency"].update({"group": "deploy-staging"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(job(w))}),
            "wrong environment": lambda w: job(w).update({"environment": "staging-events-read"}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "any branch": lambda w: job(w).update({"if": "github.ref != ''"}),
            "write grant": lambda w: job(w).update({"permissions": {"contents": "write"}}),
            "longer timeout": lambda w: job(w).update({"timeout-minutes": "30"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "other job's routing": lambda w: job(w).update(
                {"runs-on": ROUTED_RUNNER.format(job="check")}),
            "job-level secret": lambda w: job(w).update(
                {"env": {"T": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}"}}),
            "step condition": lambda w: job(w)["steps"][3].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][3].update({"continue-on-error": "true"}),
            "extra step": lambda w: job(w)["steps"].append({"run": "echo done"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update(
                {"persist-credentials": "true"}),
            "extra secret": lambda w: job(w)["steps"][3]["env"].update(
                {"X": "${{ secrets.CLOUDFLARE_API_TOKEN }}"}),
            "dropped token": lambda w: job(w)["steps"][3]["env"].pop("OWNERSHIP_CONTROL_TOKEN"),
            "dropped worker url": lambda w: job(w)["steps"][3]["env"].pop("STAGING_WORKER_URL"),
            "blanket secrets": lambda w: job(w)["steps"][3]["env"].update({"ALL": "${{ toJSON(secrets) }}"}),
            "inline expression": lambda w: job(w)["steps"][3].update(
                {"run": 'node scripts/staging-ownership-status.mjs "${{ vars.STAGING_WORKER_URL }}"'}),
            "shell trace": lambda w: job(w)["steps"][3].update(
                {"run": "set -x\nnode scripts/staging-ownership-status.mjs"}),
            "other script": lambda w: job(w)["steps"][3].update(
                {"run": "node scripts/other.mjs"}),
            "takeover step": lambda w: job(w)["steps"][3].update(
                {"run": "node scripts/ownership-control.mjs deployment-takeover"}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                2, {"uses": "cloudflare/wrangler-action@pinned"}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-ownership-status.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_slow_burn_tracker_reads_staging_bearer_and_isolates_fixtures(self):
        workflows = self.workflows
        tracker = workflows["slow-burn-tracker.yml"]
        live = tracker["jobs"]["tracker"]
        # Live runs read the existing `staging` Environment bearer; the
        # deployment: false form keeps the five-minute scrape out of the
        # staging deployment history.
        self.assertEqual(live.get("environment"), {"name": "staging", "deployment": "false"})
        self.assertEqual(live["steps"][3]["env"]["METRICS_SCRAPE_TOKEN"],
                         "${{ secrets.STAGING_METRICS_SCRAPE_TOKEN }}")
        # Fixture proofs run in their own job: no Environment, only the job token.
        self.assertIn("fixture", tracker["jobs"])
        fixture = tracker["jobs"]["fixture"]
        self.assertNotIn("environment", fixture)
        self.assertEqual(set(re.findall(r"secrets\.(\w+)", str(fixture))), {"GITHUB_TOKEN"})
        self.assertEqual(slow_burn_tracker_errors(tracker), [])

        def mutate(change):
            copy = deepcopy(tracker)
            change(copy)
            return slow_burn_tracker_errors(copy)

        def live_job(copy):
            return copy["jobs"]["tracker"]

        def fixture_job(copy):
            return copy["jobs"]["fixture"]

        def live_run(copy, text):
            live_job(copy)["steps"][3]["run"] = text(live_job(copy)["steps"][3]["run"])

        def fixture_run(copy, text):
            fixture_job(copy)["steps"][2]["run"] = text(fixture_job(copy)["steps"][2]["run"])

        bearer = "${{ secrets.STAGING_METRICS_SCRAPE_TOKEN }}"
        staging = {"name": "staging", "deployment": "false"}
        changes = {
            "pull_request trigger": lambda w: w["on"].update({"pull_request": None}),
            "pull_request_target trigger": lambda w: w["on"].update({"pull_request_target": None}),
            "workflow_run trigger": lambda w: w["on"].update({"workflow_run": {"workflows": ["check"]}}),
            "workflow-level secret": lambda w: w.update({"env": {"T": bearer}}),
            "workflow-level shell default": lambda w: w.update(
                {"defaults": {"run": {"shell": "echo {0}"}}}),
            "live job shell default": lambda w: live_job(w).update(
                {"defaults": {"run": {"shell": "echo {0}"}}}),
            "fixture job shell default": lambda w: fixture_job(w).update(
                {"defaults": {"run": {"shell": "echo {0}"}}}),
            "live run step shell override": lambda w: live_job(w)["steps"][3].update(
                {"shell": "echo {0}"}),
            "fixture run step shell override": lambda w: fixture_job(w)["steps"][2].update(
                {"shell": "echo {0}"}),
            "third job": lambda w: w["jobs"].update({"again": deepcopy(live_job(w))}),
            "fixture job dropped": lambda w: w["jobs"].pop("fixture"),
            # Live job: exactly the existing staging bearer, from main only.
            "old tracker environment": lambda w: live_job(w).update({"environment": "slow-burn-tracker"}),
            "deployment-creating staging": lambda w: live_job(w).update({"environment": "staging"}),
            "deployment true": lambda w: live_job(w).update(
                {"environment": {"name": "staging", "deployment": "true"}}),
            "production environment": lambda w: live_job(w).update(
                {"environment": {"name": "production", "deployment": "false"}}),
            "no live environment": lambda w: live_job(w).pop("environment"),
            "unbound secret name": lambda w: live_job(w)["steps"][3]["env"].update(
                {"METRICS_SCRAPE_TOKEN": "${{ secrets.METRICS_SCRAPE_TOKEN }}"}),
            "production bearer": lambda w: live_job(w)["steps"][3]["env"].update(
                {"METRICS_SCRAPE_TOKEN": "${{ secrets.PRODUCTION_METRICS_SCRAPE_TOKEN }}"}),
            "dropped bearer": lambda w: live_job(w)["steps"][3]["env"].pop("METRICS_SCRAPE_TOKEN"),
            # The script reads METRICS_SCRAPE_TOKEN: the bearer must keep that name.
            "bearer under another name": lambda w: live_job(w)["steps"][3]["env"].update(
                {"SCRAPE_TOKEN": live_job(w)["steps"][3]["env"].pop("METRICS_SCRAPE_TOKEN")}),
            "dropped worker url": lambda w: live_job(w)["steps"][3]["env"].pop("STAGING_WORKER_URL"),
            "bearer on the job": lambda w: live_job(w).update({"env": {"METRICS_SCRAPE_TOKEN": bearer}}),
            "bearer on the restore": lambda w: live_job(w)["steps"][2].update(
                {"env": {"METRICS_SCRAPE_TOKEN": bearer}}),
            "extra staging secret": lambda w: live_job(w)["steps"][3]["env"].update(
                {"X": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}"}),
            "blanket secrets": lambda w: live_job(w)["steps"][3]["env"].update(
                {"ALL": "${{ toJSON(secrets) }}"}),
            "live any branch": lambda w: live_job(w).update(
                {"if": "(github.event.inputs.fixture || 'live') == 'live'"}),
            "live runs fixtures": lambda w: live_job(w).update({"if": "github.ref == 'refs/heads/main'"}),
            "live passes a fixture": lambda w: live_run(w, lambda run: run.replace(
                "args=(", "args=(--fixture fire ")),
            # argparse is last-wins: an appended flag overrides the pinned one.
            "live forced dry run": lambda w: live_run(w, lambda run: run.replace(
                '"${args[@]}"', '"${args[@]}" --dry-run')),
            "live inline input": lambda w: live_run(w, lambda run: run + 'echo "${{ github.event.inputs.dry_run }}"\n'),
            "live shell trace": lambda w: live_run(w, lambda run: "set -x\n" + run),
            "live write grant": lambda w: live_job(w).update(
                {"permissions": {"contents": "write", "issues": "write"}}),
            "live persisted credentials": lambda w: live_job(w)["steps"][0]["with"].update(
                {"persist-credentials": "true"}),
            "live checkout ref swap": lambda w: live_job(w)["steps"][0]["with"].update(
                {"ref": "evil-branch"}),
            "live checkout repository swap": lambda w: live_job(w)["steps"][0]["with"].update(
                {"repository": "evil/repo"}),
            "live save only on success": lambda w: live_job(w)["steps"][4].update({"if": "success()"}),
            # Fixture job: proof runs never reach the staging Environment.
            "fixture binds staging": lambda w: fixture_job(w).update({"environment": deepcopy(staging)}),
            "fixture binds deployment staging": lambda w: fixture_job(w).update({"environment": "staging"}),
            "fixture binds tracker environment": lambda w: fixture_job(w).update(
                {"environment": "slow-burn-tracker"}),
            "fixture reads the bearer": lambda w: fixture_job(w)["steps"][2]["env"].update(
                {"METRICS_SCRAPE_TOKEN": bearer}),
            "fixture reads the old secret": lambda w: fixture_job(w)["steps"][2]["env"].update(
                {"METRICS_SCRAPE_TOKEN": "${{ secrets.METRICS_SCRAPE_TOKEN }}"}),
            "fixture reads the staging url": lambda w: fixture_job(w)["steps"][2]["env"].update(
                {"STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}"}),
            "fixture blanket secrets": lambda w: fixture_job(w)["steps"][2]["env"].update(
                {"ALL": "${{ toJSON(secrets) }}"}),
            "fixture job-level secret": lambda w: fixture_job(w).update({"env": {"T": bearer}}),
            "fixture runs live": lambda w: fixture_job(w).update({"if": "github.ref == 'refs/heads/main'"}),
            "fixture any branch": lambda w: fixture_job(w).update(
                {"if": "github.event_name == 'workflow_dispatch' && (github.event.inputs.fixture == 'fire'"
                       " || github.event.inputs.fixture == 'clear')"}),
            "fixture drops the flag": lambda w: fixture_run(w, lambda run: run.replace(
                '--fixture "$TRACKER_FIXTURE" ', "")),
            "fixture live label": lambda w: fixture_run(w, lambda run: run.replace(
                "--label slow-burn-test ", "")),
            "fixture label override": lambda w: fixture_run(w, lambda run: run.replace(
                '"${args[@]}"', '"${args[@]}" --label slow-burn')),
            "fixture title override": lambda w: fixture_run(w, lambda run: run.replace(
                '"${args[@]}"', "\"${args[@]}\" --title-prefix '[voice-join-slow-burn]'")),
            "fixture inline input": lambda w: fixture_run(w, lambda run: run.replace(
                '"$TRACKER_FIXTURE"', '"${{ github.event.inputs.fixture }}"')),
            "fixture restores the ring": lambda w: fixture_job(w)["steps"].insert(
                2, deepcopy(live_job(w)["steps"][2])),
            "fixture saves the ring": lambda w: fixture_job(w)["steps"].insert(
                3, deepcopy(live_job(w)["steps"][4])),
            "fixture write grant": lambda w: fixture_job(w).update(
                {"permissions": {"contents": "write", "issues": "write"}}),
            "fixture persisted credentials": lambda w: fixture_job(w)["steps"][0]["with"].update(
                {"persist-credentials": "true"}),
            "fixture checkout ref swap": lambda w: fixture_job(w)["steps"][0]["with"].update(
                {"ref": "evil-branch"}),
            "fixture checkout repository swap": lambda w: fixture_job(w)["steps"][0]["with"].update(
                {"repository": "evil/repo"}),
            "fixture unrouted runner": lambda w: fixture_job(w).update({"runs-on": "ubuntu-latest"}),
            "fixture on tracker routing": lambda w: fixture_job(w).update(
                {"runs-on": ROUTED_RUNNER.format(job="tracker")}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["slow-burn-tracker.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_synthetic_missing_binding_notices_cannot_hide_failure(self):
        mutations = {
            "dead branch": lambda run: run.replace(
                "exit 1", "if false; then\nexit 1\nfi"),
            "early success": lambda run: run.replace("exit 1", "exit 0\nexit 1"),
            "uncalled function": lambda run: run.replace(
                "exit 1", "unused() {\nexit 1\n}"),
            "heredoc": lambda run: run.replace(
                "exit 1", "cat <<'EOF'\nexit 1\nEOF"),
        }
        for job_id in ("staging", "prod-passive"):
            original = self.workflows["synthetic-user-flow.yml"]["jobs"][job_id]["steps"][2]
            baseline = subprocess.run(
                ["bash", "--noprofile", "--norc", "-c", original["run"]],
                env={"PATH": os.defpath}, capture_output=True, text=True, timeout=5)
            self.assertEqual(baseline.returncode, 1)
            self.assertIn("::error::", baseline.stdout)
            for label, transform in mutations.items():
                with self.subTest(job=job_id, mutation=label):
                    changed = deepcopy(self.workflows)
                    notice = changed["synthetic-user-flow.yml"]["jobs"][job_id]["steps"][2]
                    notice["run"] = transform(notice["run"])
                    result = subprocess.run(
                        ["bash", "--noprofile", "--norc", "-c", notice["run"]],
                        env={"PATH": os.defpath}, capture_output=True, text=True, timeout=5)
                    self.assertEqual(result.returncode, 0)
                    self.assertTrue(scheduled_binding_audit_errors(changed), label)
                    self.assertTrue(synthetic_user_flow_errors(changed["synthetic-user-flow.yml"]), label)
                    self.assertTrue(workflow_policy_errors(changed), label)
            for scope in ("step", "job", "workflow"):
                with self.subTest(job=job_id, mutation="shell override", scope=scope):
                    changed = deepcopy(self.workflows)
                    workflow = changed["synthetic-user-flow.yml"]
                    job = workflow["jobs"][job_id]
                    if scope == "step":
                        job["steps"][2]["shell"] = "echo {0}"
                    else:
                        holder = job if scope == "job" else workflow
                        holder["defaults"] = {"run": {"shell": "echo {0}"}}
                    self.assertTrue(scheduled_binding_audit_errors(changed))
                    self.assertTrue(synthetic_user_flow_errors(workflow))
                    self.assertTrue(workflow_policy_errors(changed))

    def test_five_minute_schedulers_fail_closed_on_missing_bindings(self):
        workflows = self.workflows
        self.assertEqual(scheduled_binding_audit_errors(workflows), [])

        def tracker_run_step(copy):
            steps = copy["slow-burn-tracker.yml"]["jobs"]["tracker"]["steps"]
            return next(step for step in steps
                        if "slow_burn_tracker.py" in step.get("run", ""))

        def tracker_env(copy):
            return tracker_run_step(copy)["env"]

        def notice_step(copy, job_id):
            steps = copy["synthetic-user-flow.yml"]["jobs"][job_id]["steps"]
            return next(step for step in steps
                        if str(step.get("name", "")).startswith("Fail closed when the "))

        def notice_run(copy, job_id, transform):
            notice_step(copy, job_id)["run"] = transform(notice_step(copy, job_id)["run"])

        mutations = {
            "slow-burn schedule coarsened": lambda w: w["slow-burn-tracker.yml"]["on"][
                "schedule"].__setitem__(0, {"cron": "*/6 * * * *"}),
            "nightly gains a five-minute schedule": lambda w: w["nightly.yml"]["on"][
                "schedule"].__setitem__(0, {"cron": "*/5 * * * *"}),
            "tracker drops the bearer handoff": lambda w: tracker_env(w).pop(
                "METRICS_SCRAPE_TOKEN"),
            "tracker drops the url handoff": lambda w: tracker_env(w).pop(
                "STAGING_WORKER_URL"),
            "tracker run step gated": lambda w: tracker_run_step(w).__setitem__(
                "if", "false"),
            "tracker run tolerates failure": lambda w: tracker_run_step(w).__setitem__(
                "continue-on-error", True),
            "tracker run step shell override": lambda w: tracker_run_step(w).__setitem__(
                "shell", "echo {0}"),
            "tracker job shell default": lambda w: w["slow-burn-tracker.yml"]["jobs"][
                "tracker"].__setitem__("defaults", {"run": {"shell": "echo {0}"}}),
            "slow-burn workflow shell default": lambda w: w["slow-burn-tracker.yml"].__setitem__(
                "defaults", {"run": {"shell": "echo {0}"}}),
            "staging notice gated off": lambda w: notice_step(w, "staging").__setitem__(
                "if", "false"),
            "staging notice tolerates failure": lambda w: notice_step(w, "staging").__setitem__(
                "continue-on-error", True),
            "staging notice neutralizes its exit": lambda w: notice_run(
                w, "staging", lambda run: run.replace("exit 1", "exit 1 || true")),
            "staging notice warns": lambda w: notice_run(
                w, "staging", lambda run: run.replace("::error::", "::warning::")),
            "staging notice loses its exit": lambda w: notice_run(
                w, "staging", lambda run: run.replace("exit 1", "exit 0")),
            "staging notice drops the binding name": lambda w: notice_run(
                w, "staging", lambda run: run.replace("STAGING_WORKER_URL", "the target var")),
            "staging notice renamed": lambda w: notice_step(w, "staging").update(
                {"name": "Note"}),
            "prod notice warns": lambda w: notice_run(
                w, "prod-passive", lambda run: run.replace("::error::", "::warning::")),
            "prod notice loses its exit": lambda w: notice_run(
                w, "prod-passive", lambda run: run.replace("exit 1", "exit 0")),
            "prod notice gated off": lambda w: notice_step(w, "prod-passive").__setitem__(
                "if", "false"),
            "prod notice tolerates failure": lambda w: notice_step(w, "prod-passive").__setitem__(
                "continue-on-error", True),
            "prod notice neutralizes its exit": lambda w: notice_run(
                w, "prod-passive", lambda run: run.replace("exit 1", "exit 1 || true")),
        }
        for label, change in mutations.items():
            with self.subTest(change=label):
                changed = deepcopy(workflows)
                change(changed)
                self.assertTrue(scheduled_binding_audit_errors(changed), label)
                self.assertTrue(workflow_policy_errors(changed), label)

    def test_gradual_promote_proof_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(gradual_promote_proof_errors(workflows["gradual-promote-proof.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["gradual-promote-proof.yml"])
            change(copy)
            return gradual_promote_proof_errors(copy)

        def job(copy):
            return copy["jobs"]["proof"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "rollback-only mode": lambda w: w["on"]["workflow_dispatch"]["inputs"]["mode"].update(
                {"options": ["rollback"]}),
            "mode default rollback": lambda w: w["on"]["workflow_dispatch"]["inputs"]["mode"].update(
                {"default": "rollback"}),
            "dropped domain input": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("domain"),
            "extra input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update(
                {"extra": {"type": "string", "required": "false"}}),
            "confirm default": lambda w: w["on"]["workflow_dispatch"]["inputs"]["confirm_worker"].update(
                {"default": "two-bot-next-gradual-proof"}),
            "top-level permissions": lambda w: w.update({"permissions": {"contents": "read"}}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "shared concurrency group": lambda w: w["concurrency"].update({"group": "deploy-staging"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(job(w))}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "write grant": lambda w: job(w).update({"permissions": {"contents": "write"}}),
            "longer timeout": lambda w: job(w).update({"timeout-minutes": "60"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "other job's routing": lambda w: job(w).update(
                {"runs-on": ROUTED_RUNNER.format(job="check")}),
            "repinned action": lambda w: job(w)["steps"][3].update(
                {"uses": "TogetherWeOwn/.github/actions/gradual-promote@main"}),
            "dropped error-rate threshold": lambda w: job(w)["steps"][3]["with"].pop("error-rate-threshold"),
            "widened allow-indexable": lambda w: job(w)["steps"][3]["with"].update({"allow-indexable": "true"}),
            "production secret": lambda w: job(w)["steps"][3]["with"].update(
                {"api-token": "${{ secrets.PRODUCTION_CLOUDFLARE_API_TOKEN }}"}),
            "inline expression": lambda w: job(w)["steps"][2].update(
                {"run": 'echo "${{ inputs.confirm_worker }}"'}),
            "upload may be missing": lambda w: job(w)["steps"][4]["with"].update({"if-no-files-found": "error"}),
            "upload not always": lambda w: job(w)["steps"][4].pop("if"),
            "checkout conditional": lambda w: job(w)["steps"][0].update({"if": "always()"}),
            "guard conditional": lambda w: job(w)["steps"][2].update({"if": "always()"}),
            "gradual continue-on-error": lambda w: job(w)["steps"][3].update({"continue-on-error": "true"}),
            "guard without length gate": lambda w: job(w)["steps"][2].update(
                {"run": job(w)["steps"][2]["run"].replace("${#CANDIDATE_TAG}", "CANDIDATE_TAG")}),
            "guard without single-label rejection": lambda w: job(w)["steps"][2].update(
                {"run": job(w)["steps"][2]["run"].replace("two-bot-next-gradual-proof.*.*.workers.dev",
                                                         "two-bot-next-gradual-proof-rejected")}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["gradual-promote-proof.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_check_toolchains_match_the_repository_pin(self):
        channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        self.assertRegex(channel, r"^\d+\.\d+\.\d+$")
        installers = {
            job_id: [step for step in job.get("steps", [])
                     if step.get("uses", "").startswith("dtolnay/rust-toolchain@")]
            for job_id, job in self.workflows["check.yml"]["jobs"].items()
        }
        installers = {job: steps for job, steps in installers.items() if steps}
        self.assertEqual(set(installers), {
            "check", "rust-tests", "coverage-plan", "coverage-shard",
            "ignored-db-stores", "ignored-db-runtime",
            "moderation-db", "self-role-store", "community-db", "tickets-postgres", "feeds-db",
            "fuzz-compile",
        })
        for job_id, steps in installers.items():
            with self.subTest(job=job_id):
                self.assertEqual(len(steps), 1)
                if job_id == "fuzz-compile":
                    # libFuzzer needs nightly; pinned date, not floating stable.
                    self.assertEqual(steps[0].get("with", {}).get("toolchain"), "nightly-2026-10-01")
                    self.assertEqual(set(steps[0]["with"]["components"].replace(" ", "").split(",")),
                                     {"rust-src"})
                    continue
                # Floating stable can install a different fmt/clippy than Cargo
                # selects from the repository pin inside the job container.
                self.assertEqual(steps[0].get("with", {}).get("toolchain"), channel)
        self.assertEqual(set(installers["check"][0]["with"]["components"].replace(" ", "").split(",")),
                         {"rustfmt", "clippy"})

    def pinned_toolchain_lanes(self, channel):
        # Every PR-gating job that installs the repository pin: all of
        # check.yml (the ci-ok aggregate) and the shared nextest lane it
        # calls. The nightly-pinned fuzz lane installs a different toolchain
        # and is excluded by the channel match.
        lanes = []
        for workflow_name in ("check.yml", "reusable-rust-nextest.yml"):
            for job_id, job in self.workflows[workflow_name]["jobs"].items():
                if any(str(step.get("uses", "")).startswith("dtolnay/rust-toolchain@")
                       and step.get("with", {}).get("toolchain") == channel
                       for step in job.get("steps", [])):
                    lanes.append((workflow_name, job_id))
        # Sentinels: an enumeration that silently found nothing would pass
        # every subtest vacuously.
        for lane in (("check.yml", "ignored-db-runtime"), ("reusable-rust-nextest.yml", "nextest")):
            self.assertIn(lane, lanes)
        self.assertNotIn(("check.yml", "fuzz-compile"), lanes)
        return lanes

    def test_rust_toolchain_installs_are_retry_guarded(self):
        # `rustup toolchain install` inside the installer failed once on main
        # with `Network is unreachable` syncing the channel manifest, failing
        # the job before any test ran (green on re-run). The upstream
        # installer retries only checksum-rotation failures, so the installer
        # continues on error and the next step retries the same pin and fails
        # closed. The retry must request the repository pin and every
        # component the installer requests, or a green retry can mask a drift.
        # Every PR-gating lane that installs the pin must carry the guard: a
        # 4-second install failure on ignored-db-runtime reddened PR #984
        # while the identical step passed on the other lanes of the same
        # head. The lanes are enumerated from the workflows, not listed, so a
        # new lane cannot install the pin unguarded. Scheduled and manual
        # workflows (nightly, benchmark, fuzz smoke) do not gate a PR and
        # stay out of scope.
        channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        targets = self.pinned_toolchain_lanes(channel)
        for workflow_name, job_id in targets:
            with self.subTest(workflow=workflow_name, job=job_id):
                steps = self.workflows[workflow_name]["jobs"][job_id]["steps"]
                installers = [index for index, step in enumerate(steps)
                              if str(step.get("uses", "")).startswith("dtolnay/rust-toolchain@")]
                self.assertEqual(len(installers), 1)
                index = installers[0]
                installer = steps[index]
                self.assertTrue(installer.get("continue-on-error"),
                                "toolchain installer must continue so the retry step can run")
                self.assertIn("id", installer)
                retry = steps[index + 1]
                self.assertNotIn("uses", retry)
                self.assertIn("run", retry)
                # Container steps default to sh, where `set -euo pipefail`
                # aborts before any retry runs (b7753a4). The retry needs bash.
                self.assertEqual(retry.get("shell"), "bash",
                                 "toolchain retry must run under bash, not container sh")
                self.assertEqual(retry.get("if"), f"steps.{installer['id']}.outcome == 'failure'")
                self.assertNotEqual(retry.get("continue-on-error"), True,
                                    "the retry must fail the job when the toolchain stays missing")
                self.assertIn(channel, retry["run"])
                components = installer.get("with", {}).get("components", "")
                for component in filter(None, components.replace(" ", "").split(",")):
                    with self.subTest(component=component):
                        self.assertIn(component, retry["run"])
                with self.subTest(change="retry step removed"):
                    workflows = deepcopy(self.workflows)
                    del workflows[workflow_name]["jobs"][job_id]["steps"][index + 1]
                    retry_steps = workflows[workflow_name]["jobs"][job_id]["steps"]
                    still_guarded = (
                        len(retry_steps) > index + 1
                        and "run" in retry_steps[index + 1]
                        and retry_steps[index + 1].get("if") == f"steps.{installer['id']}.outcome == 'failure'"
                    )
                    self.assertFalse(still_guarded, "removing the retry step must fail this guard")

    def test_rust_toolchain_retry_fails_closed_behaviorally(self):
        # Behavioral pin for the retry guard above: substrings alone cannot
        # pin branch selection (deleting `[ "$attempt" -lt 5 ] || exit 1`
        # keeps every substring while the step exits 0 with no toolchain
        # installed) or the exact pin (a `rustup default stable` keeps the
        # install-line substring). Run each retry step under bash with
        # function-shimmed fake `rustup`/`sleep` (a function shim, not a
        # PATH shim, so the pin also runs in harnesses that sanitize PATH
        # on exec): always-failing rustup exits 1 after exactly 5 installs
        # with no default, and rustup succeeding on attempt 2 exits 0 having
        # defaulted the exact repository pin.
        import os
        import subprocess
        import tempfile

        channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        targets = self.pinned_toolchain_lanes(channel)
        fake_prelude = (
            "rustup() {\n"
            "  printf '%s\\n' \"$*\" >> \"$FAKE_RUSTUP_CALLS\"\n"
            "  FAKE_N=$((${FAKE_N:-0} + 1))\n"
            "  if [ \"$FAKE_N\" -lt \"$FAKE_SUCCEED_ON\" ]; then return 1; fi\n"
            "  return 0\n"
            "}\n"
            "sleep() { :; }\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            calls_path = tmp_path / "rustup-calls.txt"
            for workflow_name, job_id in targets:
                steps = self.workflows[workflow_name]["jobs"][job_id]["steps"]
                installers = [index for index, step in enumerate(steps)
                              if str(step.get("uses", "")).startswith("dtolnay/rust-toolchain@")]
                self.assertEqual(len(installers), 1)
                run_text = str(steps[installers[0] + 1]["run"])
                for case_name, succeed_on, expected_status, expected_installs in (
                    ("always-fails", "99", 1, 5),
                    ("succeeds-on-2", "2", 0, 2),
                ):
                    with self.subTest(workflow=workflow_name, job=job_id, case=case_name):
                        calls_path.write_text("", encoding="utf-8")
                        run_env = dict(os.environ)
                        run_env.update(
                            {
                                "FAKE_RUSTUP_CALLS": str(calls_path),
                                "FAKE_SUCCEED_ON": succeed_on,
                            }
                        )
                        proc = subprocess.run(
                            ["bash", "-c", fake_prelude + run_text],
                            env=run_env,
                            capture_output=True,
                            text=True,
                            timeout=30,
                        )
                        self.assertEqual(
                            proc.returncode, expected_status,
                            f"unexpected retry status: {proc.stderr}",
                        )
                        calls = calls_path.read_text(encoding="utf-8").splitlines()
                        installs = [line for line in calls if line.startswith("toolchain install ")]
                        self.assertEqual(len(installs), expected_installs,
                                         f"expected {expected_installs} installs, got: {calls}")
                        for line in installs:
                            self.assertIn(channel, line,
                                          f"retry must install the exact pin, got: {line!r}")
                        defaults = [line for line in calls if line.startswith("default ")]
                        if expected_status == 0:
                            self.assertIn(f"default {channel} --force-non-host", defaults,
                                          f"retry must default the exact pin, got: {calls}")
                        else:
                            self.assertEqual(defaults, [],
                                             f"failed retry must not default anything, got: {calls}")

    def early_pinned_components_retry(self):
        # This run-step installer is not a dtolnay action. Require exactly one
        # so removing it (or changing its id) cannot make the guard vacuous.
        steps = self.workflows["check.yml"]["jobs"]["check"]["steps"]
        installers = [index for index, step in enumerate(steps)
                      if step.get("id") == "pinned_components"]
        self.assertEqual(len(installers), 1, "check must install pinned_components before pip")
        index = installers[0]
        installer = steps[index]
        self.assertNotIn("uses", installer)
        self.assertEqual(installer.get("run", "").strip(),
                         "rustup toolchain install --no-self-update")
        # UniqueKeyLoader is a BaseLoader: even 'false' is a truthy string.
        self.assertEqual(installer.get("continue-on-error"), "true")
        self.assertLess(index + 1, len(steps), "the early installer needs an immediate retry")
        retry = steps[index + 1]
        self.assertNotIn("uses", retry)
        self.assertTrue(retry.get("run"), "the immediate retry must execute a script")
        self.assertEqual(retry.get("if"), "steps.pinned_components.outcome == 'failure'")
        self.assertEqual(retry.get("shell"), "bash")
        self.assertEqual(retry.get("env", {}).get("RUSTUP_PERMIT_COPY_RENAME"), "1")
        self.assertNotIn("continue-on-error", retry, "the early retry must not swallow failure")
        pip_steps = [index for index, step in enumerate(steps)
                     if "python3 -m pip" in step.get("run", "")]
        self.assertTrue(pip_steps, "check must retain its Python tooling install")
        self.assertLess(index + 1, min(pip_steps), "finish the toolchain before pip probes rustc")
        return retry

    def test_early_rust_toolchain_install_is_retry_guarded(self):
        self.early_pinned_components_retry()

    def test_early_rust_toolchain_retry_fails_closed_behaviorally(self):
        # Execute the real retry, but never rustup or sleep: only those external
        # effects are shimmed. The early install resolves rust-toolchain.toml;
        # unlike the action retries it must not issue a new `rustup default`.
        retry = self.early_pinned_components_retry()
        fake_prelude = (
            "FAKE_N=0\n"
            "rustup() {\n"
            "  printf '%s\\0' \"$@\" >> \"$FAKE_RUSTUP_CALLS\"\n"
            "  printf '\\n' >> \"$FAKE_RUSTUP_CALLS\"\n"
            "  FAKE_N=$((FAKE_N + 1))\n"
            "  [ \"$FAKE_N\" -ge \"$FAKE_SUCCEED_ON\" ]\n"
            "}\n"
            "sleep() { :; }\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            calls_path = Path(tmp) / "rustup-calls.txt"
            for case_name, succeed_on, expected_status, expected_installs in (
                ("always-fails", "99", 1, 5),
                ("succeeds-on-2", "2", 0, 2),
            ):
                with self.subTest(case=case_name):
                    calls_path.write_bytes(b"")
                    # No inherited credentials/startup hooks, real toolchain
                    # directories or PATH tools, even if a script drifts.
                    run_env = {
                        "PATH": tmp,
                        "HOME": tmp,
                        "CARGO_HOME": str(Path(tmp) / "cargo"),
                        "RUSTUP_HOME": str(Path(tmp) / "rustup"),
                        "RUSTUP_PERMIT_COPY_RENAME": retry["env"]["RUSTUP_PERMIT_COPY_RENAME"],
                        "FAKE_RUSTUP_CALLS": str(calls_path),
                        "FAKE_SUCCEED_ON": succeed_on,
                    }
                    proc = subprocess.run(
                        ["/bin/bash", "--noprofile", "--norc", "-eo", "pipefail",
                         "-c", fake_prelude + retry["run"]],
                        env=run_env, capture_output=True, text=True, timeout=5,
                    )
                    self.assertEqual(proc.returncode, expected_status,
                                     f"unexpected early retry status: {proc.stderr}")
                    self.assertEqual(
                        calls_path.read_bytes().splitlines(),
                        [b"toolchain\0install\0--no-self-update\0"] * expected_installs,
                        "early retry must install exactly the repository-selected toolchain; "
                        "no extra arguments or default command",
                    )

    def test_overflow_runner_must_name_its_own_job(self):
        self.assertTrue(runner_allowed("worker", self.workflows["check.yml"]["jobs"]["worker"]["runs-on"]))
        for runs_on in (ROUTED_RUNNER.format(job="check"), "ubuntu-latest", ["self-hosted"],
                        "${{ vars.CI_OVERFLOW_RUNNER }}"):
            with self.subTest(runs_on=runs_on):
                self.assertFalse(runner_allowed("worker", runs_on))

    def test_certify_promotion_runner_must_use_its_own_routed_expression(self):
        # Reconciliation pin: certification is artifact-only, but that buys
        # service/container isolation, not a runner waiver. The real job must
        # use the exact canonical routed expression naming certify-promotion,
        # matching scripts/test-runner-routing.py::routed('certify-promotion').
        canonical = ROUTED_RUNNER.format(job="certify-promotion")
        job = self.workflows["deploy-staging.yml"]["jobs"]["certify-promotion"]
        self.assertTrue(runner_allowed("certify-promotion", canonical))
        # Public repositories reach GitHub-hosted runners before any variable
        # or self-hosted label is read.
        self.assertTrue(canonical.startswith(
            "${{ fromJSON((!github.event.repository.private && '[\"ubuntu-latest\"]') || "))
        missing_public_first = canonical.replace(
            "(!github.event.repository.private && '[\"ubuntu-latest\"]') || ", "")
        self.assertNotEqual(missing_public_first, canonical)
        for runs_on in ("ubuntu-latest",
                        ROUTED_RUNNER.format(job="deploy"),
                        missing_public_first,
                        ["self-hosted"],
                        ["self-hosted", "two-selfhosted"],
                        "${{ vars.CI_OVERFLOW_RUNNER }}"):
            with self.subTest(runs_on=runs_on):
                self.assertFalse(runner_allowed("certify-promotion", runs_on))
        self.assertNotIn("services", job)
        self.assertNotIn("container", job)
        self.assertNotIn("environment", job)
        self.assertNotIn("secrets", job)
        self.assertEqual(job["permissions"], {"contents": "read", "actions": "read"})
        self.assertEqual(job["runs-on"], canonical)

    def test_duplicate_keys_cannot_hide_an_enabled_job(self):
        with self.assertRaises(ValueError):
            yaml.load("jobs:\n  deploy:\n    if: '${{ false }}'\n    if: true\n", Loader=UniqueKeyLoader)

    def test_review_rules_stays_read_only_and_shadow(self):
        workflow = self.workflows["review-rules.yml"]
        self.assertEqual(set(workflow["on"]), {"pull_request", "merge_group"})
        self.assertEqual(workflow["permissions"], {})
        job = workflow["jobs"]["review-rules"]
        self.assertEqual(job["permissions"], {"contents": "read"})
        self.assertEqual(job["with"], {"packs": "rust,security,python-shell-ci", "mode": "warn"})
        self.assertNotIn("secrets", job)
        self.assertNotIn("environment", job)
        self.assertEqual(job["uses"], REUSABLE_CALLS[("review-rules.yml", "review-rules")])

    def test_private_runner_and_service_isolation(self):
        for name, workflow in self.workflows.items():
            for job_id, job in workflow["jobs"].items():
                with self.subTest(workflow=name, job=job_id):
                    if "uses" in job:
                        # The called workflow's own jobs are checked here too.
                        self.assertNotIn("runs-on", job)
                        self.assertEqual(job["uses"], REUSABLE_CALLS[(name, job_id)])
                        continue
                    if (name, job_id) == ("deploy-staging.yml", "certify-promotion"):
                        # Artifact-only certification has no host/container dependency,
                        # but it still uses the canonical routed expression for its
                        # own job id (public repo -> hosted first). A literal
                        # ubuntu-latest would bill private hosted minutes if the
                        # repo went private; see scripts/test-runner-routing.py.
                        self.assertEqual(job["runs-on"], ROUTED_RUNNER.format(job=job_id))
                        self.assertNotIn("services", job)
                        self.assertNotIn("container", job)
                    else:
                        # Main #183 returned container smoke to the shared pool.
                        self.assertTrue(runner_allowed(job_id, job["runs-on"]))
                    if job.get("services"):
                        self.assertIn("container", job)
                        for service in job["services"].values():
                            self.assertNotIn("ports", service)

    def test_nextest_lane_declares_bash_and_keeps_strict_artifacts(self):
        # SDLC 2.3 CI speed pilot: the reusable lane runs in a job container
        # whose default shell is sh, so a pipefail script without an explicit
        # shell exits 2, no tests run, and the downstream parse/upload steps
        # fail on the missing files and mask the root cause. Every run step
        # declares bash, the parse names its upstream on a missing JUnit, and
        # the artifact uploads keep failing when their files are absent.
        job = self.workflows["reusable-rust-nextest.yml"]["jobs"]["nextest"]
        run_steps = [step for step in job.get("steps", []) if "run" in step]
        self.assertGreater(len(run_steps), 0)
        for step in run_steps:
            with self.subTest(step=step.get("name")):
                self.assertEqual(step.get("shell"), "bash")
        select = next(step for step in run_steps
                      if str(step.get("name", "")).startswith("Select sccache backend"))
        self.assertIn("pipefail", str(select.get("run", "")))
        run_nextest = next(step for step in run_steps
                           if str(step.get("name", "")).startswith("cargo nextest"))
        run_text = str(run_nextest.get("run", ""))
        # The JUnit path in .config/nextest.toml is profile-store relative
        # (target/nextest/ci/junit.xml), not workspace-root: the step must
        # resolve it and copy to the workspace root, keep failing when truly
        # absent, and preserve a failing nextest status with its evidence.
        self.assertIn("target/nextest/ci/junit.xml", run_text)
        self.assertIn("cp ", run_text)
        self.assertIn("test -f junit.xml", run_text)
        self.assertIn("exit $status", run_text)
        parse = next(step for step in run_steps
                     if str(step.get("name", "")).startswith("Parse JUnit"))
        self.assertIn("-f junit.xml", str(parse.get("run", "")))
        uploads = [step for step in job.get("steps", [])
                   if str(step.get("uses", "")).startswith("actions/upload-artifact@")]
        self.assertGreater(len(uploads), 0)
        for step in uploads:
            with self.subTest(upload=step.get("name")):
                self.assertEqual(step["with"]["if-no-files-found"], "error")

    def test_nextest_ci_profile_disables_fail_fast(self):
        # Fail-fast cancels not-yet-started tests after the first exhausted
        # failure, so junit.xml omits them and per-run ledger rows stop
        # being comparable. The evidence lane must run everything.
        import tomllib
        config = tomllib.loads((ROOT / ".config/nextest.toml").read_text(encoding="utf-8"))
        profile = config["profile"]["ci"]
        self.assertEqual(profile.get("retries"), 1)
        self.assertIs(profile.get("fail-fast"), False)

    def test_nextest_shared_backends_stay_off_pull_requests(self):
        # R2 write credentials must never reach PR- or merge-queue-built code
        # (build scripts and tests execute at a head that is not on `main`
        # yet, and a shared R2 bucket has no per-PR scope: a PR could poison
        # objects main later links). check.yml runs on merge_group, so a
        # pull_request-only denylist is not enough, and workflow_dispatch
        # accepts any branch or tag, so the event name alone is not enough
        # either. Only push and workflow_dispatch runs on `main` may use
        # shared backends; every other event or ref uses sccache on local
        # disk only. The GitHub-cache backend has the same quota shape: the
        # lane's entries share the repo Actions cache with the warm `check`
        # entry, so non-main runs must not grow it.
        lane = self.workflows["reusable-rust-nextest.yml"]["jobs"]["nextest"]
        select = next(step for step in lane.get("steps", [])
                      if str(step.get("name", "")).startswith("Select sccache backend"))
        run_text = str(select.get("run", ""))
        self.assertIn("workflow_dispatch", run_text)
        self.assertIn("github.event_name", str(select.get("env", "")))
        self.assertIn("github.ref", str(select.get("env", "")))
        self.assertIn("local disk", run_text)
        # The allowlist branch must come FIRST: a denylist (`!=`) or a
        # reordered R2 branch ahead of it would let shared-backend
        # credentials reach merge_group runs as soon as any secret is set,
        # while every substring above stays present. Nothing
        # credential-shaped may precede the guard.
        lines = run_text.splitlines()
        gate = next(i for i, line in enumerate(lines)
                    if "NEXTEST_EVENT" in line)
        # Exact pin: a denylist, dead (&& false), reordered, or ref-less
        # guard all keep the `if ` prefix, so pin the full first line.
        self.assertEqual(
            lines[gate].strip(),
            'if { [ "$NEXTEST_EVENT" = "push" ] || [ "$NEXTEST_EVENT" = "workflow_dispatch" ]; } && [ "$NEXTEST_REF" = "refs/heads/main" ]; then',
            f"push/workflow_dispatch-on-main allowlist must be the exact first branch, got: {lines[gate]!r}",
        )
        before = "\n".join(lines[:gate])
        self.assertNotIn("GITHUB_ENV", before)
        self.assertNotIn("R2_", before)
        self.assertNotIn("SCCACHE_", before)
        indent = lines[gate][: len(lines[gate]) - len(lines[gate].lstrip())]

        def outer_boundary(start, word):
            return next(
                i
                for i in range(start, len(lines))
                if lines[i].strip() == word
                and lines[i][: len(lines[i]) - len(lines[i].lstrip())] == indent
            )

        outer_else = outer_boundary(gate + 1, "else")
        outer_fi = outer_boundary(outer_else + 1, "fi")
        # The local-disk body must write nothing shared: an extra
        # SCCACHE_/R2_/GITHUB_ENV write inside it keeps every substring above
        # while leaking credentials on PR and merge-queue runs.
        disk_body = "\n".join(lines[outer_else + 1 : outer_fi])
        self.assertIn("local disk", disk_body)
        self.assertNotIn("GITHUB_ENV", disk_body)
        self.assertNotIn("R2_", disk_body)
        self.assertNotIn("SCCACHE_", disk_body)
        self.assertNotIn("AWS_", disk_body)
        # The shared backends are still configured on main: this guards
        # against gutting the step instead of guarding it.
        self.assertIn("GITHUB_ENV", "\n".join(lines[gate + 1 : outer_else]))
        # Behavioral pin: run the extracted step under bash. On
        # pull_request, merge_group, and other events the env file stays
        # empty even with all R2 vars set, and so it does on push and
        # workflow_dispatch runs at a ref other than `main` (a manual
        # dispatch can target any branch or tag); only push and
        # workflow_dispatch on `main` write the R2 lines. Catches
        # inverted/dead guards and disk-branch writes that structural pins
        # miss.
        import os
        import subprocess
        import tempfile

        r2_env = {
            "R2_BUCKET": "test-bucket",
            "R2_ENDPOINT": "https://test.example.com",
            "R2_REGION": "auto",
            "R2_KEY": "test-key",
            "R2_SECRET": "test-secret",
        }
        with tempfile.NamedTemporaryFile(mode="r", delete=False) as tmp:
            backend_env_path = tmp.name
        try:
            base = dict(os.environ)
            withheld = (
                ("pull_request", "refs/pull/1/merge"),
                ("merge_group", "refs/heads/gh-readonly-queue/main/pr-1-abc"),
                ("schedule", "refs/heads/main"),
                ("push", "refs/heads/some-feature"),
                ("workflow_dispatch", "refs/heads/some-feature"),
                ("workflow_dispatch", "refs/tags/v1.0.0"),
            )
            for event, ref in withheld:
                with self.subTest(event=event, ref=ref):
                    run_env = dict(base)
                    run_env.update(
                        {
                            "NEXTEST_EVENT": event,
                            "NEXTEST_REF": ref,
                            **r2_env,
                            "GITHUB_ENV": backend_env_path,
                        }
                    )
                    Path(backend_env_path).write_text("", encoding="utf-8")
                    proc = subprocess.run(
                        ["bash", "-c", run_text],
                        env=run_env,
                        capture_output=True,
                        text=True,
                        timeout=30,
                    )
                    self.assertEqual(proc.returncode, 0, f"{event} backend step failed: {proc.stderr}")
                    self.assertEqual(
                        Path(backend_env_path).read_text(encoding="utf-8"),
                        "",
                        f"{event} run at {ref} must not write shared-backend credentials",
                    )
            for event in ("push", "workflow_dispatch"):
                with self.subTest(event=event, ref="refs/heads/main"):
                    run_env = dict(base)
                    run_env.update(
                        {
                            "NEXTEST_EVENT": event,
                            "NEXTEST_REF": "refs/heads/main",
                            **r2_env,
                            "GITHUB_ENV": backend_env_path,
                        }
                    )
                    Path(backend_env_path).write_text("", encoding="utf-8")
                    proc = subprocess.run(
                        ["bash", "-c", run_text],
                        env=run_env,
                        capture_output=True,
                        text=True,
                        timeout=30,
                    )
                    self.assertEqual(proc.returncode, 0, f"{event} backend step failed: {proc.stderr}")
                    written = Path(backend_env_path).read_text(encoding="utf-8")
                    self.assertIn("SCCACHE_BUCKET=test-bucket", written)
                    self.assertIn("AWS_ACCESS_KEY_ID=test-key", written)
        finally:
            try:
                os.unlink(backend_env_path)
            except FileNotFoundError:
                pass
        raw = (ROOT / ".github/workflows/reusable-rust-nextest.yml").read_text(encoding="utf-8")
        self.assertNotIn("adds no cache footprint", raw)

    def test_required_nextest_withholds_r2_secrets_on_pull_request(self):
        # Even with the callee-side guard, the caller must forward R2 write
        # secrets only on push and workflow_dispatch runs on `main`.
        # check.yml runs on merge_group too, so a `!= 'pull_request'` gate
        # would forward the secrets to queued code that is not on `main`
        # yet, and workflow_dispatch accepts any branch or tag, so the event
        # name without the ref would forward them to a manual run off
        # `main`. The gate must read `ref == 'refs/heads/main' && (== 'push'
        # || == 'workflow_dispatch') && secrets.X || ''`: the seemingly
        # equivalent `== 'pull_request' && '' || secrets.X` forwards the
        # secret anyway, because '' is falsy in GitHub expressions.
        job = self.workflows["check.yml"]["jobs"]["nextest"]
        r2_keys = ("SCCACHE_BUCKET", "SCCACHE_ENDPOINT", "SCCACHE_REGION",
                   "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY")
        for key in r2_keys:
            with self.subTest(secret=key):
                # Exact pin: the allowlist form withholds the secret on
                # pull_request, merge_group, and any ref off `main`. A
                # `!= 'pull_request'` form, an event-only form, or a leading
                # `== ... && ''` form forwards it on some dispatch while
                # keeping every substring present, so substrings cannot pin
                # this gate.
                self.assertEqual(
                    str(job["secrets"][key]),
                    "${{ github.ref == 'refs/heads/main' && (github.event_name == 'push' || github.event_name == 'workflow_dispatch') && secrets." + key + " || '' }}",
                )

    def test_required_nextest_caller_shape_is_pinned(self):
        # The promoted lane is a required check.yml job: least-privilege
        # grant, rust-selector gate like the other Rust test lanes, exact
        # head SHA into the reusable lane, and R2 secrets only off-PR
        # (pinned by the withholding test above).
        job = self.workflows["check.yml"]["jobs"]["nextest"]
        self.assertEqual(job.get("name"), "rust nextest")
        self.assertEqual(job.get("permissions"), {"contents": "read"})
        self.assertIn("job-inputs", job.get("needs", []))
        self.assertIn("needs.job-inputs.outputs.rust != 'false'", job.get("if", ""))
        self.assertEqual(job.get("uses"), "./.github/workflows/reusable-rust-nextest.yml")
        self.assertEqual(job["with"]["ref"], "${{ github.event.pull_request.head.sha || github.sha }}")
        self.assertNotIn("runs-on", job)
        agg = self.workflows["check.yml"]["jobs"]["ci-ok"]
        self.assertIn("nextest", agg["needs"])
        self.assertIn("NEXTEST_RESULT", "\n".join(step.get("run", "") for step in agg["steps"]))

    def test_required_nextest_timeout_ceiling_is_pinned(self):
        # Gate ceiling: the required lane must keep its 45-minute ceiling.
        # Raising it hides a wedged runner; the pin forces a reviewed change.
        job = self.workflows["reusable-rust-nextest.yml"]["jobs"]["nextest"]
        # BaseLoader keeps scalars as strings; "45" is the 45-minute ceiling.
        self.assertEqual(job.get("timeout-minutes"), "45")

    def test_nightly_sweep_timeout_ceilings_are_pinned(self):
        # Schedule/dispatch get 180 minutes for the serial ignored sweep; a PR
        # run keeps 45. The pin forces a reviewed change to either ceiling.
        job = self.workflows["nightly.yml"]["jobs"]["sweep"]
        self.assertEqual(
            job.get("timeout-minutes"),
            "${{ github.event_name == 'pull_request' && 45 || 180 }}",
        )

    def test_nextest_container_pins_disposable_database_service_address(self):
        # Same hardening as the check.yml lanes: the shared lane runs the
        # workspace lib/bins against the same disposable service, so it pins
        # the job-scoped address before the loopback forward instead of
        # depending on service-alias DNS for the whole run.
        job = self.workflows["reusable-rust-nextest.yml"]["jobs"]["nextest"]
        prerequisites = next(
            step for step in job["steps"]
            if step.get("name") == "Job-container prerequisites (loopback forward to agent-testdb)"
        )
        self.assertEqual(prerequisites.get("shell"), "bash")
        run = prerequisites["run"]
        self.assertIn("set -e", run)
        hosts_tmp = run.index('hosts_tmp="${RUNNER_TEMP:?RUNNER_TEMP must be set}/hosts"')
        resolve = run.index("getent ahostsv4 agent-testdb")
        remove_stale_pin = run.index("sed -E 's/(^|[[:space:]])agent-testdb([[:space:]]|$)/ /g' /etc/hosts > \"$hosts_tmp\"")
        self.assertNotIn("sed -i -E", run)
        self.assertLess(hosts_tmp, remove_stale_pin)
        copy_hosts = run.index('cat "$hosts_tmp" > /etc/hosts')
        cleanup_hosts = run.index('rm -f "$hosts_tmp"')
        self.assertLess(remove_stale_pin, copy_hosts)
        self.assertLess(copy_hosts, cleanup_hosts)
        pin = run.index("printf '%s agent-testdb\\n' \"$service_ip\" >> /etc/hosts")
        verify_pin = run.index('getent ahostsv4 agent-testdb | awk \'NR == 1 { print $1; exit }\'')
        forward = run.index("TCP:agent-testdb:5432")
        ready = run.index("pg_isready -q -h 127.0.0.1")
        self.assertIn("for attempt in $(seq 1 30)", run[:resolve])
        self.assertLess(remove_stale_pin, resolve)
        self.assertLess(resolve, pin)
        self.assertLess(pin, verify_pin)
        mismatch_check = run.index('if [ "$pinned_ip" != "$service_ip" ]; then')
        mismatch_exit = run.index("exit 1", mismatch_check)
        self.assertLess(verify_pin, mismatch_check)
        self.assertLess(mismatch_exit, forward)
        self.assertLess(verify_pin, forward)
        self.assertLess(forward, ready)
        self.assertIn("sleep 1", run)
        fail_closed = run.index('if [ -z "$service_ip" ]; then')
        self.assertLess(fail_closed, pin)
        self.assertLess(run.index("exit 1", fail_closed), pin)
        self.assertIn("[[ \"$service_ip\" =~ ^([0-9]{1,3}\\.){3}[0-9]{1,3}$ ]]", run)
        # A failed lookup must empty the result, not kill the step under pipefail.
        self.assertIn('service_ip="$(getent ahostsv4 agent-testdb 2>/dev/null | awk \'NR == 1 { print $1; exit }\')" || service_ip=""', run)
        self.assertIn('echo "agent-testdb did not resolve to an IPv4 address"', run)

    def test_nextest_postgres_wal_budget_is_bounded(self):
        # Same budget as the check.yml lanes: the reusable lane shares the
        # 1 GiB tmpfs service, so it caps WAL recycling in prepare and pins
        # the cap in a verify step instead of running unbounded.
        job = self.workflows["reusable-rust-nextest.yml"]["jobs"]["nextest"]
        prepare = next(
            step for step in job["steps"]
            if step.get("name") == "Prepare isolated test databases"
        )
        verify = next(
            step for step in job["steps"]
            if step.get("name") == "Verify disposable Postgres defaults"
        )
        self.assertEqual(verify.get("shell"), "bash")
        self.assertIn("ALTER SYSTEM SET max_wal_size = '512MB'", prepare["run"])
        self.assertIn("ALTER SYSTEM SET min_wal_size = '64MB'", prepare["run"])
        self.assertIn("pg_reload_conf()", prepare["run"])
        self.assert_wal_reload_ordering_is_pinned(prepare["run"])
        self.assertIn("'max_wal_size': '512MB'", verify["run"])
        self.assertIn("'min_wal_size': '64MB'", verify["run"])

    def test_required_nextest_quarantine_ejects_without_revert(self):
        # Red-group ejection path: the `quarantine` input must exist with an
        # empty default, and the nextest step must turn each filter into a
        # `--skip` so the rest of the suite still gates. Pinned
        # cargo-nextest 0.9.99 only accepts `--skip` AFTER the `--`
        # separator, so the pin asserts the exact argv order: a `--skip`
        # before `--` fails the whole lane with a usage error.
        workflow = self.workflows["reusable-rust-nextest.yml"]
        quarantine = workflow["on"]["workflow_call"]["inputs"]["quarantine"]
        self.assertEqual(quarantine.get("default"), "")
        job = workflow["jobs"]["nextest"]
        run_nextest = next(step for step in job.get("steps", [])
                           if str(step.get("name", "")).startswith("cargo nextest"))
        self.assertEqual(run_nextest["env"]["QUARANTINE"], "${{ inputs.quarantine }}")
        run_text = str(run_nextest.get("run", ""))
        self.assertIn("--skip", run_text)
        self.assertIn("$QUARANTINE", run_text)
        self.assertIn('-- "${skip_args[@]}"', run_text)
        self.assertNotIn("--profile ci $skip_args", run_text)
        # Behavioral pin: substrings alone cannot pin branch selection (an
        # inverted empty/non-empty guard, or a flag dropped from one branch,
        # keeps every substring above while silently disabling ejection).
        # Run the extracted step under bash with a function-shimmed fake
        # `cargo` that records its argv and emits the JUnit file the rest of
        # the step copies, then pin the exact argv for both quarantine
        # states. (A function shim, not a PATH shim, so the pin also runs
        # in harnesses that sanitize PATH on exec.)
        import os
        import subprocess
        import tempfile

        base_argv = [
            "nextest", "run", "--workspace", "--lib", "--bins",
            "--features", "two-bot-core/db", "--locked", "--profile", "ci",
        ]
        cases = (
            ("empty-quarantine", "", base_argv, 0, 0),
            (
                "two-quarantine-filters",
                "red_group other_red",
                base_argv + ["--", "--skip", "red_group", "--skip", "other_red"],
                0,
                0,
            ),
            # A real nextest failure must remain the step result after the
            # JUnit report is copied for the always-run ledger/upload steps.
            ("nextest-failure", "", base_argv, 37, 37),
        )
        fake_cargo = (
            "cargo() {\n"
            "  printf '%s\\n' \"$@\" > \"$FAKE_CARGO_ARGV\"\n"
            "  mkdir -p target/nextest/ci\n"
            "  printf '<testsuites/>\\n' > target/nextest/ci/junit.xml\n"
            "  return \"${FAKE_CARGO_STATUS:-0}\"\n"
            "}\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            argv_path = tmp_path / "cargo-argv.txt"
            for case_name, quarantine_value, expected_argv, cargo_status, expected_status in cases:
                with self.subTest(case=case_name):
                    work_dir = tmp_path / ("work-" + case_name)
                    work_dir.mkdir()
                    run_env = dict(os.environ)
                    run_env.update(
                        {
                            "QUARANTINE": quarantine_value,
                            "FAKE_CARGO_ARGV": str(argv_path),
                            "FAKE_CARGO_STATUS": str(cargo_status),
                        }
                    )
                    proc = subprocess.run(
                        ["bash", "-eo", "pipefail", "-c", fake_cargo + run_text],
                        cwd=work_dir,
                        env=run_env,
                        capture_output=True,
                        text=True,
                        timeout=30,
                    )
                    self.assertEqual(
                        proc.returncode, expected_status,
                        f"unexpected step status for {case_name}: {proc.stderr}",
                    )
                    recorded = argv_path.read_text(encoding="utf-8").splitlines()
                    self.assertEqual(recorded, expected_argv)
                    report = work_dir / "junit.xml"
                    self.assertTrue(report.is_file(), f"nextest report was not copied for {case_name}")
                    self.assertEqual(report.read_text(encoding="utf-8"), "<testsuites/>\n")

    def test_ci_speed_rollout_documents_pr_local_cache(self):
        rollout = (ROOT / "docs/ci-speed-rollout.md").read_text(encoding="utf-8")
        # Markdown wraps mid-phrase, so match on whitespace-normalized text.
        flat = " ".join(rollout.split())
        self.assertIn("local disk", flat)
        self.assertNotIn("adds no cache footprint", flat)

    def test_minimal_grants_and_no_persisted_checkout_credentials(self):
        for name, workflow in self.workflows.items():
            self.assertEqual(workflow["permissions"], {})
            for job_id, job in workflow["jobs"].items():
                with self.subTest(workflow=name, job=job_id):
                    expected = {"contents": "read"}
                    if (name, job_id) in (("synthetic-user-flow.yml", "staging"),
                                          ("synthetic-user-flow.yml", "prod-passive")):
                        # Read workflow/job history to debounce alerts and detect
                        # recovery; open, comment on and close the incident issue.
                        expected = {"contents": "read", "actions": "read", "issues": "write"}
                    elif (name, job_id) == ("supply-chain.yml", "pr-lint"):
                        expected["pull-requests"] = "read"
                    elif (name, job_id) == ("test-guard.yml", "test-guard"):
                        # Marker-label actor audit via the issue-events API.
                        expected["pull-requests"] = "read"
                    elif (name, job_id) in (("staging-migrate.yml", "apply"),
                                                 ("production-migrate.yml", "apply")):
                        # Read-only fetch of the producing plan
                        # run's manifest artifact for the provenance gate.
                        expected = {"contents": "read", "actions": "read"}
                    elif (name, job_id) == ("deploy-staging.yml", "deploy"):
                        # Derives the deploy ID from today's staging
                        # Deployments, then records this one.
                        expected = {"contents": "read", "deployments": "write"}
                    elif (name, job_id) == ("deploy-staging.yml", "certify-promotion"):
                        # Reads the producing run and its immutable diagnostics artifact;
                        # certification itself has no environment or deploy credentials.
                        expected = {"contents": "read", "actions": "read"}
                        self.assertNotIn("environment", job)
                        self.assertNotIn("secrets", job)
                    elif (name, job_id) == ("deploy-production.yml", "production"):
                        # Reads immutable staging artifacts, then records this
                        # independent production Deployment event.
                        expected = {"contents": "read", "actions": "read", "deployments": "write"}
                    elif (name, job_id) == ("deploy-production.yml", "guard"):
                        expected = {"contents": "read", "actions": "read", "checks": "read"}
                    elif (name, job_id) == ("deploy-production.yml", "recheck"):
                        # Reads workflow runs to skip while an operator
                        # dispatch is active; the compare read needs contents.
                        expected = {"contents": "read", "actions": "read"}
                    elif (name, job_id) == ("deploy-production.yml", "staging-soak-evidence"):
                        # Reads deploy-staging runs to tell a promoted newer
                        # build (skip green) from a stalled promotion (refuse).
                        expected = {"contents": "read", "actions": "read"}
                    elif (name, job_id) == ("daily-digest.yml", "digest"):
                        # Cuts the vYYYY.MM.DD tag and its release, reads the
                        # production Deployments fallback, and dispatches the
                        # SBOM chain for the fresh digest tag.
                        expected = {"contents": "write", "deployments": "read",
                                    "actions": "write"}
                    elif (name, job_id) == ("staging-rollback-drill.yml", "proof"):
                        expected = {"contents": "read", "actions": "write"}
                    elif (name, job_id) == ("daily-digest.yml", "rollback-proof-watch"):
                        # Reads the daily proof run; opens or closes its incident issue.
                        expected = {"contents": "read", "actions": "read", "issues": "write"}
                    elif (name, job_id) in (("daily-digest.yml", "alert"),
                                            ("staging-rollback-drill.yml", "alert")):
                        # Opens or comments on the check's incident issue.
                        expected = {"contents": "read", "issues": "write"}
                    elif (name, job_id) == ("release.yml", "attach-sbom"):
                        # TOG-10893: uploads verified SBOMs to the published tag.
                        expected = {"contents": "write"}
                    elif (name, job_id) == ("pr-size.yml", "pr-size"):
                        # Advisory labeller: writes the size/* label and keeps
                        # the one advisory comment on the caller's PR.
                        expected = {"contents": "read", "issues": "write",
                                    "pull-requests": "write"}
                    elif (name, job_id) == ("pr-size.yml", "pr-size-weekly"):
                        # Advisory weekly tally: read-only count of merged PRs
                        # by size label.
                        expected = {"contents": "read", "issues": "read",
                                    "pull-requests": "read"}
                    elif (name, job_id) in (("slow-burn-tracker.yml", "tracker"),
                                            ("slow-burn-tracker.yml", "fixture")):
                        # Slow-burn card filer: files, refreshes and resolves
                        # the one tracking issue per firing key (fixture
                        # proofs under the test label).
                        expected = {"contents": "read", "issues": "write"}
                    elif (name, job_id) == ("dora-metrics.yml", "metrics"):
                        # Weekly DORA file: commits the JSON and summary to
                        # the automation branch, reads the production
                        # Deployments history and verifies each recorded
                        # deploy's Actions run.
                        expected = {"contents": "write", "deployments": "read",
                                    "actions": "read"}
                    self.assertEqual(job["permissions"], expected)
                    for step in job.get("steps", []):
                        if step.get("uses", "").startswith("actions/checkout@"):
                            self.assertEqual(step["with"]["persist-credentials"], "false")

    def test_every_job_declares_a_timeout(self):
        # Direct jobs state a ceiling instead of burning the six-hour default.
        # Reusable callers cannot declare timeout-minutes: execution ceilings
        # belong to the callee jobs. Local callees are also in self.workflows;
        # pinned organization callees own their ceilings in their source repo.
        # Numeric ceilings stay at or below 360; expressions (the push-dependent
        # lanes) only need to be present and non-blank.
        for name, workflow in self.workflows.items():
            for job_id, job in workflow["jobs"].items():
                with self.subTest(workflow=name, job=job_id):
                    if "uses" in job:
                        self.assertNotIn("timeout-minutes", job)
                        continue
                    self.assertIn("timeout-minutes", job)
                    ceiling = job["timeout-minutes"]
                    self.assertIsNotNone(ceiling)
                    self.assertTrue(str(ceiling).strip())
                    if re.fullmatch(r"\d+", str(ceiling)):
                        self.assertGreaterEqual(int(ceiling), 1)
                        self.assertLessEqual(int(ceiling), 360)

    def test_own_trigger_workflows_serialize_at_the_top_level(self):
        # A workflow with its own triggers (anything beyond a pure
        # `workflow_call` callee) declares top-level `concurrency`, so two
        # dispatches or pushes cannot race each other on the shared runners.
        # Pure callees are serialized by their callers instead; the two
        # deploy-side workflows below serialize at the lane jobs on purpose
        # (a top-level group would queue work that must stay unqueued).
        exempt = {
            # Pure `workflow_call` callees: the caller holds the group.
            "sbom.yml",
            "reusable-rust-nextest.yml",
            # Serializes at the `production` lane job only, so a sleeping
            # promotion can never queue an operator rollback behind it (see
            # the header comment in the workflow).
            "deploy-production.yml",
        }
        for name, workflow in self.workflows.items():
            on = workflow.get("on") or {}
            triggers = {on} if isinstance(on, str) else set(on)
            if name in ("sbom.yml", "reusable-rust-nextest.yml"):
                # Gaining an own trigger forces a concurrency decision here.
                self.assertEqual(triggers, {"workflow_call"}, name)
            if name == "deploy-production.yml":
                self.assertEqual(
                    workflow["jobs"]["production"].get("concurrency", {}).get("group"),
                    "deploy-production", f"{name}:production")
            if name in exempt:
                continue
            with self.subTest(workflow=name):
                concurrency = workflow.get("concurrency")
                self.assertIsInstance(concurrency, dict)
                group = concurrency.get("group")
                self.assertIsNotNone(group)
                self.assertTrue(str(group).strip())


if __name__ == "__main__":
    unittest.main()
