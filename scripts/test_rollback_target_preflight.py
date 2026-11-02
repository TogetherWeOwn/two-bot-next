"""Offline pins for scripts/rollback_target_preflight.py.

No Cloudflare, registry or GitHub access: the control plane is faked in
memory, registry commands are scripted, and migrations live in a throwaway
local Git repository.
"""

import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parent))
# E402-safe by convention: every scripts/ test adds its own directory first.
import staging_rollout as base
import rollback_target_preflight as preflight
from test_worker_container_rollback import FakeClient, rollout_row, version_route

ACCOUNT = "a" * 32
APPLICATION = preflight.APPLICATION
WORKER = preflight.WORKER
APP_ID = "app-1"
VERSION = "11111111-1111-4111-8111-111111111111"
IMAGE = f"registry.cloudflare.com/{ACCOUNT}/{APPLICATION}@sha256:" + "a" * 64
SERVING_SHA = "0123456789abcdef0123456789abcdef01234567"


def snapshot(**overrides):
    state = {"has_previous": True, "worker": WORKER, "application": APPLICATION,
             "application_id": APP_ID, "namespace_id": "ns-1", "serving_version": VERSION,
             "serving_sha": SERVING_SHA, "provenance_complete": True,
             "serving_rollout_id": "r-prev", "serving_image": IMAGE, "recorded_at": 1.0}
    state.update(overrides)
    return state


def live_routes(image=IMAGE, message=SERVING_SHA, served=(VERSION,)):
    deployments = [{"versions": [{"version_id": v, "percentage": 100}]} for v in served]
    return {
        ("GET", f"/containers/applications/{APP_ID}/rollouts/r-prev"):
            lambda: {"success": True, "result": rollout_row("r-prev", image)},
        ("GET", f"/workers/scripts/{WORKER}/versions/{VERSION}"): version_route(VERSION, message),
        ("GET", f"/workers/scripts/{WORKER}/deployments"):
            lambda: {"success": True, "result": {"deployments": deployments}},
        ("GET", "/containers/applications"):
            lambda: {"success": True, "result": [{"id": APP_ID, "name": APPLICATION,
                                                  "durable_objects": {"namespace_id": "ns-1"}}]},
        ("GET", f"/containers/applications/{APP_ID}/rollouts?limit={base.LIMIT}"):
            lambda: {"success": True, "result": [rollout_row("r-prev", image)]},
    }


def registry_runner(manifest_ok=True, creds=None):
    calls = []

    def run(argv, env=None, input_bytes=None):
        calls.append(argv[:3])
        if argv[:3] == ["npx", "--no-install", "wrangler"]:
            return json.dumps(creds or {"account_id": ACCOUNT, "registry_host": "registry.cloudflare.com",
                                        "username": "user", "password": "secret"}).encode()
        if argv[:2] == ["docker", "login"]:
            assert input_bytes == b"secret" and "secret" not in argv
            return b""
        if argv[:3] == ["docker", "manifest", "inspect"]:
            if not manifest_ok:
                raise base.GateError("registry_command_failed")
            return json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json"}).encode()
        raise AssertionError(argv)
    return run, calls


class GitRepo:
    """A throwaway repository: the serving commit, then the candidate."""

    def __init__(self, tmp):
        self.root = Path(tmp)
        self.root.mkdir(parents=True, exist_ok=True)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "t@example.test")
        self.git("config", "user.name", "t")
        self.write("crates/store/migrations/0001_base.sql", "CREATE TABLE t (id bigint);\n")
        self.serving = self.commit("serving")

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.root), *args], check=True,
                              capture_output=True, text=True).stdout.strip()

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")


class IncompatibleCodeTests(unittest.TestCase):
    def test_backward_incompatible_statements_are_named(self):
        cases = {
            "ALTER TABLE t DROP COLUMN c;": "drop_object",
            "drop table if exists t;": "drop_object",
            "ALTER TABLE t RENAME COLUMN a TO b;": "rename",
            "ALTER TABLE t ALTER COLUMN c TYPE text;": "column_type_change",
            "ALTER TABLE t ALTER COLUMN c SET DATA TYPE text;": "column_type_change",
            "ALTER TABLE t ALTER COLUMN c SET NOT NULL;": "set_not_null",
            "ALTER TABLE t ALTER COLUMN c DROP DEFAULT;": "drop_default",
            "TRUNCATE t;": "truncate",
            "DELETE FROM t;": "delete_all_rows",
            "delete from t;": "delete_all_rows",
            "DELETE\nFROM t;": "delete_all_rows",
            "DELETE FROM ONLY t;": "delete_all_rows",
            "DELETE FROM t RETURNING *;": "delete_all_rows",
            "/* cleanup */ DELETE FROM t;": "delete_all_rows",
            # A WHERE outside the DELETE's own scope does not waive it: an
            # outer query's WHERE or a RETURNING subquery's WHERE still
            # empties the table.
            "WITH removed AS (DELETE FROM members RETURNING id) "
            "SELECT id FROM removed WHERE id > 0;": "delete_all_rows",
            "DELETE FROM t RETURNING (SELECT max(x) FROM u WHERE y = 1);":
                "delete_all_rows",
            "WITH x AS (SELECT * FROM b WHERE y = 1) DELETE FROM t;":
                "delete_all_rows",
            "REVOKE SELECT ON t FROM app;": "revoke",
            "ALTER TABLE t ADD COLUMN c bigint NOT NULL;": "add_not_null_column_without_default",
            # PostgreSQL lets COLUMN be omitted.
            "ALTER TABLE t DROP c;": "drop_object",
            "ALTER TABLE ONLY t DROP IF EXISTS c CASCADE;": "drop_object",
            "ALTER TABLE t ADD c text NOT NULL;": "add_not_null_column_without_default",
            "ALTER TABLE t ALTER c TYPE text;": "column_type_change",
            "ALTER TABLE t ADD COLUMN a int, DROP COLUMN b;": "drop_object",
            # A comment marker or semicolon inside a literal hides nothing.
            "INSERT INTO config VALUES ('--'); ALTER TABLE t DROP COLUMN c;": "drop_object",
            "INSERT INTO config VALUES ('/*'); ALTER TABLE t DROP c; -- */": "drop_object",
            "INSERT INTO config VALUES ('a;b'); TRUNCATE t;": "truncate",
            # Top-level DROP of anything but an index refuses conservatively.
            "DROP TRIGGER t_audit ON t;": "drop_object",
            "DROP POLICY p ON t;": "drop_object",
            # A DO block runs its dollar-quoted body.
            "DO $$ BEGIN ALTER TABLE t DROP COLUMN c; END $$;": "drop_object",
            "DO $$BEGIN TRUNCATE t; END$$;": "truncate",
            # Schema-qualified quoted relations still expose their actions.
            'ALTER TABLE "public"."members" DROP COLUMN legacy_id;': "drop_object",
            'ALTER TABLE "public"."members" ADD COLUMN c bigint NOT NULL;':
                "add_not_null_column_without_default",
            "ALTER TABLE public.members DROP COLUMN legacy_id;": "drop_object",
            # Partly quoted qualifiers: neither part may absorb the dot.
            'ALTER TABLE public."members" DROP COLUMN legacy_id;': "drop_object",
            'ALTER TABLE public."members" ADD COLUMN c bigint NOT NULL;':
                "add_not_null_column_without_default",
            'ALTER TABLE "public".members DROP COLUMN legacy_id;': "drop_object",
            # Backslash escapes in E-strings do not end the literal.
            "INSERT INTO logs VALUES (E'it\\'s'); DROP TABLE members;": "drop_object",
            # An unterminated literal, comment or dollar body refuses
            # fail-closed instead of silently passing the migration.
            "INSERT INTO t VALUES ('oops); DROP TABLE t;": "unparseable_migration",
            "SELECT 1 /* DROP TABLE t;": "unparseable_migration",
            "COMMENT ON TABLE t IS $$it's legacy$$; DROP TABLE t;": "unparseable_migration",
        }
        for sql, code in cases.items():
            with self.subTest(sql=sql):
                self.assertEqual(preflight.incompatible_code(sql), code)

    def test_expand_only_statements_pass(self):
        for sql in (
            "CREATE TABLE IF NOT EXISTS t (id bigint NOT NULL);",
            "ALTER TABLE t ADD COLUMN IF NOT EXISTS c timestamptz;",
            "ALTER TABLE t ADD COLUMN c boolean NOT NULL DEFAULT false;",
            "CREATE INDEX IF NOT EXISTS t_c ON t (c);",
            "DROP INDEX IF EXISTS t_old;",
            "ALTER TABLE t ALTER COLUMN c DROP NOT NULL;",
            "GRANT SELECT ON t TO app;",
            "-- DROP TABLE t; only a comment\nSELECT 1;",
            "/* RENAME later */ INSERT INTO t VALUES ('DROP TABLE x; TRUNCATE y');",
            "ALTER TABLE t ADD CONSTRAINT c_pos CHECK (c > 0) NOT VALID;",
            "ALTER TABLE t ADD PRIMARY KEY (id), DROP CONSTRAINT old_pk;",
            "ALTER TABLE t ALTER COLUMN c SET DEFAULT 0, ADD COLUMN d int NOT NULL DEFAULT 1;",
            'CREATE TABLE "drop" (id bigint);',
            "DROP INDEX CONCURRENTLY IF EXISTS t_old;",
            "DELETE FROM t WHERE id = 1;",
            "delete from t using u where t.id = u.id;",
            "DELETE FROM ONLY t WHERE id = 1 RETURNING *;",
            "DELETE FROM t WHERE id IN (SELECT id FROM u);",
            "WITH x AS (SELECT * FROM b WHERE y = 1) DELETE FROM t WHERE id = 1;",
            # A DELETE trigger event deletes nothing: defining the trigger
            # must not block a promotion.
            "CREATE TRIGGER watch BEFORE DELETE ON members FOR EACH ROW "
            "EXECUTE FUNCTION log_change();",
            # A TRUNCATE trigger event truncates nothing: defining the
            # trigger must not block a promotion.
            "CREATE TRIGGER watch BEFORE TRUNCATE ON members FOR EACH STATEMENT "
            "EXECUTE FUNCTION log_change();",
            "CREATE TRIGGER watch AFTER INSERT OR TRUNCATE ON members FOR EACH ROW "
            "EXECUTE FUNCTION log_change();",
        ):
            with self.subTest(sql=sql):
                self.assertIsNone(preflight.incompatible_code(sql))


class MigrationFindingTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.repo = GitRepo(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def findings(self, candidate, serving=None):
        return preflight.migration_findings(self.repo.root, serving or self.repo.serving, candidate)

    def test_additive_migrations_pass_and_are_listed(self):
        self.repo.write("crates/store/migrations/0002_add.sql", "ALTER TABLE t ADD COLUMN c text;\n")
        self.repo.write("crates/store/src/lib.rs", "// DROP TABLE is not a migration\n")
        checked, refusals = self.findings(self.repo.commit("candidate"))
        self.assertEqual(checked, ["0002_add.sql"])
        self.assertEqual(refusals, [])

    def test_incompatible_migration_refuses_unless_reviewed(self):
        self.repo.write("crates/cutover/migrations/0002_drop.sql", "ALTER TABLE t DROP COLUMN c;\n")
        candidate = self.repo.commit("drop")
        self.assertEqual(self.findings(candidate)[1],
                         [{"migration": "0002_drop.sql", "code": "drop_object"}])
        self.repo.write("crates/cutover/migrations/0002_drop.sql",
                        "-- rollback-compatible: no shipped build ever read column c\n"
                        "ALTER TABLE t DROP COLUMN c;\n")
        self.assertEqual(self.findings(self.repo.commit("reviewed"))[1], [])

    def test_an_empty_override_does_not_count(self):
        self.repo.write("crates/store/migrations/0002_drop.sql",
                        "-- rollback-compatible: ok\nTRUNCATE t;\n")
        self.assertEqual(self.findings(self.repo.commit("short"))[1],
                         [{"migration": "0002_drop.sql", "code": "truncate"}])

    def test_an_empty_marker_cannot_claim_the_next_line(self):
        self.repo.write("crates/store/migrations/0002_drop.sql",
                        "-- rollback-compatible:\nDROP TABLE x;\n")
        self.assertEqual(self.findings(self.repo.commit("newline"))[1],
                         [{"migration": "0002_drop.sql", "code": "drop_object"}])

    def test_an_override_cannot_waive_an_unreadable_migration(self):
        self.repo.write("crates/store/migrations/0002_drop.sql",
                        "-- rollback-compatible: reviewed and this is fine\n"
                        "INSERT INTO t VALUES ('oops); DROP TABLE t;\n")
        self.assertEqual(self.findings(self.repo.commit("unparseable"))[1],
                         [{"migration": "0002_drop.sql", "code": "unparseable_migration"}])

    def test_padded_marker_reason_must_be_ten_characters(self):
        self.repo.write("crates/store/migrations/0002_drop.sql",
                        "-- rollback-compatible: x         \nDROP TABLE x;\n")
        self.assertEqual(self.findings(self.repo.commit("padded"))[1],
                         [{"migration": "0002_drop.sql", "code": "drop_object"}])

    def test_unqualified_delete_refuses_unless_reviewed(self):
        self.repo.write("crates/store/migrations/0002_wipe.sql", "DELETE FROM t;\n")
        self.assertEqual(self.findings(self.repo.commit("wipe"))[1],
                         [{"migration": "0002_wipe.sql", "code": "delete_all_rows"}])
        self.repo.write("crates/store/migrations/0002_wipe.sql",
                        "DELETE FROM t WHERE id = 1;\n")
        checked, refusals = self.findings(self.repo.commit("scoped"))
        self.assertEqual(checked, ["0002_wipe.sql"])
        self.assertEqual(refusals, [])
        self.repo.write("crates/store/migrations/0002_wipe.sql",
                        "-- rollback-compatible: table holds only rebuildable cache rows\n"
                        "DELETE FROM t;\n")
        self.assertEqual(self.findings(self.repo.commit("reviewed"))[1], [])
        self.repo.write("crates/store/migrations/0002_wipe.sql",
                        "-- rollback-compatible: x         \nDELETE FROM t;\n")
        self.assertEqual(self.findings(self.repo.commit("padded"))[1],
                         [{"migration": "0002_wipe.sql", "code": "delete_all_rows"}])

    def test_rewritten_or_deleted_migration_refuses(self):
        self.repo.write("crates/store/migrations/0001_base.sql", "CREATE TABLE t (id int);\n")
        self.assertEqual(self.findings(self.repo.commit("rewrite"))[1],
                         [{"migration": "0001_base.sql", "code": "migration_rewritten"}])
        (self.repo.root / "crates/store/migrations/0001_base.sql").unlink()
        self.assertEqual(self.findings(self.repo.commit("delete"))[1],
                         [{"migration": "0001_base.sql", "code": "migration_rewritten"}])

    def test_serving_sha_must_be_an_ancestor_of_the_candidate(self):
        candidate = self.repo.commit("candidate")
        self.repo.git("checkout", "-q", "--orphan", "other")
        unrelated = self.repo.commit("unrelated")
        with self.assertRaises(base.GateError) as raised:
            self.findings(candidate, serving=unrelated)
        self.assertEqual(str(raised.exception), "rollback_target_not_ancestor")


class RegistryTests(unittest.TestCase):
    def test_present_image_passes_without_echoing_the_password(self):
        run, calls = registry_runner()
        preflight.image_in_registry(IMAGE, ACCOUNT, run=run)
        self.assertEqual(calls[-1], ["docker", "manifest", "inspect"])

    def test_missing_image_refuses(self):
        run, _ = registry_runner(manifest_ok=False)
        with self.assertRaises(base.GateError) as raised:
            preflight.image_in_registry(IMAGE, ACCOUNT, run=run)
        self.assertEqual(str(raised.exception), "rollback_image_missing")

    def test_foreign_credentials_or_account_refuse(self):
        run, _ = registry_runner(creds={"account_id": "b" * 32, "registry_host": "registry.cloudflare.com",
                                        "username": "user", "password": "secret"})
        with self.assertRaises(base.GateError) as raised:
            preflight.image_in_registry(IMAGE, ACCOUNT, run=run)
        self.assertEqual(str(raised.exception), "registry_authentication_refused")
        with self.assertRaises(base.GateError) as raised:
            preflight.image_in_registry(IMAGE, "b" * 32, run=registry_runner()[0])
        self.assertEqual(str(raised.exception), "rollback_image_wrong_account")


class MainTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.dir = Path(self.tmp.name)
        self.repo = GitRepo(self.dir / "repo")
        self.repo.write("crates/store/migrations/0002_add.sql", "ALTER TABLE t ADD COLUMN c text;\n")
        self.candidate = self.repo.commit("candidate")

    def tearDown(self):
        self.tmp.cleanup()

    def run_main(self, state=None, routes=None, registry=None, raw=None):
        snap = self.dir / "snapshot.json"
        snap.write_text(raw if raw is not None else json.dumps(
            snapshot(serving_sha=self.repo.serving, **(state or {}))))
        out = self.dir / "evidence.json"
        message = (state or {}).get("serving_sha", self.repo.serving)
        client = FakeClient(routes or live_routes(message=message))
        stdout = io.StringIO()
        with redirect_stdout(stdout):
            code = preflight.main(
                ["--snapshot", str(snap), "--sha", self.candidate, "--repo", str(self.repo.root),
                 "--out", str(out)],
                environ={"CLOUDFLARE_ACCOUNT_ID": ACCOUNT, "CLOUDFLARE_API_TOKEN": "t"},
                client_factory=lambda account, token: client,
                registry=registry or (lambda image, account: None))
        return code, json.loads(out.read_text()), stdout.getvalue(), client

    def test_valid_target_passes_and_records_the_receipt(self):
        code, evidence, stdout, client = self.run_main()
        self.assertEqual(code, 0, stdout)
        self.assertEqual(evidence, {
            "candidate_sha": self.candidate, "rollback_version": VERSION,
            "rollback_sha": self.repo.serving, "rollback_image": IMAGE,
            "migrations_checked": ["0002_add.sql"], "migration_refusals": [],
            "verdict": "pass", "code": "rollback_target_valid"})
        self.assertFalse([call for call in client.calls if call[0] == "POST"])

    def test_missing_or_incomplete_target_refuses(self):
        cases = {
            "first deploy": ({"has_previous": False}, "no_rollback_target"),
            "unstamped version": ({"provenance_complete": False}, "rollback_target_unstamped"),
            "wrong worker": ({"worker": "two-bot-next-staging"}, "snapshot_identity_mismatch"),
            "tag image": ({"serving_image": IMAGE.split("@")[0] + ":latest"}, "rollback_target_incomplete"),
            "bad version": ({"serving_version": "not-a-uuid"}, "rollback_target_incomplete"),
        }
        for name, (state, code_name) in cases.items():
            with self.subTest(name):
                code, evidence, stdout, _ = self.run_main(state=state)
                self.assertEqual(code, 1)
                self.assertEqual(evidence["code"], code_name)
                self.assertIn("::error::Refusing promotion: no valid rollback target", stdout)

    def test_unreadable_snapshot_refuses(self):
        for raw in ("", "[]", '{"has_previous": true, "has_previous": true}'):
            with self.subTest(raw=raw):
                code, evidence, _, _ = self.run_main(raw=raw)
                self.assertEqual((code, evidence["code"]), (1, "snapshot_unreadable"))

    def test_live_state_that_no_longer_binds_the_receipt_refuses(self):
        other = IMAGE[:-64] + "b" * 64
        serving = self.repo.serving
        cases = {
            "rollout names another image": (live_routes(image=other, message=serving), "provenance_mismatch"),
            "version from another sha": (live_routes(message="f" * 40), "provenance_mismatch"),
            "version never served": (live_routes(message=serving,
                                                 served=("22222222-2222-4222-8222-222222222222",)),
                                     "provenance_version_never_served"),
        }
        for name, (routes, code_name) in cases.items():
            with self.subTest(name):
                code, evidence, _, _ = self.run_main(routes=routes)
                self.assertEqual((code, evidence["code"]), (1, code_name))

    def test_missing_registry_image_refuses(self):
        def missing(image, account):
            raise base.GateError("rollback_image_missing")
        code, evidence, _, _ = self.run_main(registry=missing)
        self.assertEqual((code, evidence["code"]), (1, "rollback_image_missing"))

    def test_incompatible_migration_refuses_with_its_name(self):
        self.repo.write("crates/store/migrations/0003_rename.sql", "ALTER TABLE t RENAME TO u;\n")
        self.candidate = self.repo.commit("rename")
        code, evidence, _, _ = self.run_main()
        self.assertEqual(code, 1)
        self.assertEqual(evidence["code"], "backward_incompatible_migration")
        self.assertEqual(evidence["migration_refusals"], [{"migration": "0003_rename.sql", "code": "rename"}])

    def test_evidence_never_carries_sql_or_credentials(self):
        self.repo.write("crates/store/migrations/0003_rename.sql",
                        "ALTER TABLE secret_table_name RENAME TO u;\n")
        self.candidate = self.repo.commit("rename")
        _, evidence, stdout, _ = self.run_main()
        self.assertNotIn("secret_table_name", json.dumps(evidence) + stdout)


if __name__ == "__main__":
    unittest.main()
