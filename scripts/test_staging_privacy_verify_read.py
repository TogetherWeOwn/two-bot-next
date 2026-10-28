#!/usr/bin/env python3
"""Offline fixtures for scripts/staging_privacy_verify_read.py. No database, no network.

A fake `psql` executable stands in for the client, so the tests prove what the
script would send (argv, stdin, environment) and what it would write, without a
connection. The login URL, member id and H guild pin below are synthetic. The
sweep SQL is conformance-checked against the reviewed member_erasure manifest,
so manifest drift fails here and must be re-reviewed in the script.
"""

from contextlib import redirect_stderr, redirect_stdout
from datetime import datetime, timezone
import io
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_privacy_verify_read as reader

ROOT = Path(__file__).resolve().parents[1]
NOW = datetime(2026, 10, 22, 8, 0, 0, tzinfo=timezone.utc)
MEMBER = "123456789012345678"
H_PIN = "1555555555555555555"
PASSWORD = "s3cr3t-Pa55word"
STAGING_HOST = "ep-staging-example.us-east-2.aws.neon.tech"
URL = (f"postgresql://two_bot_privacy_verify_ro:{PASSWORD}@{STAGING_HOST}/two_bot"
       "?sslmode=require&channel_binding=require")
IDENTITY = "two_bot_privacy_verify_ro,two_bot\n"
FAKE_PSQL = """#!/usr/bin/env python3
import json, os, pathlib, re, sys
here = pathlib.Path(__file__).parent
behavior = json.loads((here / "behavior.json").read_text())
stdin = sys.stdin.read()
# Record presence only; the in-process database_target test checks the value.
logged = dict(os.environ)
if "PGPASSWORD" in logged:
    logged["PGPASSWORD"] = "REDACTED"
with open(here / "calls.jsonl", "a") as log:
    log.write(json.dumps({"argv": sys.argv[1:], "stdin": stdin, "environ": logged}) + "\\n")
# Failure text is built here from the live process values, never stored in
# behavior.json: that file is clear text and must not hold a credential.
kind = behavior.get("fail_kind")
if behavior.get("fail_on_query") and "current_user" in stdin:
    kind = None
if kind:
    values = {"member": "", "password": os.environ.get("PGPASSWORD", ""),
              "guild": "1545644954272137297"}
    for token in sys.argv[1:]:
        if token.startswith("member="):
            values["member"] = token[len("member="):]
        elif token.startswith("guild="):
            values["guild"] = token[len("guild="):]
    templates = {
        "permission": ("ERROR:  permission denied for table events\\n"
                       "LINE 5: AND member_id = '{member}'\\n"
                       "DETAIL: password {password} for guild {guild}\\n"),
        "auth": "psql: error: password authentication failed for user x {password}",
        "refused": "psql: error: connection to server at {member} failed: Connection refused",
        "timeout": "canceling statement due to statement timeout {member}",
        "strange": "strange failure {member}",
    }
    sys.stderr.write(templates[kind].format(**values))
    sys.exit(behavior.get("exit", 2))
if behavior.get("fail"):
    sys.stderr.write(behavior["fail"])
    sys.exit(behavior.get("exit", 2))
if "current_user" in stdin:
    sys.stdout.write(behavior["identity"])
    sys.exit(0)
match = re.search(r"FROM\\s+(\\w+)", stdin)
table = match.group(1) if match else ""
if stdin.lstrip().startswith("BEGIN") and "SELECT actor" in stdin:
    sys.stdout.write(behavior["audit"])
elif "IS DISTINCT FROM" in stdin:
    sys.stdout.write(behavior["sweep"][table])
else:
    sys.stdout.write(behavior["counts"][table])
"""


def zero_counts():
    return {table: "0\n" for table in reader.COUNT_TABLES}


def zero_sweep():
    return {table: "0\n" for table in reader.SWEEP_TABLES}


class Fixture:
    """A temp dir holding the fake psql and its behavior, plus a runner for main()."""

    def __init__(self, test, counts=None, sweep=None, audit="", identity=IDENTITY,
                 fail=None, fail_kind=None, fail_on_query=False, exit_code=2):
        self.dir = Path(tempfile.mkdtemp(prefix="privacy-verify-read-test-"))
        test.addCleanup(self.cleanup)
        self.psql = self.dir / "psql"
        self.psql.write_text(FAKE_PSQL)
        self.psql.chmod(self.psql.stat().st_mode | stat.S_IXUSR)
        merged_counts = zero_counts()
        merged_counts.update(counts or {})
        merged_sweep = zero_sweep()
        merged_sweep.update(sweep or {})
        self.set(counts=merged_counts, sweep=merged_sweep, audit=audit,
                 identity=identity, fail=fail, fail_kind=fail_kind,
                 fail_on_query=fail_on_query, exit=exit_code)
        self.output = self.dir / "out.json"
        self.summary = self.dir / "summary.md"

    def cleanup(self):
        for path in self.dir.iterdir():
            path.unlink()
        self.dir.rmdir()

    def set(self, **behavior):
        (self.dir / "behavior.json").write_text(json.dumps(behavior))

    def calls(self):
        log = self.dir / "calls.jsonl"
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    def run(self, env=None, guild="g", psql="fake"):
        # psql="" stands for a runner without psql (None would fall back to PATH).
        merged = {"PATH": os.environ.get("PATH", ""), "GH_TOKEN": "gh-token-must-not-reach-psql",
                  reader.URL_ENV: URL, reader.HOST_ENV: STAGING_HOST,
                  reader.MEMBER_ENV: MEMBER, reader.H_GUILD_ENV: H_PIN,
                  "GITHUB_STEP_SUMMARY": str(self.summary)}
        merged.update(env or {})
        merged = {key: value for key, value in merged.items() if value is not None}
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = reader.main(["--guild", guild, "--output", str(self.output)],
                               env=merged, now=NOW, psql=str(self.psql) if psql == "fake" else psql)
        return code, out.getvalue(), err.getvalue()

    def artifact(self):
        return json.loads(self.output.read_text())


class GuildAliasTests(unittest.TestCase):
    def test_g_is_the_pinned_staging_guild(self):
        self.assertEqual(reader.fence_guild("g", {}), "1545644954272137297")

    def test_h_requires_a_distinct_non_live_snowflake_pin(self):
        self.assertEqual(reader.fence_guild("h", {reader.H_GUILD_ENV: H_PIN}), H_PIN)
        bad = {"unset": {}, "empty": {reader.H_GUILD_ENV: ""},
               "not a snowflake": {reader.H_GUILD_ENV: "12345x"},
               "leading zero": {reader.H_GUILD_ENV: "01545644954272137297"},
               "too big": {reader.H_GUILD_ENV: "18446744073709551616"},
               "live guild": {reader.H_GUILD_ENV: reader.pins.LIVE_GUILD_ID},
               "staging guild": {reader.H_GUILD_ENV: reader.pins.STAGING_GUILD_ID}}
        for label, env in bad.items():
            with self.subTest(case=label), self.assertRaises(reader.Refused):
                reader.fence_guild("h", env)

    def test_h_refusals_never_echo_the_pin(self):
        evil = "99999-evil-pin"
        with self.assertRaises(reader.Refused) as caught:
            reader.fence_guild("h", {reader.H_GUILD_ENV: evil})
        self.assertNotIn(evil, str(caught.exception))

    def test_anything_but_g_or_h_refuses(self):
        for alias in ("", "G", "guild", "1545644954272137297", "g ", "gh", "0"):
            with self.subTest(alias=alias), self.assertRaises(reader.Refused):
                reader.fence_guild(alias, {reader.H_GUILD_ENV: H_PIN})

    def test_a_repinned_staging_constant_is_caught_by_the_smoke_guard(self):
        with mock.patch.object(reader.pins, "STAGING_GUILD_ID", reader.pins.LIVE_GUILD_ID), \
                self.assertRaises(reader.Refused) as caught:
            reader.fence_guild("g", {})
        self.assertIn("live guild", str(caught.exception))


class MemberTests(unittest.TestCase):
    def test_member_must_be_bound_digits(self):
        for value in ("1" * 15, "1" * 22, MEMBER):
            self.assertEqual(reader.fixture_member({reader.MEMBER_ENV: value}), value)
        for value in (None, "", "1" * 14, "1" * 23, "12345678901234x", " " + MEMBER, MEMBER + "\n",
                      "1234567890123456;DROP", "-" + MEMBER):
            with self.subTest(value=value), self.assertRaises(reader.Refused):
                reader.fixture_member({} if value is None else {reader.MEMBER_ENV: value})

    def test_refusals_never_echo_the_member(self):
        with self.assertRaises(reader.Refused) as caught:
            reader.fixture_member({reader.MEMBER_ENV: "12345678901234x"})
        self.assertNotIn("12345678901234x", str(caught.exception))


class DatabaseTargetTests(unittest.TestCase):
    def target(self, url, expected_host=STAGING_HOST):
        env = {reader.URL_ENV: url}
        if expected_host is not None:
            env[reader.HOST_ENV] = expected_host
        return reader.database_target(env)

    def test_the_login_becomes_pg_variables_with_no_url_left_behind(self):
        pg = self.target(URL)
        self.assertEqual(pg, {"PGHOST": STAGING_HOST,
                              "PGUSER": "two_bot_privacy_verify_ro", "PGPASSWORD": PASSWORD,
                              "PGDATABASE": "two_bot", "PGSSLMODE": "verify-full",
                              "PGSSLROOTCERT": reader.SYSTEM_CA_BUNDLE,
                              "PGCONNECT_TIMEOUT": "10", "PGCHANNELBINDING": "require"})

    def test_sslmode_is_upgraded_to_authenticated_hostname_verification(self):
        for suffix in ("", "?sslmode=require", "?sslmode=verify-ca"):
            with self.subTest(sslmode=suffix):
                pg = self.target(
                    f"postgres://two_bot_privacy_verify_ro:p%40ss%3Aw%2Frd@{STAGING_HOST}/two_bot{suffix}"
                )
                self.assertEqual(pg["PGSSLMODE"], "verify-full")
                self.assertEqual(pg["PGSSLROOTCERT"], reader.SYSTEM_CA_BUNDLE)

    def test_the_events_smoke_role_is_not_the_verify_role(self):
        with self.assertRaises(reader.Refused):
            self.target(URL.replace("two_bot_privacy_verify_ro", "two_bot_events_ro"))

    def test_refusals(self):
        good = f"postgresql://two_bot_privacy_verify_ro:pw@{STAGING_HOST}/two_bot"
        cases = {
            "missing": None,
            "empty": "",
            "not postgres": "mysql://two_bot_privacy_verify_ro:pw@h.example.net/two_bot",
            "no host": "postgresql://two_bot_privacy_verify_ro:pw@/two_bot",
            "two hosts": "postgresql://two_bot_privacy_verify_ro:pw@a.example.net,b.example.net/two_bot",
            "migrator role": good.replace("two_bot_privacy_verify_ro", "two_bot_migrator"),
            "runtime role": good.replace("two_bot_privacy_verify_ro", "two_bot_runtime"),
            "no password": f"postgresql://two_bot_privacy_verify_ro@{STAGING_HOST}/two_bot",
            "other database": good.replace("/two_bot", "/neondb"),
            "prod database": good.replace("/two_bot", "/two_bot_prod"),
            "no database": good.replace("/two_bot", ""),
            "prod host": good.replace("ep-staging-example", "ep-prod-example"),
            "bad port": good.replace("neon.tech", "neon.tech:notaport"),
            "options param": good + "?options=-c%20role%3Dneon_superuser",
            "host override": good + "?host=other.example.net",
            "weak sslmode": good + "?sslmode=disable",
            "bad channel binding": good + "?channel_binding=maybe",
            "bad timeout": good + "?connect_timeout=soon",
            "malformed query": good + "?sslmode",
        }
        for label, url in cases.items():
            env = {} if url is None else {reader.URL_ENV: url, reader.HOST_ENV: STAGING_HOST}
            with self.subTest(case=label), self.assertRaises(reader.Refused) as caught:
                reader.database_target(env)
            self.assertNotIn("pw@", str(caught.exception), "a refusal must not echo the login")

    def test_unknown_query_key_is_a_fixed_refusal_that_never_echoes_the_key(self):
        good = f"postgresql://two_bot_privacy_verify_ro:pw@{STAGING_HOST}/two_bot"
        with self.assertRaises(reader.Refused) as caught:
            self.target(good + "?evil%0d%0aSENTINEL=1")
        message = str(caught.exception)
        self.assertEqual(message, "refusing: the database login may not set that query parameter")
        self.assertNotIn("SENTINEL", message)


class ManifestConformanceTests(unittest.TestCase):
    """The sweep queries must embed the reviewed manifest predicates verbatim
    ($1/$2 translated to psql bindings). Manifest drift fails here and must
    be re-reviewed in the script, never auto-adopted."""

    def manifest(self):
        plan = json.loads((ROOT / "crates/cutover/src/member_erasure_plan.json").read_text())
        return {entry["table"]: entry["predicate"] for entry in plan["tables"]}

    @staticmethod
    def squashed(text):
        return re.sub(r"\s+", " ", text).strip()

    def test_the_count_tables_are_the_purge_tables_plus_suppression_and_receipt(self):
        service = (ROOT / "crates/cutover/src/privacy.rs").read_text()
        match = re.search(r"PURGE_TABLES[^=]*=\s*\[(.*?)\];", service, re.DOTALL)
        self.assertIsNotNone(match, "privacy.rs must keep its PURGE_TABLES const")
        purged = re.findall(r'"([a-z_]+)"', match.group(1))
        self.assertEqual(list(reader.COUNT_TABLES),
                         purged + ["privacy_opt_outs", "privacy_opt_out_audit"])

    def test_sweeps_exist_exactly_for_manifests_matching_beyond_ownership(self):
        manifest = self.manifest()
        regex_tables = {table for table in reader.COUNT_TABLES[:4] if "~" in manifest[table]}
        self.assertEqual(set(reader.SWEEP_SQL), regex_tables)
        self.assertEqual(set(reader.SWEEP_SQL), set(reader.SWEEP_TABLES))
        for table in reader.COUNT_TABLES[:4]:
            if table not in regex_tables:
                self.assertNotIn(table, reader.SWEEP_SQL)

    def test_each_sweep_embeds_its_manifest_predicate_verbatim(self):
        manifest = self.manifest()
        for table, sql in reader.SWEEP_SQL.items():
            translated = manifest[table].replace("$1", ":'guild'").replace("$2", ":'member'")
            with self.subTest(table=table):
                self.assertIn(self.squashed(translated), self.squashed(sql))

    def test_each_sweep_excludes_directly_owned_rows(self):
        for table, sql in reader.SWEEP_SQL.items():
            owner = reader.OWNER_COLUMN[table]
            with self.subTest(table=table):
                self.assertIn(f"AND {owner} IS DISTINCT FROM :'member'", sql)

    def test_each_count_uses_its_owner_column_equality(self):
        for table in reader.COUNT_TABLES:
            sql = reader.count_sql(table)
            owner = reader.OWNER_COLUMN[table]
            with self.subTest(table=table):
                self.assertIn(f"FROM {table}", sql)
                self.assertIn(f"guild_id = :'guild' AND {owner} = :'member'", sql)
                for forbidden in ("metadata", "source_event_id", "idempotency_key", "~"):
                    self.assertNotIn(forbidden, sql)


class ReadTests(unittest.TestCase):
    def test_a_read_writes_counts_sweep_and_receipts(self):
        fixture = Fixture(self,
                          counts={"events": "3\n", "privacy_opt_outs": "1\n",
                                  "privacy_opt_out_audit": "2\n"},
                          sweep={"events": "1\n"},
                          audit=("member:self-serve,2026-10-22T07:01:00.000Z\n"
                                 "operator:fixture-check,2026-10-22T07:31:00.500Z\n"))
        code, out, err = fixture.run()
        self.assertEqual((code, err), (0, ""))
        self.assertEqual(fixture.artifact(), {
            "schema_version": 1, "guild": "g", "collected_at": "2026-10-22T08:00:00.000Z",
            "counts": {"events": 3, "members": 0, "invite_snapshots": 0, "community_facts": 0,
                       "privacy_opt_outs": 1, "privacy_opt_out_audit": 2},
            "sweep_extra": {"events": 1, "community_facts": 0},
            "audit": {"truncated": False,
                      "rows": [{"ordinal": 1, "actor": "member:self-serve",
                                "opted_out_at": "2026-10-22T07:01:00.000Z"},
                               {"ordinal": 2, "actor": "operator:fixture-check",
                                "opted_out_at": "2026-10-22T07:31:00.500Z"}]}})
        self.assertEqual(reader.artifact_errors(fixture.artifact()), [])
        self.assertEqual(out, "ok: guild=g audit_rows=2 truncated=false\n")
        summary = fixture.summary.read_text()
        self.assertIn("guild: g", summary)
        self.assertIn("- events: 3", summary)
        self.assertIn("- sweep_extra events: 1", summary)
        self.assertIn("audit rows: 2 (truncated: false)", summary)

    def test_an_h_read_uses_the_h_pin_and_names_only_the_alias(self):
        fixture = Fixture(self)
        code, out, err = fixture.run(guild="h")
        self.assertEqual((code, err), (0, ""))
        self.assertEqual(fixture.artifact()["guild"], "h")
        guild_args = [token for call in fixture.calls() for token in call["argv"]
                      if token.startswith("guild=")]
        self.assertTrue(guild_args)
        self.assertTrue(all(arg == f"guild={H_PIN}" for arg in guild_args))
        written = fixture.output.read_text() + fixture.summary.read_text() + out + err
        self.assertNotIn(H_PIN, written)
        self.assertNotIn(MEMBER, written)

    def test_nothing_identifying_reaches_the_artifact_the_log_or_the_summary(self):
        fixture = Fixture(self, counts={"events": "5\n"},
                          audit="member:self-serve,2026-10-22T07:01:00.000Z\n")
        code, out, err = fixture.run()
        self.assertEqual(code, 0)
        written = fixture.output.read_text() + fixture.summary.read_text() + out + err
        for forbidden in (MEMBER, reader.pins.STAGING_GUILD_ID, H_PIN, PASSWORD,
                          "neon.tech", "two_bot_privacy_verify_ro", "idempotency",
                          "metadata", "source_event_id"):
            self.assertNotIn(forbidden, written)

    def test_the_sql_is_read_only_capped_and_column_limited(self):
        fixture = Fixture(self)
        fixture.run()
        calls = fixture.calls()
        # Identity probe, six equality counts, two sweeps, one audit read.
        self.assertEqual(len(calls), 10)
        for call in calls:
            self.assertTrue(call["stdin"].startswith("BEGIN READ ONLY;"))
            self.assertIn("statement_timeout = '10s'", call["stdin"])
            self.assertTrue(call["stdin"].rstrip().endswith("COMMIT;"))
            for forbidden in ("SELECT *", "INSERT", "UPDATE", "DELETE", "DROP", "GRANT"):
                self.assertNotIn(forbidden, call["stdin"])
        from_tables = [re.search(r"FROM\s+(\w+)", call["stdin"]).group(1) for call in calls[1:7]]
        self.assertEqual(from_tables, list(reader.COUNT_TABLES))
        for call in calls[1:7]:
            self.assertIn("COUNT(*)", call["stdin"])
            self.assertIn("guild_id = :'guild'", call["stdin"])
        audit_sql = calls[9]["stdin"]
        self.assertIn("SELECT actor,", audit_sql)
        self.assertIn("FROM privacy_opt_out_audit", audit_sql)
        self.assertIn("ORDER BY id", audit_sql)
        self.assertIn("LIMIT 11;", audit_sql)

    def test_variables_and_flags_reach_psql_and_no_secret_does(self):
        fixture = Fixture(self)
        with mock.patch.object(reader.subprocess, "run", wraps=reader.subprocess.run) as run:
            code, _, err = fixture.run()
        self.assertEqual((code, err), (0, ""))
        self.assertEqual(run.call_count, 10)
        for call in run.call_args_list:
            self.assertEqual(call.kwargs["env"]["PGPASSWORD"], PASSWORD)
            self.assertEqual(call.kwargs["env"]["PGSSLMODE"], "verify-full")
            self.assertEqual(call.kwargs["env"]["PGSSLROOTCERT"], reader.SYSTEM_CA_BUNDLE)
            self.assertEqual(call.kwargs.get("timeout"), 60)
        identity_call = fixture.calls()[0]
        self.assertEqual(identity_call["argv"],
                         ["-X", "-q", "-t", "--csv", "-v", "ON_ERROR_STOP=1", "-f", "-"])
        rows_call = fixture.calls()[1]
        self.assertEqual(rows_call["argv"], [
            "-X", "-q", "-t", "--csv", "-v", "ON_ERROR_STOP=1",
            "-v", "guild=1545644954272137297", "-v", f"member={MEMBER}", "-f", "-"])
        for call in fixture.calls():
            joined = " ".join(call["argv"])
            self.assertNotIn(PASSWORD, joined)
            self.assertNotIn("postgres", joined)
            self.assertNotIn(PASSWORD, call["stdin"])
            environ = call["environ"]
            self.assertEqual(environ["PGPASSWORD"], "REDACTED")
            self.assertEqual(environ["PGUSER"], "two_bot_privacy_verify_ro")
            self.assertEqual(environ["PGDATABASE"], "two_bot")
            self.assertNotIn("GH_TOKEN", environ)
            self.assertNotIn(reader.URL_ENV, environ)
            self.assertNotIn(reader.MEMBER_ENV, environ)
            self.assertNotIn(reader.H_GUILD_ENV, environ)
            self.assertNotIn("PGOPTIONS", environ)

    def test_count_boundaries(self):
        ceiling = str(reader.COUNT_SANITY_MAX)
        fixture = Fixture(self, counts={"events": ceiling + "\n"})
        code, _, err = fixture.run()
        self.assertEqual((code, err), (0, ""))
        self.assertEqual(fixture.artifact()["counts"]["events"], reader.COUNT_SANITY_MAX)
        for label, value in {"over the ceiling": str(reader.COUNT_SANITY_MAX + 1) + "\n",
                             "negative": "-1\n", "not digits": "many\n",
                             "two columns": "1,2\n", "two rows": "1\n2\n",
                             "empty": "", "blank": "\n",
                             "hostile width": "9" * 5000 + "\n"}.items():
            with self.subTest(case=label):
                bad = Fixture(self, counts={"events": value})
                code, out, err = bad.run()
                self.assertEqual(code, 1, err)
                self.assertEqual(out, "")
                self.assertIn("count", err)
                if value.strip():
                    self.assertNotIn(value.strip()[:24], err + out)
                self.assertFalse(bad.output.exists())

    def test_audit_overflow_is_truncated_and_flagged(self):
        rows = "".join(f"member:self-serve,2026-10-22T07:{index:02d}:00.000Z\n"
                       for index in range(11))
        fixture = Fixture(self, audit=rows)
        code, out, _ = fixture.run()
        self.assertEqual(code, 0)
        audit = fixture.artifact()["audit"]
        self.assertEqual((audit["truncated"], len(audit["rows"])), (True, 10))
        self.assertEqual([row["ordinal"] for row in audit["rows"]], list(range(1, 11)))
        self.assertEqual(reader.artifact_errors(fixture.artifact()), [])
        self.assertIn("truncated=true", out)

    def test_more_than_the_query_limit_is_refused_not_trusted(self):
        rows = "".join(f"member:self-serve,2026-10-22T07:{index // 60:02d}:{index % 60:02d}.000Z\n"
                       for index in range(12))
        fixture = Fixture(self, audit=rows)
        code, _, err = fixture.run()
        self.assertEqual(code, 1)
        self.assertFalse(fixture.output.exists())
        self.assertIn("query limit", err)

    def test_a_bad_actor_or_timestamp_fails_loudly_and_writes_nothing(self):
        bad = {
            "empty actor": ",2026-10-22T07:01:00.000Z\n",
            "blank actor": "   ,2026-10-22T07:01:00.000Z\n",
            "129-byte actor": f"{'x' * 129},2026-10-22T07:01:00.000Z\n",
            "control char": "op\nerator,2026-10-22T07:01:00.000Z\n",
            "extra column": "member:self-serve,2026-10-22T07:01:00.000Z,x\n",
            "single column": "member:self-serve\n",
            "impossible date": "member:self-serve,2026-02-30T07:01:00.000Z\n",
            "no millis": "member:self-serve,2026-10-22T07:01:00Z\n",
            "timestamptz text": "member:self-serve,2026-10-22 07:01:00+00\n",
        }
        for label, audit in bad.items():
            with self.subTest(case=label):
                fixture = Fixture(self, audit=audit)
                code, out, err = fixture.run()
                self.assertEqual(code, 1, err)
                self.assertFalse(fixture.output.exists())
                self.assertIn("unexpected shape", err)
                self.assertNotIn("1545644954272137297", err + out)
        # The service accepts this shape, but an unreviewed label must not publish.
        self.assertTrue(reader.valid_actor("x" * 128))
        fixture = Fixture(self, audit=f"{'x' * 128},2026-10-22T07:01:00.000Z\n")
        code, out, err = fixture.run()
        self.assertEqual(code, 1)
        self.assertEqual(out, "")
        self.assertIn("not approved", err)
        self.assertFalse(fixture.output.exists())
        self.assertFalse(fixture.summary.exists())

    def test_an_actor_naming_the_fixture_subject_fails_closed(self):
        stamp = "2026-10-22T07:01:00.000Z\n"
        for label, actor in (("member id as actor", MEMBER),
                             ("guild id as actor", "1545644954272137297")):
            with self.subTest(case=label):
                fixture = Fixture(self, audit=f"{actor},{stamp}")
                code, out, err = fixture.run()
                self.assertEqual(code, 1, err)
                self.assertFalse(fixture.output.exists())
                self.assertIn("fixture subject", err)
                self.assertNotIn(MEMBER, err + out)
                self.assertNotIn("1545644954272137297", err + out)

    def test_an_actor_containing_the_member_id_fails_closed_for_both_aliases(self):
        for alias in ("g", "h"):
            for label, actor in (("prefix", f"member:{MEMBER}"),
                                 ("suffix", f"{MEMBER}:fixture"),
                                 ("embedded", f"member:{MEMBER}:fixture")):
                with self.subTest(alias=alias, case=label):
                    fixture = Fixture(self, audit=f"{actor},2026-10-22T07:01:00.000Z\n")
                    code, out, err = fixture.run(guild=alias)
                    self.assertEqual(code, 1)
                    self.assertEqual(out, "")
                    self.assertIn("not approved", err)
                    self.assertFalse(fixture.output.exists())
                    self.assertFalse(fixture.summary.exists())
                    for value in (actor, MEMBER, reader.pins.STAGING_GUILD_ID, H_PIN):
                        self.assertNotIn(value, out + err)

    def test_actors_naming_either_guild_fail_closed_for_both_aliases(self):
        for alias in ("g", "h"):
            for guild_label, pin in (("G", reader.pins.STAGING_GUILD_ID), ("H", H_PIN)):
                for form, actor in (("bare", pin), ("embedded", f"operator:{pin}:fixture")):
                    with self.subTest(alias=alias, guild=guild_label, form=form):
                        fixture = Fixture(self, audit=f"{actor},2026-10-22T07:01:00.000Z\n")
                        code, out, err = fixture.run(guild=alias)
                        self.assertEqual(code, 1)
                        self.assertEqual(out, "")
                        self.assertIn(err, (
                            "failed: an audit row named the fixture subject; nothing was written\n",
                            "failed: an audit actor was not approved; nothing was written\n"))
                        self.assertFalse(fixture.output.exists())
                        self.assertFalse(fixture.summary.exists())
                        for value in (actor, MEMBER, reader.pins.STAGING_GUILD_ID, H_PIN):
                            self.assertNotIn(value, out + err)

    def test_unreviewed_actor_text_is_not_published_or_normalized(self):
        for label, actor in (("unknown", "operator:unreviewed"),
                             ("leading space", " member:self-serve"),
                             ("trailing space", "member:self-serve "),
                             ("case change", "Member:self-serve"),
                             ("suffix", "operator:fixture-check:extra")):
            with self.subTest(case=label):
                fixture = Fixture(self, audit=f"{actor},2026-10-22T07:01:00.000Z\n")
                code, out, err = fixture.run()
                self.assertEqual(code, 1)
                self.assertEqual(out, "")
                self.assertIn("not approved", err)
                self.assertNotIn(actor, err)
                self.assertFalse(fixture.output.exists())
                self.assertFalse(fixture.summary.exists())

    def test_an_unapproved_overflow_witness_prevents_partial_publication(self):
        rows = "member:self-serve,2026-10-22T07:01:00.000Z\n" * 10
        rows += f"operator:{MEMBER}:fixture,2026-10-22T07:02:00.000Z\n"
        fixture = Fixture(self, audit=rows)
        code, out, err = fixture.run()
        self.assertEqual(code, 1)
        self.assertEqual(out, "")
        self.assertIn("not approved", err)
        self.assertNotIn(MEMBER, err)
        self.assertFalse(fixture.output.exists())
        self.assertFalse(fixture.summary.exists())

    def test_final_validation_stops_an_identity_bearing_row_if_the_parser_is_bypassed(self):
        fixture = Fixture(self)
        with mock.patch.object(reader, "parse_audit_rows", return_value=[{
                "actor": f"operator:{MEMBER}:fixture",
                "opted_out_at": "2026-10-22T07:01:00.000Z"}]):
            code, out, err = fixture.run()
        self.assertEqual(code, 1)
        self.assertEqual(out, "")
        self.assertEqual(err, "failed: the artifact failed its own shape check; nothing was written\n")
        self.assertNotIn(MEMBER, err)
        self.assertFalse(fixture.output.exists())
        self.assertFalse(fixture.summary.exists())

    def test_a_wrong_server_identity_stops_before_the_counts_are_read(self):
        for identity in ("two_bot_migrator,two_bot\n", "two_bot_privacy_verify_ro,neondb\n",
                         "", "two_bot_privacy_verify_ro,two_bot\nx,y\n"):
            with self.subTest(identity=identity):
                fixture = Fixture(self, identity=identity)
                code, _, err = fixture.run()
                self.assertEqual(code, 2)
                self.assertFalse(fixture.output.exists())
                self.assertEqual(len(fixture.calls()), 1, "no count query must run")
                self.assertIn("refusing", err)

    def test_psql_stderr_is_never_echoed(self):
        for fail_on_query in (False, True):
            for kind, phrase in (("permission", "permission denied for the read-only role"),
                                 ("auth", "authentication failed"),
                                 ("refused", "could not connect"),
                                 ("timeout", "statement timeout"),
                                 ("strange", "psql failed")):
                with self.subTest(phrase=phrase, fail_on_query=fail_on_query):
                    fixture = Fixture(self, fail_kind=kind, fail_on_query=fail_on_query,
                                      exit_code=2)
                    code, out, err = fixture.run()
                    self.assertEqual(code, 1)
                    self.assertEqual(out, "")
                    self.assertEqual(err, f"failed: {phrase} (psql exit 2)\n")
                    calls = fixture.calls()
                    self.assertEqual(len(calls), 2 if fail_on_query else 1)
                    for secret in (MEMBER, PASSWORD, "1545644954272137297", "LINE 5"):
                        self.assertNotIn(secret, out + err)
                    self.assertFalse(fixture.output.exists())

    def test_client_timeout_fails_without_publishing_or_echoing_exception_details(self):
        for after_probe in (False, True):
            with self.subTest(after_probe=after_probe):
                fixture = Fixture(self)
                failure = subprocess.TimeoutExpired(
                    cmd=[str(fixture.psql), MEMBER, PASSWORD], timeout=60,
                    output=f"partial output {MEMBER}", stderr=f"private diagnostic {PASSWORD}")
                effects = [failure]
                if after_probe:
                    effects.insert(0, subprocess.CompletedProcess([], 0, IDENTITY, ""))
                with mock.patch.object(reader.subprocess, "run", side_effect=effects):
                    code, out, err = fixture.run()
                self.assertEqual((code, out, err), (1, "", "failed: psql timed out\n"))
                self.assertFalse(fixture.output.exists())
                self.assertFalse(fixture.summary.exists())

    def test_client_start_failure_does_not_publish_or_echo_os_details(self):
        for after_probe in (False, True):
            with self.subTest(after_probe=after_probe):
                fixture = Fixture(self)
                effects = [OSError(f"private OS diagnostic {MEMBER} {PASSWORD}")]
                if after_probe:
                    effects.insert(0, subprocess.CompletedProcess([], 0, IDENTITY, ""))
                with mock.patch.object(reader.subprocess, "run", side_effect=effects):
                    code, out, err = fixture.run()
                self.assertEqual((code, out, err), (1, "", "failed: psql could not start\n"))
                self.assertFalse(fixture.output.exists())
                self.assertFalse(fixture.summary.exists())

    def test_an_unexpected_exception_does_not_print_a_traceback(self):
        fixture = Fixture(self)
        with mock.patch.object(reader, "read_verify",
                               side_effect=RuntimeError(f"boom {MEMBER} {PASSWORD}")):
            code, out, err = fixture.run()
        self.assertEqual(code, 1)
        self.assertEqual(err, "failed: unexpected RuntimeError; nothing was written\n")
        self.assertFalse(fixture.output.exists())

    def test_the_summary_is_optional(self):
        fixture = Fixture(self)
        code, _, _ = fixture.run(env={"GITHUB_STEP_SUMMARY": None})
        self.assertEqual(code, 0)
        self.assertFalse(fixture.summary.exists())


class FenceTests(unittest.TestCase):
    """Every refusal is exit 2 and happens before psql runs even once."""

    def assert_refused(self, fixture, code, err, fragment):
        self.assertEqual(code, 2, err)
        self.assertIn(fragment, err)
        self.assertEqual(fixture.calls(), [], "psql must not run")
        self.assertFalse(fixture.output.exists())

    def test_a_bad_guild_alias_refuses(self):
        fixture = Fixture(self)
        code, _, err = fixture.run(guild="x")
        self.assert_refused(fixture, code, err, "guild must be g or h")

    def test_an_unestablished_h_refuses(self):
        fixture = Fixture(self)
        code, _, err = fixture.run(guild="h", env={reader.H_GUILD_ENV: None})
        self.assert_refused(fixture, code, err, "not established")

    def test_a_live_h_pin_refuses(self):
        fixture = Fixture(self)
        code, _, err = fixture.run(guild="h", env={reader.H_GUILD_ENV: reader.pins.LIVE_GUILD_ID})
        self.assert_refused(fixture, code, err, "live guild")

    def test_a_missing_member_refuses(self):
        fixture = Fixture(self)
        code, _, err = fixture.run(env={reader.MEMBER_ENV: None})
        self.assert_refused(fixture, code, err, "no fixture member bound")

    def test_a_missing_login_refuses(self):
        fixture = Fixture(self)
        code, _, err = fixture.run(env={reader.URL_ENV: None})
        self.assert_refused(fixture, code, err, "no database login bound")

    def test_the_wrong_login_refuses(self):
        fixture = Fixture(self)
        code, _, err = fixture.run(env={reader.URL_ENV: URL.replace("/two_bot?", "/two_bot_prod?")})
        self.assert_refused(fixture, code, err, "production-like")

    def test_a_different_valid_staging_host_refuses_before_psql(self):
        fixture = Fixture(self)
        code, out, err = fixture.run(env={
            reader.URL_ENV: URL.replace(STAGING_HOST, "other-staging.example.net")})
        self.assert_refused(fixture, code, err, "host does not match the pinned staging endpoint")
        self.assertEqual(out, "")
        self.assertEqual(err, "refusing: the database host does not match the pinned staging endpoint\n")
        self.assertFalse(fixture.summary.exists())

    def test_an_absent_or_blank_host_pin_refuses_before_psql(self):
        for pin in (None, "", "   "):
            with self.subTest(pin=pin):
                fixture = Fixture(self)
                code, out, err = fixture.run(env={reader.HOST_ENV: pin})
                self.assert_refused(fixture, code, err, "no valid staging host pin bound")
                self.assertEqual(out, "")
                self.assertEqual(err, f"refusing: no valid staging host pin bound (${reader.HOST_ENV})\n")
                self.assertFalse(fixture.summary.exists())

    def test_a_numeric_non_endpoint_port_refuses_before_psql(self):
        fixture = Fixture(self)
        code, out, err = fixture.run(env={
            reader.URL_ENV: URL.replace(STAGING_HOST, STAGING_HOST + ":5433")})
        self.assert_refused(fixture, code, err, "port must be the PostgreSQL endpoint port")
        self.assertEqual(out, "")
        self.assertEqual(err, "refusing: the database port must be the PostgreSQL endpoint port\n")
        self.assertFalse(fixture.summary.exists())

    def test_a_runner_without_psql_refuses(self):
        fixture = Fixture(self)
        code, _, err = fixture.run(psql="")
        self.assert_refused(fixture, code, err, "psql is not installed")

    def test_the_member_is_read_from_the_environment_never_a_flag(self):
        for flag in ("--member", "--window-start", "--window-end"):
            with self.subTest(flag=flag), self.assertRaises(SystemExit), \
                    redirect_stderr(io.StringIO()):
                reader.main(["--guild", "g", "--output", "x.json", flag, "1"],
                            env={}, now=NOW, psql="psql")


class ShapeTests(unittest.TestCase):
    def good(self):
        return reader.build_artifact(
            "g", "2026-10-22T08:00:00.000Z",
            {table: 0 for table in reader.COUNT_TABLES},
            {table: 0 for table in reader.SWEEP_TABLES},
            [{"actor": "member:self-serve", "opted_out_at": "2026-10-22T07:01:00.000Z"}])

    def test_final_validation_rejects_unreviewed_actors_without_fixture_context(self):
        actors = (("member", MEMBER), ("G", reader.pins.STAGING_GUILD_ID), ("H", H_PIN))
        for alias in ("g", "h"):
            for label, value in actors:
                for form, actor in (("bare", value), ("embedded", f"operator:{value}:fixture")):
                    with self.subTest(alias=alias, subject=label, form=form):
                        artifact = self.good()
                        artifact["guild"] = alias
                        artifact["audit"]["rows"][0]["actor"] = actor
                        self.assertEqual(reader.artifact_errors(artifact), ["audit row 1"])
        artifact = self.good()
        artifact["audit"]["rows"][0]["actor"] = "operator:unreviewed"
        self.assertEqual(reader.artifact_errors(artifact), ["audit row 1"])

    def test_the_artifact_contract_rejects_extra_fields(self):
        good = self.good()
        self.assertEqual(reader.artifact_errors(good), [])
        for label, change in {
            "top-level key": lambda a: a.update({"member": MEMBER}),
            "guild pin": lambda a: a.update({"guild": "1545644954272137297"}),
            "guild alias": lambda a: a.update({"guild": "x"}),
            "collected_at": lambda a: a.update({"collected_at": "yesterday"}),
            "counts key": lambda a: a["counts"].update({"guild_analytics_settings": 0}),
            "missing count": lambda a: a["counts"].pop("events"),
            "negative count": lambda a: a["counts"].update({"events": -1}),
            "bool count": lambda a: a["counts"].update({"events": True}),
            "float count": lambda a: a["counts"].update({"events": 1.0}),
            "over-ceiling count": lambda a: a["counts"].update(
                {"events": reader.COUNT_SANITY_MAX + 1}),
            "sweep key": lambda a: a["sweep_extra"].update({"members": 0}),
            "sweep value": lambda a: a["sweep_extra"].update({"events": -1}),
            "audit key": lambda a: a["audit"].update({"count": 1}),
            "row key": lambda a: a["audit"]["rows"][0].update({"member_id": MEMBER}),
            "bad actor": lambda a: a["audit"]["rows"][0].update({"actor": ""}),
            "bad timestamp": lambda a: a["audit"]["rows"][0].update(
                {"opted_out_at": "2026-02-30T07:01:00.000Z"}),
            "ordinal": lambda a: a["audit"]["rows"][0].update({"ordinal": 2}),
            "truncated type": lambda a: a["audit"].update({"truncated": "no"}),
            "schema version": lambda a: a.update({"schema_version": 2}),
            "over cap": lambda a: a["audit"].update(
                {"rows": a["audit"]["rows"] * 11}),
        }.items():
            with self.subTest(change=label):
                broken = json.loads(json.dumps(good))
                change(broken)
                self.assertTrue(reader.artifact_errors(broken))


class WiringTests(unittest.TestCase):
    """The workflow, the script and the doc must agree on names; text checks only."""

    def test_the_workflow_binds_the_names_the_script_reads(self):
        workflow = (ROOT / ".github/workflows/staging-privacy-verify-read.yml").read_text()
        for secret in (reader.URL_ENV, reader.HOST_ENV, reader.MEMBER_ENV, reader.H_GUILD_ENV):
            self.assertIn(f"{secret}: ${{{{ secrets.{secret} }}}}", workflow)
        self.assertIn("environment: staging-privacy-verify-read", workflow)
        self.assertIn("scripts/staging_privacy_verify_read.py", workflow)

    def test_the_privacy_doc_documents_the_ci_route(self):
        doc = (ROOT / "docs/privacy.md").read_text()
        for fragment in ("staging-privacy-verify-read", reader.MEMBER_ENV, reader.URL_ENV,
                         reader.H_GUILD_ENV, "staging-privacy-verify-read-<run_id>.json",
                         "two_bot_privacy_verify_ro"):
            self.assertIn(fragment, doc)


if __name__ == "__main__":
    unittest.main()
