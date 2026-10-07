#!/usr/bin/env python3
"""Offline fixtures for scripts/staging_events_read.py. No database, no network.

A fake `psql` executable stands in for the client, so the tests prove what the
script would send (argv, stdin, environment) and what it would write, without a
connection. The login URL and member id below are synthetic.
"""

from contextlib import redirect_stderr, redirect_stdout
from datetime import datetime, timezone
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_events_read as reader  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
NOW = datetime(2026, 10, 7, 16, 0, 0, tzinfo=timezone.utc)
START, END = "2026-10-07T14:00:00Z", "2026-10-07T14:15:00Z"
MEMBER = "123456789012345678"
PASSWORD = "s3cr3t-Pa55word"
URL = (f"postgresql://two_bot_events_ro:{PASSWORD}@ep-staging-example.us-east-2.aws.neon.tech/two_bot"
       "?sslmode=require&channel_binding=require")
IDENTITY = "two_bot_events_ro,two_bot\n"
FAKE_PSQL = """#!/usr/bin/env python3
import hashlib, json, os, pathlib, sys
here = pathlib.Path(__file__).parent
behavior = json.loads((here / "behavior.json").read_text())
stdin = sys.stdin.read()
# The login password must never reach a file in clear: log a marker plus a
# hash the test compares, so a leaked log still proves nothing.
logged = dict(os.environ)
secret = logged.pop("PGPASSWORD", None)
if secret is not None:
    logged["PGPASSWORD_SHA256"] = hashlib.sha256(secret.encode()).hexdigest()
    logged["PGPASSWORD"] = "REDACTED"
with open(here / "calls.jsonl", "a") as log:
    log.write(json.dumps({"argv": sys.argv[1:], "stdin": stdin, "environ": logged}) + "\\n")
# Failure text is built here from the live process values, never stored in
# behavior.json: that file is clear text and must not hold a credential.
kind = behavior.get("fail_kind")
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
sys.stdout.write(behavior["identity"] if "current_user" in stdin else behavior["rows"])
"""


def rows_csv(count, event_type="message_created"):
    return "".join(f"{event_type},2026-10-07T14:{index // 60:02d}:{index % 60:02d}.250Z\n"
                   for index in range(count))


class Fixture:
    """A temp dir holding the fake psql and its behavior, plus a runner for main()."""

    def __init__(self, test, rows="", identity=IDENTITY, fail=None, fail_kind=None, exit_code=2):
        self.dir = Path(tempfile.mkdtemp(prefix="events-read-test-"))
        test.addCleanup(self.cleanup)
        self.psql = self.dir / "psql"
        self.psql.write_text(FAKE_PSQL)
        self.psql.chmod(self.psql.stat().st_mode | stat.S_IXUSR)
        self.set(rows=rows, identity=identity, fail=fail, fail_kind=fail_kind, exit=exit_code)
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

    def run(self, env=None, start=START, end=END, psql="fake"):
        # psql="" stands for a runner without psql (None would fall back to PATH).
        merged = {"PATH": os.environ.get("PATH", ""), "GH_TOKEN": "gh-token-must-not-reach-psql",
                  reader.URL_ENV: URL, reader.MEMBER_ENV: MEMBER, "GITHUB_STEP_SUMMARY": str(self.summary)}
        merged.update(env or {})
        merged = {key: value for key, value in merged.items() if value is not None}
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = reader.main(["--window-start", start, "--window-end", end, "--output", str(self.output)],
                               env=merged, now=NOW, psql=str(self.psql) if psql == "fake" else psql)
        return code, out.getvalue(), err.getvalue()

    def artifact(self):
        return json.loads(self.output.read_text())


class WindowTests(unittest.TestCase):
    def test_a_valid_window_is_normalized_to_the_table_text_form(self):
        self.assertEqual(reader.parse_window(START, END, NOW),
                         ("2026-10-07T14:00:00.000Z", "2026-10-07T14:15:00.000Z"))
        self.assertEqual(reader.parse_window("2026-10-07T14:00:00.5Z", "2026-10-07T14:00:01.25Z", NOW),
                         ("2026-10-07T14:00:00.500Z", "2026-10-07T14:00:01.250Z"))

    def test_refusals(self):
        cases = {
            "naive": ("2026-10-07T14:00:00", END),
            "offset": ("2026-10-07T14:00:00+00:00", END),
            "other offset": ("2026-10-07T09:00:00-05:00", END),
            "space separator": ("2026-10-07 14:00:00Z", END),
            "microseconds": ("2026-10-07T14:00:00.123456Z", END),
            "date only": ("2026-10-07", END),
            "empty": ("", END),
            "injection": ("2026-10-07T14:00:00Z'; DROP TABLE events;--", END),
            "newline": (START + "\n::set-output", END),
            "not a calendar day": ("2026-02-30T14:00:00Z", END),
            "leap second": ("2026-10-07T14:00:60Z", END),
            "end equals start": (START, START),
            "end before start": (END, START),
            "sixteen minutes": (START, "2026-10-07T14:16:00Z"),
            "fifteen minutes and a millisecond": (START, "2026-10-07T14:15:00.001Z"),
            "still running": ("2026-10-07T15:50:00Z", "2026-10-07T16:00:00.001Z"),
            "in the future": ("2026-10-07T17:00:00Z", "2026-10-07T17:05:00Z"),
        }
        for label, (start, end) in cases.items():
            with self.subTest(case=label), self.assertRaises(reader.Refused):
                reader.parse_window(start, end, NOW)

    def test_boundaries_that_must_pass(self):
        reader.parse_window(START, "2026-10-07T14:15:00.000Z", NOW)
        reader.parse_window("2026-10-07T15:45:00Z", "2026-10-07T16:00:00Z", NOW)  # ends exactly now


class MemberAndGuildTests(unittest.TestCase):
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

    def test_the_guild_is_the_pinned_staging_guild_and_the_live_guild_refuses(self):
        self.assertEqual(reader.fence_guild(), "1545644954272137297")
        for guild in (reader.pins.LIVE_GUILD_ID, "1", ""):
            with self.subTest(guild=guild), self.assertRaises(reader.Refused):
                reader.fence_guild(guild)
        # A repinned constant is caught by the same guard the Discord smokes use.
        with mock.patch.object(reader.pins, "STAGING_GUILD_ID", reader.pins.LIVE_GUILD_ID), \
                self.assertRaises(reader.Refused) as caught:
            reader.fence_guild()
        self.assertIn("live guild", str(caught.exception))


class DatabaseTargetTests(unittest.TestCase):
    def target(self, url):
        return reader.database_target({reader.URL_ENV: url})

    def test_the_login_becomes_pg_variables_with_no_url_left_behind(self):
        pg = self.target(URL)
        self.assertEqual(pg, {"PGHOST": "ep-staging-example.us-east-2.aws.neon.tech",
                              "PGUSER": "two_bot_events_ro", "PGPASSWORD": PASSWORD,
                              "PGDATABASE": "two_bot", "PGSSLMODE": "require",
                              "PGCONNECT_TIMEOUT": "10", "PGCHANNELBINDING": "require"})

    def test_port_percent_encoded_password_and_default_sslmode(self):
        pg = self.target("postgres://two_bot_events_ro:p%40ss%3Aw%2Frd@staging-db.example.net:6543/two_bot")
        self.assertEqual((pg["PGPORT"], pg["PGPASSWORD"], pg["PGSSLMODE"]), ("6543", "p@ss:w/rd", "require"))

    def test_the_pooler_host_is_allowed_because_the_read_is_one_plain_select(self):
        self.target("postgresql://two_bot_events_ro:pw@ep-staging-example-pooler.neon.tech/two_bot")

    def test_refusals(self):
        good = "postgresql://two_bot_events_ro:pw@ep-staging-example.neon.tech/two_bot"
        cases = {
            "missing": None,
            "empty": "",
            "not postgres": "mysql://two_bot_events_ro:pw@h.example.net/two_bot",
            "no host": "postgresql://two_bot_events_ro:pw@/two_bot",
            "two hosts": "postgresql://two_bot_events_ro:pw@a.example.net,b.example.net/two_bot",
            "migrator role": good.replace("two_bot_events_ro", "two_bot_migrator"),
            "runtime role": good.replace("two_bot_events_ro", "two_bot_runtime"),
            "ro migrator": good.replace("two_bot_events_ro", "two_bot_migrator_ro"),
            "no password": "postgresql://two_bot_events_ro@ep-staging-example.neon.tech/two_bot",
            "other database": good.replace("/two_bot", "/neondb"),
            "prod database": good.replace("/two_bot", "/two_bot_prod"),
            "no database": good.replace("/two_bot", ""),
            "prod host": good.replace("ep-staging-example", "ep-prod-example"),
            "production host upper": good.replace("ep-staging-example", "EP-PRODUCTION"),
            "bad port": good.replace("neon.tech", "neon.tech:notaport"),
            "options param": good + "?options=-c%20role%3Dneon_superuser",
            "host override": good + "?host=other.example.net",
            "service param": good + "?service=prod",
            "weak sslmode": good + "?sslmode=disable",
            "prefer sslmode": good + "?sslmode=prefer",
            "bad channel binding": good + "?channel_binding=maybe",
            "bad timeout": good + "?connect_timeout=soon",
            "malformed query": good + "?sslmode",
        }
        for label, url in cases.items():
            with self.subTest(case=label), self.assertRaises(reader.Refused) as caught:
                reader.database_target({} if url is None else {reader.URL_ENV: url})
            self.assertNotIn("pw@", str(caught.exception), "a refusal must not echo the login")


class ReadTests(unittest.TestCase):
    def test_a_read_writes_only_ordinal_type_and_time(self):
        fixture = Fixture(self, rows="member_join,2026-10-07T14:01:00.000Z\n"
                                     "message_created,2026-10-07T14:02:03.250Z\n"
                                     "voice_join,2026-10-07T14:05:00.500Z\n")
        code, out, err = fixture.run()
        self.assertEqual((code, err), (0, ""))
        self.assertEqual(fixture.artifact(), {
            "schema_version": 1,
            "window": {"start": "2026-10-07T14:00:00.000Z", "end": "2026-10-07T14:15:00.000Z"},
            "row_count": 3, "truncated": False,
            "rows": [{"ordinal": 1, "event_type": "member_join", "recorded_at": "2026-10-07T14:01:00.000Z"},
                     {"ordinal": 2, "event_type": "message_created", "recorded_at": "2026-10-07T14:02:03.250Z"},
                     {"ordinal": 3, "event_type": "voice_join", "recorded_at": "2026-10-07T14:05:00.500Z"}]})
        self.assertEqual(reader.artifact_errors(fixture.artifact()), [])
        self.assertEqual(out, "ok: 3 rows, truncated=false\n")

    def test_nothing_identifying_reaches_the_artifact_the_log_or_the_summary(self):
        fixture = Fixture(self, rows=rows_csv(5))
        code, out, err = fixture.run()
        self.assertEqual(code, 0)
        written = fixture.output.read_text() + fixture.summary.read_text() + out + err
        for forbidden in (MEMBER, reader.pins.STAGING_GUILD_ID, PASSWORD, "idempotency", "metadata",
                          "source", "guild", "member", "neon.tech", "two_bot_events_ro"):
            self.assertNotIn(forbidden, written)
        self.assertIn("message_created: 5", fixture.summary.read_text())

    def test_the_sql_selects_only_what_the_artifact_carries_and_is_read_only_and_capped(self):
        fixture = Fixture(self, rows=rows_csv(1))
        fixture.run()
        identity_call, rows_call = fixture.calls()
        for call in (identity_call, rows_call):
            self.assertTrue(call["stdin"].startswith("BEGIN READ ONLY;"))
            self.assertIn("statement_timeout = '10s'", call["stdin"])
            self.assertTrue(call["stdin"].rstrip().endswith("COMMIT;"))
        sql = rows_call["stdin"]
        for forbidden in ("idempotency_key", "metadata", "source", "occurred_at", "SELECT *", "INSERT", "UPDATE"):
            self.assertNotIn(forbidden, sql)
        self.assertIn("FROM events", sql)
        self.assertIn("guild_id = :'guild'", sql)
        self.assertIn("member_id = :'member'", sql)
        self.assertIn("BETWEEN :'window_start'::timestamptz AND :'window_end'::timestamptz", sql)
        self.assertIn("ORDER BY id", sql)
        self.assertIn("LIMIT 61;", sql)

    def test_variables_and_flags_reach_psql_and_no_secret_does(self):
        fixture = Fixture(self, rows=rows_csv(1))
        fixture.run()
        identity_call, rows_call = fixture.calls()
        self.assertEqual(identity_call["argv"], ["-X", "-q", "-t", "--csv", "-v", "ON_ERROR_STOP=1", "-f", "-"])
        self.assertEqual(rows_call["argv"], [
            "-X", "-q", "-t", "--csv", "-v", "ON_ERROR_STOP=1",
            "-v", "guild=1545644954272137297", "-v", f"member={MEMBER}",
            "-v", "window_start=2026-10-07T14:00:00.000Z", "-v", "window_end=2026-10-07T14:15:00.000Z",
            "-f", "-"])
        for call in (identity_call, rows_call):
            joined = " ".join(call["argv"])
            self.assertNotIn(PASSWORD, joined)
            self.assertNotIn("postgres", joined)
            self.assertNotIn(PASSWORD, call["stdin"])
            environ = call["environ"]
            self.assertEqual(environ["PGPASSWORD"], "REDACTED")
            self.assertEqual(environ["PGPASSWORD_SHA256"],
                             hashlib.sha256(PASSWORD.encode()).hexdigest())
            self.assertEqual(environ["PGUSER"], "two_bot_events_ro")
            self.assertEqual(environ["PGDATABASE"], "two_bot")
            self.assertNotIn("GH_TOKEN", environ)
            self.assertNotIn(reader.URL_ENV, environ)
            self.assertNotIn(reader.MEMBER_ENV, environ)
            self.assertNotIn("PGOPTIONS", environ)

    def test_overflow_is_truncated_and_flagged(self):
        for returned, expect_rows, expect_truncated in ((0, 0, False), (60, 60, False), (61, 60, True)):
            with self.subTest(returned=returned):
                fixture = Fixture(self, rows=rows_csv(returned))
                code, out, _ = fixture.run()
                self.assertEqual(code, 0)
                artifact = fixture.artifact()
                self.assertEqual((artifact["row_count"], artifact["truncated"], len(artifact["rows"])),
                                 (expect_rows, expect_truncated, expect_rows))
                self.assertEqual([row["ordinal"] for row in artifact["rows"]], list(range(1, expect_rows + 1)))
                self.assertEqual(reader.artifact_errors(artifact), [])
                self.assertIn(f"truncated={str(expect_truncated).lower()}", out)

    def test_an_unexpected_row_shape_fails_loudly_and_writes_nothing(self):
        bad = {
            "key-looking type": "member_join:1545644954272137297:123456789012345678,2026-10-07T14:01:00.000Z\n",
            "uppercase type": "MemberJoin,2026-10-07T14:01:00.000Z\n",
            "empty type": ",2026-10-07T14:01:00.000Z\n",
            "extra column": "member_join,2026-10-07T14:01:00.000Z,extra\n",
            "single column": "member_join\n",
            "timestamptz text": "member_join,2026-10-07 14:01:00+00\n",
            "no millis": "member_join,2026-10-07T14:01:00Z\n",
            "quoted key": 'member_join,"2026-10-07T14:01:00.000Z"x\n',
        }
        for label, rows in bad.items():
            with self.subTest(case=label):
                fixture = Fixture(self, rows=rows)
                code, out, err = fixture.run()
                self.assertEqual(code, 1)
                self.assertFalse(fixture.output.exists())
                self.assertIn("unexpected shape", err)
                self.assertNotIn("1545644954272137297", err + out)

    def test_more_than_the_query_limit_is_refused_not_trusted(self):
        fixture = Fixture(self, rows=rows_csv(62))
        code, _, err = fixture.run()
        self.assertEqual(code, 1)
        self.assertFalse(fixture.output.exists())
        self.assertIn("query limit", err)

    def test_a_wrong_server_identity_stops_before_the_rows_are_read(self):
        for identity in ("two_bot_migrator,two_bot\n", "two_bot_events_ro,neondb\n", "", "two_bot_events_ro,two_bot\nx,y\n"):
            with self.subTest(identity=identity):
                fixture = Fixture(self, rows=rows_csv(3), identity=identity)
                code, _, err = fixture.run()
                self.assertEqual(code, 2)
                self.assertFalse(fixture.output.exists())
                self.assertEqual(len(fixture.calls()), 1, "the rows query must not run")
                self.assertIn("refusing", err)

    def test_psql_stderr_is_never_echoed(self):
        # The fake builds each stderr from its live process values, so the
        # synthetic credential never sits in behavior.json in clear text.
        for kind, phrase in (("permission", "permission denied for the read-only role"),
                             ("auth", "authentication failed"),
                             ("refused", "could not connect"),
                             ("timeout", "statement timeout"),
                             ("strange", "psql failed")):
            with self.subTest(phrase=phrase):
                fixture = Fixture(self, fail_kind=kind, exit_code=2)
                code, out, err = fixture.run()
                self.assertEqual(code, 1)
                self.assertIn(phrase, err)
                for secret in (MEMBER, PASSWORD, "1545644954272137297", "LINE 5"):
                    self.assertNotIn(secret, out + err)
                self.assertFalse(fixture.output.exists())

    def test_an_unexpected_exception_does_not_print_a_traceback(self):
        fixture = Fixture(self, rows=rows_csv(1))
        with mock.patch.object(reader, "read_events", side_effect=RuntimeError(f"boom {MEMBER} {PASSWORD}")):
            code, out, err = fixture.run()
        self.assertEqual(code, 1)
        self.assertEqual(err, "failed: unexpected RuntimeError; nothing was written\n")
        self.assertFalse(fixture.output.exists())

    def test_the_summary_is_optional(self):
        fixture = Fixture(self, rows=rows_csv(1))
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

    def test_a_bad_window_refuses(self):
        fixture = Fixture(self, rows=rows_csv(1))
        code, _, err = fixture.run(start=END, end=START)
        self.assert_refused(fixture, code, err, "window_end must be after window_start")

    def test_a_missing_member_refuses(self):
        fixture = Fixture(self, rows=rows_csv(1))
        code, _, err = fixture.run(env={reader.MEMBER_ENV: None})
        self.assert_refused(fixture, code, err, "no fixture member bound")

    def test_a_missing_login_refuses(self):
        fixture = Fixture(self, rows=rows_csv(1))
        code, _, err = fixture.run(env={reader.URL_ENV: None})
        self.assert_refused(fixture, code, err, "no database login bound")

    def test_the_wrong_login_refuses(self):
        fixture = Fixture(self, rows=rows_csv(1))
        code, _, err = fixture.run(env={reader.URL_ENV: URL.replace("/two_bot?", "/two_bot_prod?")})
        self.assert_refused(fixture, code, err, "production-like")

    def test_the_live_guild_refuses_before_any_connection(self):
        fixture = Fixture(self, rows=rows_csv(1))
        with mock.patch.object(reader.pins, "STAGING_GUILD_ID", reader.pins.LIVE_GUILD_ID):
            code, _, err = fixture.run()
        self.assert_refused(fixture, code, err, "live guild")

    def test_a_runner_without_psql_refuses(self):
        fixture = Fixture(self, rows=rows_csv(1))
        code, _, err = fixture.run(psql="")
        self.assert_refused(fixture, code, err, "psql is not installed")

    def test_the_member_is_read_from_the_environment_never_a_flag(self):
        with self.assertRaises(SystemExit), redirect_stderr(io.StringIO()):
            reader.main(["--window-start", START, "--window-end", END, "--output", "x.json",
                         "--member", MEMBER], env={}, now=NOW, psql="psql")


class ShapeTests(unittest.TestCase):
    def test_the_artifact_contract_rejects_extra_fields(self):
        good = reader.build_artifact("2026-10-07T14:00:00.000Z", "2026-10-07T14:15:00.000Z",
                                     [{"event_type": "member_join", "recorded_at": "2026-10-07T14:01:00.000Z"}])
        self.assertEqual(reader.artifact_errors(good), [])
        for label, change in {
            "top-level key": lambda a: a.update({"member": MEMBER}),
            "row key": lambda a: a["rows"][0].update({"idempotency_key": "k"}),
            "window key": lambda a: a["window"].update({"guild": "1"}),
            "row count": lambda a: a.update({"row_count": 2}),
            "ordinal": lambda a: a["rows"][0].update({"ordinal": 2}),
            "truncated type": lambda a: a.update({"truncated": "no"}),
            "schema version": lambda a: a.update({"schema_version": 2}),
            "over cap": lambda a: a.update({"rows": a["rows"] * 61, "row_count": 61}),
        }.items():
            with self.subTest(change=label):
                broken = json.loads(json.dumps(good))
                change(broken)
                self.assertTrue(reader.artifact_errors(broken))


class WiringTests(unittest.TestCase):
    """The workflow, the script and the doc must agree on names; text checks only."""

    def test_the_workflow_binds_the_names_the_script_reads(self):
        workflow = (ROOT / ".github/workflows/staging-events-read.yml").read_text()
        self.assertIn(f"{reader.URL_ENV}: ${{{{ secrets.{reader.URL_ENV} }}}}", workflow)
        self.assertIn(f"{reader.MEMBER_ENV}: ${{{{ secrets.{reader.MEMBER_ENV} }}}}", workflow)
        self.assertIn("environment: staging-events-read", workflow)
        self.assertIn("scripts/staging_events_read.py", workflow)

    def test_the_evidence_route_documents_the_ci_route(self):
        doc = (ROOT / "docs/evidence-route.md").read_text()
        for fragment in ("staging-events-read", reader.MEMBER_ENV, reader.URL_ENV,
                         "staging-events-read-<run_id>.json", "two_bot_events_ro"):
            self.assertIn(fragment, doc)


if __name__ == "__main__":
    unittest.main()
