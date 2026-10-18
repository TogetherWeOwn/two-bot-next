"""Acceptance tests: assistant monthly-cap slice touches no transport,
credential, handler or endpoint-call code (path allowlist).

Protected test file: the build card implements against these tests and
must not edit them. If a pinned name below cannot be implemented as
written, stop and escalate instead of changing the tests.

The slice under test adds only a persistent per-guild monthly-cap
ledger (migration + store) and a disabled-unless-configured before-send
gate in the existing pure assistant modules. It must not introduce an
endpoint call, an HTTP/transport change, a credential binding or a
command handler anywhere in its diff. This file pins that negatively:

* Every changed path must sit inside the allowlist below (store
  migration/store code, the two pure assistant core modules, tests,
  docs, the migration checksum lock, schema-tripwire SQL, and the CI
  workflow steps that wire the new tests).
* Changed paths under transport/credential/handler/endpoint-call areas
  are violations even before content is considered.
* Added lines in non-test source must not contain transport or
  credential tokens (HTTP clients, sockets, auth headers, provider
  call paths, credential env names).

The checker functions are unit-tested against synthetic diffs so the
allowlist itself cannot silently rot. The repo-state tests fail until
the build card lands the pinned migration, store type and gate.
"""

import re
import subprocess
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]

ALLOWED_PREFIXES = (
    "crates/store/migrations/",
    "crates/store/src/",
    "crates/store/tests/",
    "crates/core/tests/",
    "docs/",
    "scripts/ci/",
    "sql/",
)

ALLOWED_FILES = frozenset(
    {
        "crates/core/src/voice_assistant.rs",
        "crates/core/src/voice_assistant_cap.rs",
        "crates/core/src/lib.rs",
        "migrations.lock",
        ".github/workflows/check.yml",
    }
)

# Transport, credential, handler and endpoint-call surfaces. A change
# here is a violation regardless of the allowlist.
FORBIDDEN_PREFIXES = (
    "crates/discord/",
    "crates/bot/",
    "crates/core/src/router/",
    "src/",
    "wrangler/",
)

FORBIDDEN_FILES = frozenset(
    {
        # Endpoint-call / request-construction / output-validation path.
        "crates/core/src/voice_assistant_request.rs",
        "crates/core/src/voice_assistant_build.rs",
        "crates/core/src/voice_assistant_validate.rs",
        # Handlers and routing.
        "crates/core/src/handlers.rs",
        "crates/core/src/router.rs",
        # Credential and MAC material.
        "crates/core/src/secret.rs",
        "crates/core/src/mac.rs",
        # Existing HTTP transport.
        "crates/core/src/feeds_http.rs",
        "crates/core/src/feeds_connector.rs",
        "crates/core/src/eventsub_receiver_config.rs",
    }
)

# Test files may name the forbidden things in fixtures; production and
# SQL/CI additions may not.
_TEST_PATH = re.compile(r"(^|/)(tests?|test_[^/]*)$|(^|/)test_[^/]*\.py$|_test\.rs$|/tests/")

FORBIDDEN_CONTENT = (
    re.compile(r"reqwest", re.IGNORECASE),
    re.compile(r"hyper::", re.IGNORECASE),
    re.compile(r"\bureq\b", re.IGNORECASE),
    re.compile(r"\bcurl\b", re.IGNORECASE),
    re.compile(r"TcpStream|UdpSocket|tokio::net|std::net::"),
    re.compile(r"Authorization", re.IGNORECASE),
    re.compile(r"Bearer\s", re.IGNORECASE),
    re.compile(r"chat/completions", re.IGNORECASE),
    re.compile(r"TWO_ASSISTANT_(API_KEY|TOKEN|SECRET|PASSWORD)", re.IGNORECASE),
    re.compile(r"ASSISTANT_(API_KEY|TOKEN|SECRET)\b", re.IGNORECASE),
)


def path_violation(path):
    """Return a reason string when `path` is outside the slice, else None."""
    if path in FORBIDDEN_FILES or path.startswith(FORBIDDEN_PREFIXES):
        return "forbidden surface (transport/credential/handler/endpoint-call)"
    if path in ALLOWED_FILES or path.startswith(ALLOWED_PREFIXES):
        return None
    return "outside the monthly-cap slice allowlist"


def is_test_path(path):
    return bool(_TEST_PATH.search(path)) or "/tests/" in path or path.startswith("scripts/ci/test_")


def content_violations(path, added_lines):
    """Forbidden transport/credential tokens in non-test added lines."""
    if is_test_path(path):
        return []
    found = []
    for line in added_lines:
        for pattern in FORBIDDEN_CONTENT:
            if pattern.search(line):
                found.append(f"{path}: added line matches /{pattern.pattern}/: {line.strip()[:80]}")
    return found


def diff_violations(changed_paths, added_lines_by_path):
    violations = []
    for path in changed_paths:
        reason = path_violation(path)
        if reason is not None:
            violations.append(f"{path}: {reason}")
        violations.extend(content_violations(path, added_lines_by_path.get(path, ())))
    return violations


BUILDER_SHAPED_PATHS = [
    "crates/store/migrations/0443_assistant_monthly_builds.sql",
    "crates/store/src/assistant_cap.rs",
    "crates/store/src/lib.rs",
    "crates/core/src/voice_assistant.rs",
    "crates/store/tests/assistant_cap_ledger_acceptance.rs",
    "crates/core/tests/assistant_config_gate_acceptance.rs",
    "scripts/ci/test_assistant_cap_diff_allowlist.py",
    "docs/voice-assistant-cap-core.md",
    "migrations.lock",
    ".github/workflows/check.yml",
]


class AllowlistCheckerTest(unittest.TestCase):
    def test_builder_shaped_diff_passes(self):
        added = {
            "crates/store/src/assistant_cap.rs": [
                "pub struct PgAssistantCapLedger { pool: Pool<Postgres> }",
                "INSERT INTO assistant_monthly_builds (guild_id, month_key, build_count)",
            ]
        }
        self.assertEqual(diff_violations(BUILDER_SHAPED_PATHS, added), [])

    def test_handler_paths_fail(self):
        for path in ("crates/core/src/handlers.rs", "crates/core/src/router.rs", "crates/discord/src/lib.rs", "src/main.rs"):
            with self.subTest(path=path):
                self.assertTrue(diff_violations([path], {}))

    def test_credential_paths_fail(self):
        for path in ("crates/core/src/secret.rs", "crates/core/src/mac.rs"):
            with self.subTest(path=path):
                self.assertTrue(diff_violations([path], {}))

    def test_endpoint_call_paths_fail(self):
        for path in (
            "crates/core/src/voice_assistant_request.rs",
            "crates/core/src/voice_assistant_build.rs",
            "crates/core/src/voice_assistant_validate.rs",
            "crates/core/src/feeds_http.rs",
        ):
            with self.subTest(path=path):
                self.assertTrue(diff_violations([path], {}))

    def test_unlisted_path_fails_closed(self):
        self.assertTrue(diff_violations(["crates/core/src/commands.rs"], {}))
        self.assertTrue(diff_violations(["Cargo.toml"], {}))

    def test_transport_content_in_allowed_source_fails(self):
        for line in (
            "let client = reqwest::Client::new();",
            "let stream = tokio::net::TcpStream::connect(addr).await?;",
            'headers.insert("Authorization", value);',
            'let url = format!("{endpoint}/chat/completions");',
            'let key = env("TWO_ASSISTANT_API_KEY");',
        ):
            with self.subTest(line=line):
                added = {"crates/store/src/assistant_cap.rs": [line]}
                self.assertTrue(diff_violations(["crates/store/src/assistant_cap.rs"], added))

    def test_same_tokens_in_test_files_are_fixtures_not_violations(self):
        added = {"crates/store/tests/assistant_cap_ledger_acceptance.rs": ["// never reqwest"]}
        self.assertEqual(
            diff_violations(["crates/store/tests/assistant_cap_ledger_acceptance.rs"], added),
            [],
        )


def _read(relpath):
    return (REPO_ROOT / relpath).read_text(encoding="utf-8", errors="replace")


class RepoStateTest(unittest.TestCase):
    """Fail on today's tree; pass once the build card lands its slice."""

    def test_migration_creates_assistant_monthly_builds(self):
        migrations = sorted((REPO_ROOT / "crates/store/migrations").glob("*.sql"))
        bodies = [m.read_text(encoding="utf-8", errors="replace") for m in migrations]
        creating = [
            m.name
            for m, body in zip(migrations, bodies)
            if re.search(r"CREATE TABLE[^;]*assistant_monthly_builds", body, re.IGNORECASE | re.DOTALL)
        ]
        self.assertTrue(
            creating,
            "no store migration creates assistant_monthly_builds yet",
        )
        body = next(b for m, b in zip(migrations, bodies) if m.name == creating[0])
        for column in ("guild_id", "month_key", "build_count"):
            self.assertIn(column, body)

    def test_store_type_exists(self):
        sources = list((REPO_ROOT / "crates/store/src").rglob("*.rs"))
        found = [
            str(p.relative_to(REPO_ROOT))
            for p in sources
            if "PgAssistantCapLedger" in p.read_text(encoding="utf-8", errors="replace")
        ]
        self.assertTrue(found, "PgAssistantCapLedger is not implemented in crates/store/src yet")

    def test_gate_refusal_exists_in_pure_assistant_module(self):
        body = _read("crates/core/src/voice_assistant.rs")
        self.assertIn("AssistantGateRefusal", body)
        self.assertIn("require_send_allowed", body)
        self.assertIn("assistant_not_configured", body)
        self.assertIn("monthly_cap_reached", body)


class LiveDiffTest(unittest.TestCase):
    """The real diff against the base branch must stay inside the allowlist."""

    def test_current_diff_within_allowlist(self):
        try:
            base = subprocess.run(
                ["git", "merge-base", "origin/main", "HEAD"],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                timeout=30,
            )
        except (OSError, subprocess.TimeoutExpired):
            self.skipTest("git merge-base unavailable")
        if base.returncode != 0 or not base.stdout.strip():
            self.skipTest("no origin/main merge base in this checkout")
        names = subprocess.run(
            ["git", "diff", "--name-only", f"{base.stdout.strip()}...HEAD"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(names.returncode, 0, names.stderr)
        changed = [p for p in names.stdout.splitlines() if p.strip()]
        diff = subprocess.run(
            ["git", "diff", "-U0", f"{base.stdout.strip()}...HEAD"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(diff.returncode, 0, diff.stderr)
        added = {}
        current = None
        for line in diff.stdout.splitlines():
            if line.startswith("+++ b/"):
                current = line[len("+++ b/") :].split("\t")[0]
                added.setdefault(current, [])
            elif current is not None and line.startswith("+") and not line.startswith("+++"):
                added[current].append(line[1:])
        self.assertEqual(diff_violations(changed, added), [])


if __name__ == "__main__":
    unittest.main()
