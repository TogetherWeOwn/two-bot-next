"""Acceptance tests: assistant monthly-cap slice touches no transport,
credential, handler or endpoint-call code (path allowlist).

Protected test file: the build card implements against these tests and
must not edit them. If a pinned name below cannot be implemented as
written, stop and escalate instead of changing the tests.

The slice under test adds only a persistent per-guild monthly-cap
ledger (migration + store) and a disabled-unless-configured before-send
gate in the existing pure assistant modules. A new table also needs its
deployment classification in the same slice: a cutover-chain mirror
migration (plus its checksum-lock entry) so deployed environments
create the table, a backup dump classification and a rollback table
spec so backup/rollback tooling never meets an unclassified table,
plus the backup format-version bump and the matching rollback-probe
version admission so pre-table archives stay readable.
Those classification paths are admitted below as exact files (and the
cutover migrations directory); every other cutover/backup path stays
forbidden by omission. The slice must not introduce an endpoint call,
an HTTP/transport change, a credential binding or a command handler
anywhere in its diff. This file pins that negatively:

* Every changed path must sit inside the allowlist below (store
  migration/store code, the two pure assistant core modules, tests,
  docs, the migration checksum lock, schema-tripwire SQL, the exact
  backup/rollback classification files, cutover migrations, and the CI
  workflow steps that wire the new tests).
* Changed paths under transport/credential/handler/endpoint-call areas
  are violations even before content is considered.
* Added lines in non-test source must not contain transport or
  credential tokens (HTTP clients, sockets, auth headers, provider
  call paths, credential env names) or endpoint-call sites.

The checker functions are unit-tested against synthetic diffs so the
allowlist itself cannot silently rot. The repo-state tests fail until
the build card lands the pinned migration, store type and gate. The
live-diff assertion is one-shot: it judges only the diff that first
lands this slice and retires once this checker already exists at the
merge base, so later unrelated work is never judged against this
slice's allowlist. The synthetic checker and repo-state tests stay
evergreen.
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
    # Cutover mirror migration only; cutover sources stay out except the
    # exact rollback classification file below.
    "crates/cutover/migrations/",
    "docs/",
    "scripts/ci/",
    "sql/",
)

ALLOWED_FILES = frozenset(
    {
        "crates/core/src/voice_assistant.rs",
        "crates/core/src/voice_assistant_cap.rs",
        "crates/core/src/lib.rs",
        # Backup/rollback classification of the new table (exact files;
        # added lines there still face the content scan below).
        "crates/core/src/backup/dump_file.rs",
        "crates/cutover/src/rollback_delta.rs",
        # The backup format-version bump for the new table must also
        # teach exactly this probe (and its test) the new version;
        # added lines there still face the content scan below.
        "scripts/rollback_readiness_probe.py",
        "scripts/test_rollback_readiness_probe.py",
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
# SQL/CI additions may not. Only real test locations are exempt: a
# `tests` directory outside any production `src` tree, or an explicit
# test script under scripts/. A path under a `src` directory is always
# production — including files named test_*.rs / *_test.rs and anything
# under a test-named directory nested inside src/ — because it is
# wireable as a normal module.
_TEST_SCRIPT = re.compile(r"scripts/(?:ci/)?test_[^/]+\.py")

CHECKER_PATH = "scripts/ci/test_assistant_cap_diff_allowlist.py"

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
    # Endpoint-call sites that repeat no transport token on the added
    # line (a client already in scope): an HTTP-method call addressed
    # at an endpoint/url, a method call on a *client* value, or a
    # no-argument `.send().await` request dispatch (channel sends take
    # an argument, so they do not match).
    re.compile(
        r"\.\s*(?:get|post|put|patch|delete|head|request)\s*\(\s*[^)]*\b(?:endpoint|url|uri)\b",
        re.IGNORECASE,
    ),
    re.compile(
        r"\b\w*client\w*\s*\.\s*(?:get|post|put|patch|delete|head|request)\s*\(",
        re.IGNORECASE,
    ),
    re.compile(r"\.\s*send\s*\(\s*\)\s*\.\s*await\b"),
)


def path_violation(path):
    """Return a reason string when `path` is outside the slice, else None."""
    if path in FORBIDDEN_FILES or path.startswith(FORBIDDEN_PREFIXES):
        return "forbidden surface (transport/credential/handler/endpoint-call)"
    if path in ALLOWED_FILES or path.startswith(ALLOWED_PREFIXES):
        return None
    return "outside the monthly-cap slice allowlist"


def is_test_path(path):
    directories = path.split("/")[:-1]
    if "src" in directories:
        return False
    if "tests" in directories:
        return True
    return bool(_TEST_SCRIPT.fullmatch(path))


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


def live_diff_violations(changed_paths, added_lines_by_path, checker_at_base):
    """One-shot live-diff gate for the diff that first lands this slice.

    Once this checker already exists at the merge base the slice has
    landed, and later diffs must not be judged against its allowlist.
    Until then the live diff is checked in full.
    """
    if checker_at_base:
        return []
    return diff_violations(changed_paths, added_lines_by_path)


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
    # Backup/rollback classification + cutover mirror of the new table.
    "crates/core/src/backup/dump_file.rs",
    "crates/core/tests/fixtures/backup_schema_seed.sql",
    "crates/core/tests/fixtures/pre-reservation-v4.ndjson",
    "crates/cutover/migrations/0444_assistant_monthly_builds.sql",
    "crates/cutover/src/rollback_delta.rs",
    "docs/database-roles.md",
    "sql/database_role_matrix.sql",
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

    def test_backup_rollback_classification_paths_pass(self):
        added = {
            "crates/core/src/backup/dump_file.rs": ['    "assistant_monthly_builds",'],
            "crates/cutover/src/rollback_delta.rs": [
                '    TableSpec { table: "assistant_monthly_builds", measure: TableMeasure::Columns(&["updated_at"]) },'
            ],
            "crates/cutover/migrations/0444_assistant_monthly_builds.sql": [
                "CREATE TABLE IF NOT EXISTS assistant_monthly_builds ("
            ],
        }
        self.assertEqual(diff_violations(list(added), added), [])

    def test_rollback_probe_version_paths_pass_but_stay_scanned(self):
        # The backup format-version bump that waives the new table for
        # old archives must also teach the rollback readiness probe the
        # new version; admit exactly the probe and its test.
        added = {
            "scripts/rollback_readiness_probe.py": ["DUMP_VERSIONS = (3, 4, 5, 6, 7, 8, 9)"],
            "scripts/test_rollback_readiness_probe.py": ["    def test_dump_version_9_manifest_passes(self):"],
        }
        self.assertEqual(diff_violations(list(added), added), [])
        transport = {"scripts/rollback_readiness_probe.py": ["import reqwest  # transport"]}
        self.assertTrue(diff_violations(["scripts/rollback_readiness_probe.py"], transport))
        for path in ("scripts/rollback_other.py", "scripts/check-migrations.py"):
            with self.subTest(path=path):
                self.assertTrue(diff_violations([path], {}))

    def test_other_cutover_and_backup_paths_fail_closed(self):
        for path in (
            "crates/cutover/src/lib.rs",
            "crates/cutover/src/legacy_copy/options.rs",
            "crates/cutover/tests/legacy_verify_db.rs",
            "crates/core/src/backup/mod.rs",
            "crates/core/src/backup/restore.rs",
        ):
            with self.subTest(path=path):
                self.assertTrue(diff_violations([path], {}))

    def test_transport_content_in_classification_files_fails(self):
        for path in (
            "crates/core/src/backup/dump_file.rs",
            "crates/cutover/src/rollback_delta.rs",
            "crates/cutover/migrations/0444_assistant_monthly_builds.sql",
        ):
            with self.subTest(path=path):
                added = {path: ["let client = reqwest::Client::new();"]}
                self.assertTrue(diff_violations([path], added))

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

    def test_production_src_paths_named_like_tests_are_scanned(self):
        # A production file under src/ is wireable as a normal module
        # whatever its name; test-shaped names must not skip the scan.
        for path in (
            "crates/store/src/test_proxy.rs",
            "crates/store/src/transport_test.rs",
            "crates/store/src/test_helpers/proxy.rs",
            "crates/store/src/tests/proxy.rs",
            "crates/store/src/test/proxy.rs",
        ):
            with self.subTest(path=path):
                self.assertFalse(is_test_path(path))
                added = {path: ["let client = reqwest::Client::new();"]}
                self.assertTrue(diff_violations([path], added))

    def test_real_test_locations_stay_exempt_from_content_scan(self):
        for path in (
            "crates/store/tests/test_proxy.rs",
            "crates/store/tests/transport_test.rs",
            "crates/core/tests/fixtures/test_proxy.rs",
            "scripts/ci/test_assistant_cap_diff_allowlist.py",
        ):
            with self.subTest(path=path):
                self.assertTrue(is_test_path(path))
                added = {path: ["// fixture naming reqwest"]}
                self.assertEqual(content_violations(path, added[path]), [])

    def test_endpoint_call_without_transport_token_fails(self):
        for line in (
            "let resp = client.post(endpoint).send().await?;",
            "let resp = http.post(url).send().await?;",
            "let resp = builder.send().await?;",
        ):
            with self.subTest(line=line):
                added = {"crates/store/src/assistant_cap.rs": [line]}
                self.assertTrue(diff_violations(["crates/store/src/assistant_cap.rs"], added))

    def test_live_diff_retires_once_checker_exists_at_merge_base(self):
        # Simulated post-merge state, offline: the checker is already on
        # the base, so an unrelated later diff is no longer judged
        # against this one slice's allowlist.
        changed = ["crates/bot/src/lib.rs"]
        added = {"crates/bot/src/lib.rs": ["pub fn unrelated() {}"]}
        self.assertEqual(live_diff_violations(changed, added, checker_at_base=True), [])
        # The current slice remains checked while the checker is not at
        # the base yet: a forbidden surface still fails, and a clean
        # builder-shaped slice still passes.
        self.assertTrue(live_diff_violations(changed, added, checker_at_base=False))
        clean_added = {
            "crates/store/src/assistant_cap.rs": [
                "INSERT INTO assistant_monthly_builds (guild_id, month_key, build_count)"
            ]
        }
        self.assertEqual(
            live_diff_violations(BUILDER_SHAPED_PATHS, clean_added, checker_at_base=False),
            [],
        )
        transport_added = {"crates/store/src/assistant_cap.rs": ["let c = reqwest::Client::new();"]}
        self.assertTrue(
            live_diff_violations(
                ["crates/store/src/assistant_cap.rs"], transport_added, checker_at_base=False
            )
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

    def test_cutover_migration_mirrors_assistant_monthly_builds(self):
        migrations = sorted((REPO_ROOT / "crates/cutover/migrations").glob("*.sql"))
        creating = [
            m.name
            for m in migrations
            if re.search(
                r"CREATE TABLE[^;]*assistant_monthly_builds",
                m.read_text(encoding="utf-8", errors="replace"),
                re.IGNORECASE | re.DOTALL,
            )
        ]
        self.assertTrue(
            creating,
            "no cutover migration creates assistant_monthly_builds yet "
            "(store-only tables are never deployed)",
        )

    def test_backup_dump_classifies_assistant_monthly_builds(self):
        body = _read("crates/core/src/backup/dump_file.rs")

        def table_list(const_name):
            start = body.find(f"pub const {const_name}")
            self.assertGreaterEqual(start, 0, f"{const_name} not found in dump_file.rs")
            end = body.find("pub const ", start + 1)
            return body[start:] if end < 0 else body[start:end]

        # Dumped, not excluded: an excluded ledger would hand back a
        # fresh monthly quota after every restore.
        self.assertIn('"assistant_monthly_builds"', table_list("DUMP_TABLES"))
        self.assertNotIn('"assistant_monthly_builds"', table_list("EXCLUDED_TABLES"))

    def test_rollback_delta_classifies_assistant_monthly_builds(self):
        body = _read("crates/cutover/src/rollback_delta.rs")
        self.assertRegex(
            body,
            r"TableSpec\s*\{[^}]*\"assistant_monthly_builds\"",
            "no rollback TableSpec classifies assistant_monthly_builds yet",
        )


class LiveDiffTest(unittest.TestCase):
    """The real diff that first lands this slice stays inside the allowlist.

    One-shot: once this checker exists at the merge base, the live
    assertion retires (see live_diff_violations); the synthetic and
    repo-state suites above remain evergreen.
    """

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
        base_sha = base.stdout.strip()
        # Fail closed: only a confirmed presence at the base retires
        # the gate; any probe failure keeps the slice checked.
        at_base = subprocess.run(
            ["git", "cat-file", "-e", f"{base_sha}:{CHECKER_PATH}"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=30,
        )
        checker_at_base = at_base.returncode == 0
        names = subprocess.run(
            ["git", "diff", "--name-only", f"{base_sha}...HEAD"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(names.returncode, 0, names.stderr)
        changed = [p for p in names.stdout.splitlines() if p.strip()]
        diff = subprocess.run(
            ["git", "diff", "-U0", f"{base_sha}...HEAD"],
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
        self.assertEqual(live_diff_violations(changed, added, checker_at_base), [])


if __name__ == "__main__":
    unittest.main()
