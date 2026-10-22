"""Offline retention-census guards; never touches staging.

The census script may only run pinned read-only aggregates over the
physically read-only plan login, with the staging identity pinned
independently of the secret binding. Every fence refuses before any
connection (exit 2); every read failure prints fixed vocabulary, never
psql's stderr; the report carries counts, digests and timestamps only.
"""

import csv
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import staging_retention_census as census


HOST = "staging-direct.neon.example"
DATABASE = "two_bot"
PASSWORD = "s3cret-fixture-only"
GOOD_URL = f"postgres://two_bot_migrator_ro_plan:{PASSWORD}@{HOST}:5432/{DATABASE}"


def good_env(**overrides):
    env = {census.URL_ENV: GOOD_URL,
           census.HOST_ENV: HOST,
           census.DATABASE_ENV: DATABASE,
           census.BRANCH_ENV: ""}
    env.update(overrides)
    return env


IDENTITY_OUT = "two_bot_migrator_ro,two_bot,f\n"
MIGRATIONS_OUT = ("444,2026-10-19 23:19:00+00,t\n"
                  "445,2026-10-19 23:19:01+00,t\n")
INDEXES_OUT = ('idx_event_store_expiry_class_time_id,'
               '"CREATE INDEX idx_event_store_expiry_class_time_id ON public.event_store '
               'USING btree (event_type, event_time, id)"\n')
COUNTS_OUT = ("GUILD_AUDIT_LOG_ENTRY_CREATE,12,400,412\n"
              "PRESENCE_UPDATE,7,900,907\n"
              "VOICE_SERVER_UPDATE,0,50,50\n"
              "VOICE_STATE_UPDATE,3,120,123\n")
DIGEST = "9f" * 32
ANCHOR = 1760990400
ANCHOR_OUT = f"{ANCHOR}\n"
COHORT_OUT = (f"3,{DIGEST},2026-10-20 20:00:00+00,2026-10-20 20:10:00+00\n")
COHORT_NODIGEST_OUT = "3,2026-10-20 20:00:00+00,2026-10-20 20:10:00+00\n"
EMPTY_COHORT_OUT = "0,,,\n"


def canned_run(outputs, pgcrypto=True):
    """Route canned psql outputs by script marker; record (script, pg_env)."""
    calls = []

    def fake_run(command, input=None, capture_output=None, text=None,
                 env=None, timeout=None, check=None):
        calls.append((input, dict(env)))
        assert command[0].endswith("psql") or command[0] == "psql"
        assert "-f" in command and "-" in command
        assert "ON_ERROR_STOP=1" in command
        if "digest('retention-census-probe'" in input:
            if pgcrypto:
                return subprocess.CompletedProcess(command, 0, "ab12\n", "")
            return subprocess.CompletedProcess(
                command, 1, "",
                "ERROR: function digest(text, unknown) does not exist\n")
        for marker, output in outputs:
            if marker in input:
                return subprocess.CompletedProcess(command, 0, output, "")
        raise AssertionError(f"unexpected psql script: {input[:120]!r}")

    fake_run.calls = calls
    return fake_run


def standard_outputs():
    return [("current_user", IDENTITY_OUT),
            ("_sqlx_migrations", MIGRATIONS_OUT),
            ("pg_indexes", INDEXES_OUT),
            ("eligible_old", COUNTS_OUT),
            ("extract(epoch", ANCHOR_OUT),
            ("cohort_digest", COHORT_OUT)]


class QueryPinTests(unittest.TestCase):
    SCRIPTS = (census.IDENTITY_SQL, census.MIGRATIONS_SQL, census.INDEXES_SQL,
               census.COUNTS_SQL, census.cohort_sql(ANCHOR),
               census.cohort_nodigest_sql(ANCHOR), census.PROBE_SQL,
               census.ANCHOR_SQL)

    def test_every_statement_is_read_only_with_timeouts(self):
        for script in self.SCRIPTS:
            with self.subTest(script=script[:40]):
                self.assertIn("BEGIN READ ONLY", script)
                self.assertIn("statement_timeout", script)
                self.assertIn("lock_timeout", script)
                self.assertTrue(script.rstrip().endswith("COMMIT;"))

    def test_no_select_star_and_no_ddl(self):
        for script in self.SCRIPTS:
            lowered = script.lower()
            self.assertNotIn("select *", lowered)
            for verb in ("create ", "alter ", "drop ", "insert ", "update ",
                         "delete ", "truncate ", "grant ", "revoke ", "copy "):
                self.assertNotIn(verb, lowered)

    def test_counts_carry_no_dead_ctes(self):
        self.assertNotIn("unguarded_queued", census.COUNTS_SQL)
        self.assertNotIn("eligible_pairs", census.COUNTS_SQL)
        self.assertNotIn("LIMIT 0", census.COUNTS_SQL)

    def test_cohort_predicate_is_shared_and_bounded(self):
        predicate = census.cohort_predicate(ANCHOR)
        self.assertIn(predicate, census.cohort_sql(ANCHOR))
        self.assertIn(predicate, census.cohort_nodigest_sql(ANCHOR))
        self.assertIn(f"LIMIT {census.COHORT_LIMIT}", predicate)
        self.assertEqual(census.COHORT_LIMIT, 1000)
        for family in census.FAMILIES:
            self.assertIn(family, predicate)
            self.assertIn(family, census.COUNTS_SQL)

    def test_cohort_digest_casts_the_bigint_id(self):
        # PostgreSQL has no bigint || text operator: the bare concatenation
        # errors (42883) on the BIGSERIAL schema and aborts the census.
        query = census.cohort_sql(ANCHOR)
        self.assertIn("id::text || '|'", query)
        self.assertNotRegex(query, r"(?<!:)id \|\|")

    def test_every_script_asserts_the_read_only_role_first(self):
        # Every psql invocation is its own session: each script must SET ROLE
        # before its transaction, exactly once, or a later script could run
        # as the login instead of the read-only group.
        for script in self.SCRIPTS:
            with self.subTest(script=script[:40]):
                self.assertTrue(script.startswith(census.SET_ROLE))
                self.assertEqual(script.count("SET ROLE"), 1)
        self.assertEqual(census.SET_ROLE, "SET ROLE two_bot_migrator_ro;\n")

    def test_identity_reports_current_user_and_migrator_membership(self):
        # current_user proves SET ROLE took effect; the session_user flag
        # proves the login itself holds no path into the migrator group.
        self.assertIn("SELECT current_user, current_database(),", census.IDENTITY_SQL)
        self.assertIn("pg_has_role(session_user, 'two_bot_migrator', 'MEMBER')",
                      census.IDENTITY_SQL)

    def test_cohort_window_is_frozen_on_one_anchor(self):
        # A now() bound would move ~900 s between the captures and the
        # oldest-1000 cohort would drift with it. Both captures reuse one
        # server-anchored window: the same anchor literal in both bounds,
        # a lower bound past the end of the observed interval, and no now().
        for query in (census.cohort_sql(ANCHOR),
                      census.cohort_nodigest_sql(ANCHOR)):
            self.assertNotIn("now()", query)
            self.assertEqual(query.count(f"to_timestamp({ANCHOR})"), 2)
        predicate = census.cohort_predicate(ANCHOR)
        self.assertIn(f"make_interval(secs => {census.COHORT_MARGIN_SECONDS})",
                      predicate)
        self.assertEqual(census.COHORT_MARGIN_SECONDS,
                         900 + census.SLEEP_SECONDS + 120)
        # floor(), not a bare cast: PostgreSQL rounds a float/numeric epoch,
        # so without the floor the upper bound could postdate the anchor read.
        self.assertIn("floor(extract(epoch FROM now()))::bigint", census.ANCHOR_SQL)
        self.assertNotIn("extract(epoch FROM now())::bigint", census.ANCHOR_SQL)

    def test_cohort_predicate_rejects_non_integer_anchors(self):
        for bad in (True, "1760990400", "1; SELECT", 1.5, None):
            with self.subTest(anchor=bad):
                with self.assertRaises(census.ReadFailed):
                    census.cohort_predicate(bad)

    def test_cutoffs_match_the_implementation(self):
        self.assertIn("make_interval(secs => 900)", census.COUNTS_SQL)
        self.assertIn("make_interval(days => 30)", census.COUNTS_SQL)
        self.assertIn("make_interval(days => 45)", census.COUNTS_SQL)
        self.assertEqual(census.SLEEP_SECONDS, 900)

    def test_migration_ledger_and_index_scope(self):
        # Hardcoded, not derived from the constant: 444/445 are the two
        # expiry migrations in the cutover ledger. 446 lives only in the
        # store chain's _two_bot_migrations ledger (so it could never appear
        # here) and 447 is unrelated; either one in this query is a scope bug.
        self.assertEqual(census.MIGRATION_VERSIONS, (444, 445))
        self.assertIn("_sqlx_migrations", census.MIGRATIONS_SQL)
        self.assertIn("version IN (444, 445)", census.MIGRATIONS_SQL)
        self.assertNotIn("446", census.MIGRATIONS_SQL)
        self.assertNotIn("447", census.MIGRATIONS_SQL)
        for index in (census.CLASS_INDEX, census.TIME_INDEX):
            self.assertIn(index, census.INDEXES_SQL)


class FenceTests(unittest.TestCase):
    def test_good_binding_builds_pg_env_without_echoing_the_url(self):
        pg, host_pin, database_pin = census.database_target(good_env())
        self.assertEqual((host_pin, database_pin), (HOST, DATABASE))
        self.assertEqual(pg["PGHOST"], HOST)
        self.assertEqual(pg["PGUSER"], "two_bot_migrator_ro_plan")
        self.assertEqual(pg["PGPASSWORD"], PASSWORD)
        self.assertEqual(pg["PGDATABASE"], DATABASE)
        self.assertEqual(pg["PGSSLMODE"], "verify-full")
        self.assertEqual(pg["PGSSLROOTCERT"], census.SYSTEM_CA_BUNDLE)
        self.assertNotIn(GOOD_URL, str(pg))

    def test_allowed_url_params_pass_through(self):
        url = GOOD_URL + "?sslmode=require&channel_binding=require&connect_timeout=5"
        pg, _, _ = census.database_target(good_env(**{census.URL_ENV: url}))
        self.assertEqual(pg["PGSSLMODE"], "verify-full")
        self.assertEqual(pg["PGCHANNELBINDING"], "require")
        self.assertEqual(pg["PGCONNECT_TIMEOUT"], "5")

    def test_login_name_is_not_pinned_only_its_shape(self):
        # Mirroring the plan binary: any bare login passes the URL fence and
        # the server-side guards (SET ROLE, current_user, migrator-membership)
        # prove it holds only the read-only group. The group itself is
        # NOLOGIN, so the binding username is never the group name.
        for login in ("two_bot_migrator_ro_plan", "operator_plan_login"):
            url = f"postgres://{login}:{PASSWORD}@{HOST}:5432/{DATABASE}"
            pg, _, _ = census.database_target(good_env(**{census.URL_ENV: url}))
            self.assertEqual(pg["PGUSER"], login)

    def test_refusals(self):
        ps_host = "ep-quiet-tree-12345678.us-east-2.aws.psdb.cloud"
        cases = {
            "missing login": {census.URL_ENV: ""},
            "bad scheme": {census.URL_ENV: GOOD_URL.replace("postgres://", "http://")},
            "multi host": {census.URL_ENV: GOOD_URL.replace(HOST, f"{HOST},other.example")},
            "bad port": {census.URL_ENV: GOOD_URL.replace(":5432/", ":6432/")},
            "pooler host": {census.URL_ENV: GOOD_URL.replace(HOST, "x-pooler.example"),
                             census.HOST_ENV: "x-pooler.example"},
            "pooler-style username": {
                census.URL_ENV: f"postgres://two_bot_migrator_ro_plan|bouncer:{PASSWORD}"
                                f"@{HOST}:5432/{DATABASE}"},
            "empty username": {census.URL_ENV: f"postgres://:{PASSWORD}@{HOST}:5432/{DATABASE}"},
            "host mismatch": {census.HOST_ENV: "other.example"},
            "prod host": {census.HOST_ENV: "prod-db.example"},
            "prod database": {census.DATABASE_ENV: "two_bot_prod"},
            "empty host pin": {census.HOST_ENV: ""},
            "empty database pin": {census.DATABASE_ENV: "  "},
            "no password": {census.URL_ENV: f"postgres://two_bot_migrator_ro_plan@{HOST}:5432/{DATABASE}"},
            "database mismatch": {census.URL_ENV: GOOD_URL.replace(f"/{DATABASE}", "/other_db")},
            "extra query param": {census.URL_ENV: GOOD_URL + "?options=-cstatement_timeout%3D0"},
            "bad sslmode": {census.URL_ENV: GOOD_URL + "?sslmode=disable"},
            "bad connect timeout": {census.URL_ENV: GOOD_URL + "?connect_timeout=soon"},
            "planetscale without branch": {
                census.URL_ENV: f"postgres://two_bot_migrator_ro_plan.pw1234:{PASSWORD}@{ps_host}/{DATABASE}",
                census.HOST_ENV: ps_host},
            "planetscale without login": {
                census.URL_ENV: f"postgres://.pw1234:{PASSWORD}@{ps_host}/{DATABASE}",
                census.HOST_ENV: ps_host, census.BRANCH_ENV: "pw1234"},
            "branch pin on neon": {census.BRANCH_ENV: "pw1234"},
            "malformed branch pin": {
                census.URL_ENV: f"postgres://two_bot_migrator_ro_plan.pw1234:{PASSWORD}@{ps_host}/{DATABASE}",
                census.HOST_ENV: ps_host, census.BRANCH_ENV: "not a pin!"},
        }
        for label, overrides in cases.items():
            with self.subTest(fence=label):
                with self.assertRaises(census.Refused):
                    census.database_target(good_env(**overrides))

    def test_planetscale_branch_pin_match(self):
        ps_host = "ep-quiet-tree-12345678.us-east-2.aws.psdb.cloud"
        url = f"postgres://two_bot_migrator_ro_plan.pw1234:{PASSWORD}@{ps_host}/{DATABASE}"
        pg, host_pin, _ = census.database_target(
            good_env(**{census.URL_ENV: url, census.HOST_ENV: ps_host,
                        census.BRANCH_ENV: "pw1234"}))
        self.assertEqual(host_pin, ps_host)
        self.assertEqual(pg["PGUSER"], "two_bot_migrator_ro_plan.pw1234")
        with self.assertRaises(census.Refused):
            census.database_target(
                good_env(**{census.URL_ENV: url, census.HOST_ENV: ps_host,
                            census.BRANCH_ENV: "other"}))

    def test_psql_inherits_no_caller_secrets(self):
        pg, _, _ = census.database_target(good_env())
        env = {"PATH": "/usr/bin", census.URL_ENV: GOOD_URL,
               "AWS_SECRET_ACCESS_KEY": "caller-secret"}
        merged = census.psql_environment(pg, env)
        self.assertEqual(merged["PATH"], "/usr/bin")
        self.assertEqual(merged["PGPASSWORD"], PASSWORD)
        self.assertNotIn(census.URL_ENV, merged)
        self.assertNotIn("AWS_SECRET_ACCESS_KEY", merged)

    def test_identity_mismatch_refuses(self):
        for output, database in (("two_bot,two_bot,f\n", DATABASE),
                                 (IDENTITY_OUT, "other_db"),
                                 ("two_bot_migrator_ro,two_bot,t\n", DATABASE)):
            with self.subTest(output=output[:30]):
                with self.assertRaises(census.Refused):
                    census.check_identity(output, database)
        census.check_identity(IDENTITY_OUT, DATABASE)  # must not raise

    def test_malformed_identity_fails(self):
        for output in ("two_bot_migrator_ro,two_bot\n",  # missing flag
                       "two_bot_migrator_ro,two_bot,maybe\n",
                       ""):
            with self.subTest(output=output[:30]):
                with self.assertRaises(census.ReadFailed):
                    census.check_identity(output, DATABASE)


class ParseTests(unittest.TestCase):
    def test_migrations_map_absent_versions_to_none(self):
        parsed = census.parse_migrations(MIGRATIONS_OUT)
        self.assertEqual(set(parsed), {444, 445})
        self.assertEqual(parsed[444], ("2026-10-19 23:19:00+00", True))
        self.assertEqual(parsed[445], ("2026-10-19 23:19:01+00", True))
        partial = census.parse_migrations("444,2026-10-19 23:19:00+00,t\n")
        self.assertIsNone(partial[445])

    def test_migration_failures_are_data_not_refusals(self):
        parsed = census.parse_migrations("444,2026-10-19 23:19:00+00,f\n")
        self.assertEqual(parsed[444], ("2026-10-19 23:19:00+00", False))

    def test_malformed_migrations_fail(self):
        for output in ("444,2026-10-19 23:19:00+00\n",  # short row
                       "999,2026-10-19 23:19:00+00,t\n",  # out of scope
                       "446,2026-10-19 23:19:00+00,t\n",  # store ledger, not this one
                       "447,2026-10-19 23:19:00+00,t\n",  # unrelated migration
                       "444,not-a-time,t\n",
                       "444,2026-10-19 23:19:00+00,yes\n",
                       MIGRATIONS_OUT + MIGRATIONS_OUT):  # duplicate
            with self.subTest(output=output[:30]):
                with self.assertRaises(census.ReadFailed):
                    census.parse_migrations(output)

    def test_indexes_report_presence_per_name(self):
        self.assertEqual(census.parse_indexes(INDEXES_OUT),
                         {census.CLASS_INDEX: True, census.TIME_INDEX: False})
        self.assertEqual(census.parse_indexes(""),
                         {census.CLASS_INDEX: False, census.TIME_INDEX: False})

    def test_malformed_indexes_fail(self):
        for output in ("other_index,def\n",
                       f"{census.CLASS_INDEX},\n",  # empty definition
                       "only-one-column\n"):
            with self.subTest(output=output[:30]):
                with self.assertRaises(census.ReadFailed):
                    census.parse_indexes(output)

    def test_counts_zero_fill_absent_families(self):
        parsed = census.parse_counts("PRESENCE_UPDATE,7,900,907\n")
        self.assertEqual(parsed["PRESENCE_UPDATE"], (7, 900, 907))
        self.assertEqual(parsed["VOICE_STATE_UPDATE"], (0, 0, 0))
        self.assertEqual(census.parse_counts(""),
                         {family: (0, 0, 0) for family in census.FAMILIES})

    def test_malformed_counts_fail(self):
        for output in ("PRESENCE_UPDATE,7,900,906\n",  # buckets do not sum
                       "PRESENCE_UPDATE,7,900\n",
                       "OTHER_FAMILY,1,2,3\n",
                       "PRESENCE_UPDATE,-1,900,899\n",
                       "PRESENCE_UPDATE,7,900,907\nPRESENCE_UPDATE,7,900,907\n"):
            with self.subTest(output=output[:30]):
                with self.assertRaises(census.ReadFailed):
                    census.parse_counts(output)

    def test_cohort_parses_digest_and_bounds(self):
        self.assertEqual(census.parse_cohort(COHORT_OUT, True),
                         (3, DIGEST, "2026-10-20 20:00:00+00",
                          "2026-10-20 20:10:00+00"))
        self.assertEqual(census.parse_cohort(COHORT_NODIGEST_OUT, False),
                         (3, None, "2026-10-20 20:00:00+00",
                          "2026-10-20 20:10:00+00"))

    def test_empty_cohort_parses_to_nones(self):
        self.assertEqual(census.parse_cohort(EMPTY_COHORT_OUT, True),
                         (0, None, None, None))

    def test_anchor_parses_one_epoch(self):
        self.assertEqual(census.parse_anchor(ANCHOR_OUT), ANCHOR)
        for output in ("", "1\n2\n", "abc\n", f"{ANCHOR},extra\n"):
            with self.subTest(output=output[:20]):
                with self.assertRaises(census.ReadFailed):
                    census.parse_anchor(output)

    def test_malformed_cohorts_fail(self):
        for output, with_digest in ((COHORT_OUT + COHORT_OUT, True),  # two rows
                                    ("", True),  # no row
                                    (f"3,short,2026-10-20 20:00:00+00,"
                                     "2026-10-20 20:10:00+00\n", True),
                                    ("3,,2026-10-20 20:00:00+00,"
                                     "2026-10-20 20:10:00+00\n", True),
                                    ("0,,,x\n", True),
                                    (COHORT_NODIGEST_OUT, True),  # wrong arity
                                    (COHORT_OUT, False)):
            with self.subTest(output=output[:30]):
                with self.assertRaises(census.ReadFailed):
                    census.parse_cohort(output, with_digest)


class CensusRunTests(unittest.TestCase):
    def run_with(self, outputs, pgcrypto=True, cohorts=(COHORT_OUT, COHORT_OUT)):
        fake = canned_run(outputs, pgcrypto)
        before, after = cohorts
        cohort_calls = {"n": 0}

        def cohort_aware(command, input=None, **kwargs):
            if "cohort_digest" in input or "cohort_min_event_time" in input:
                cohort_calls["n"] += 1
                fake.calls.append((input, dict(kwargs.get("env", {}))))
                output = before if cohort_calls["n"] == 1 else after
                return subprocess.CompletedProcess(command, 0, output, "")
            return fake(command, input=input, **kwargs)

        cohort_aware.calls = fake.calls
        sleeps = []
        clock_values = iter((datetime(2026, 10, 20, 21, 0, 0, tzinfo=timezone.utc),
                             datetime(2026, 10, 20, 21, 15, 5, tzinfo=timezone.utc)))
        stdout = io.StringIO()
        with mock.patch("subprocess.run", cohort_aware):
            with redirect_stdout(stdout):
                report = census.run_census(
                    good_env(), "psql", lambda: next(clock_values),
                    lambda seconds: sleeps.append(seconds))
        return report, stdout.getvalue(), sleeps, cohort_aware.calls

    def test_success_reports_counts_digests_timestamps(self):
        report, printed, sleeps, calls = self.run_with(standard_outputs())
        self.assertEqual(sleeps, [900])
        self.assertIn("sleeping 900s", printed)
        self.assertIn("observed_before: 2026-10-20T21:00:00.000Z", report)
        self.assertIn("observed_after: 2026-10-20T21:15:05.000Z", report)
        self.assertIn("migrations: 444 applied success 2026-10-19 23:19:00+00; "
                        "445 applied success 2026-10-19 23:19:01+00", report)
        self.assertNotIn("446", report)
        self.assertNotIn("447", report)
        self.assertIn(f"{census.CLASS_INDEX} present", report)
        self.assertIn(f"{census.TIME_INDEX} absent", report)
        self.assertIn("PRESENCE_UPDATE: eligible_old=7 fresh=900 total=907", report)
        self.assertIn("pre-exclusion upper bounds", report)
        self.assertIn(f"cohort before: count=3 digest={DIGEST}", report)
        self.assertIn("digest_match: true", report)
        self.assertIn("mode/counters: not observed by this read path", report)
        # Secrets and verdicts never print.
        self.assertNotIn(PASSWORD, report)
        self.assertNotIn(HOST, report)
        self.assertNotIn(GOOD_URL, report)
        self.assertNotIn("PASS", report)
        # One anchor read feeds both captures, which run the identical
        # frozen-window query with no now().
        anchor_scripts = [script for script, _ in calls
                          if "extract(epoch" in script]
        self.assertEqual(anchor_scripts, [census.ANCHOR_SQL])
        cohort_scripts = [script for script, _ in calls
                          if "cohort_min_event_time" in script]
        self.assertEqual(len(cohort_scripts), 2)
        self.assertEqual(cohort_scripts[0], cohort_scripts[1])
        self.assertEqual(cohort_scripts[0], census.cohort_sql(ANCHOR))
        self.assertNotIn("now()", cohort_scripts[0])
        scripts = [script for script, _ in calls]
        self.assertLess(scripts.index(census.ANCHOR_SQL),
                        scripts.index(cohort_scripts[0]))

    def test_digest_mismatch_is_data(self):
        other = COHORT_OUT.replace(DIGEST, "ab" * 32)
        report, _, _, _ = self.run_with(standard_outputs(),
                                        cohorts=(COHORT_OUT, other))
        self.assertIn("digest_match: false", report)

    def test_empty_cohort_is_a_limitation(self):
        report, _, _, _ = self.run_with(standard_outputs(),
                                        cohorts=(EMPTY_COHORT_OUT, EMPTY_COHORT_OUT))
        self.assertIn("limitation: empty cohort; preservation unobserved", report)
        self.assertNotIn("digest_match", report)

    def test_missing_pgcrypto_falls_back_without_digest(self):
        outputs = [("current_user", IDENTITY_OUT),
                   ("_sqlx_migrations", MIGRATIONS_OUT),
                   ("pg_indexes", INDEXES_OUT),
                   ("eligible_old", COUNTS_OUT),
                   ("extract(epoch", ANCHOR_OUT)]
        report, _, _, calls = self.run_with(
            outputs, pgcrypto=False,
            cohorts=(COHORT_NODIGEST_OUT, COHORT_NODIGEST_OUT))
        self.assertIn("limitation: pgcrypto absent", report)
        self.assertNotIn("digest_match", report)
        cohort_scripts = [script for script, _ in calls
                          if "cohort_min_event_time" in script]
        self.assertEqual(cohort_scripts,
                         [census.cohort_nodigest_sql(ANCHOR)] * 2)

    def test_empty_families_are_a_limitation(self):
        outputs = [("current_user", IDENTITY_OUT),
                   ("_sqlx_migrations", MIGRATIONS_OUT),
                   ("pg_indexes", INDEXES_OUT),
                   ("eligible_old", ""),
                   ("extract(epoch", ANCHOR_OUT)]
        report, _, _, _ = self.run_with(outputs)
        self.assertIn("limitation: no rows in the retention families", report)

    def test_psql_stderr_is_never_echoed(self):
        def failing(command, input=None, **kwargs):
            if "current_user" in input:
                return subprocess.CompletedProcess(command, 0, IDENTITY_OUT, "")
            return subprocess.CompletedProcess(
                command, 1, "", f"FATAL: password authentication failed for {PASSWORD}\n")

        with mock.patch("subprocess.run", failing):
            with self.assertRaises(census.ReadFailed) as raised:
                census.run_census(good_env(), "psql",
                                  lambda: datetime.now(timezone.utc),
                                  lambda seconds: None)
        self.assertEqual(str(raised.exception),
                         "authentication failed (psql exit 1)")

    def test_psql_timeout_and_spawn_failure_are_fixed_phrases(self):
        for effect, phrase in ((subprocess.TimeoutExpired("psql", 60), "psql timed out"),
                               (OSError("missing"), "psql could not start")):
            with self.subTest(phrase=phrase):
                with mock.patch("subprocess.run", side_effect=effect):
                    with self.assertRaises(census.ReadFailed) as raised:
                        census.run_census(good_env(), "psql",
                                          lambda: datetime.now(timezone.utc),
                                          lambda seconds: None)
                self.assertEqual(str(raised.exception), phrase)

    def test_probe_failure_other_than_missing_function_fails(self):
        def probe_denied(command, input=None, **kwargs):
            if "current_user" in input:
                return subprocess.CompletedProcess(command, 0, IDENTITY_OUT, "")
            return subprocess.CompletedProcess(command, 1, "",
                                               "ERROR: permission denied\n")

        with mock.patch("subprocess.run", probe_denied):
            with self.assertRaises(census.ReadFailed) as raised:
                census.run_census(good_env(), "psql",
                                  lambda: datetime.now(timezone.utc),
                                  lambda seconds: None)
        self.assertIn("permission denied", str(raised.exception))

    def test_identity_gate_runs_inside_run_census(self):
        # The server-identity check is the fence proving the connection
        # landed on the pinned role and database with a read-only login.
        # Deleting the check_identity call (or neutering it) must turn this
        # red: a wrong identity refuses before the migrations query runs.
        for output in ("two_bot,two_bot,f\n",  # SET ROLE did not take effect
                       "two_bot_migrator_ro,other_db,f\n",  # wrong database
                       "two_bot_migrator_ro,two_bot,t\n"):  # migrator member
            with self.subTest(output=output[:30]):
                fake = canned_run([("current_user", output)])
                with mock.patch("subprocess.run", fake):
                    with self.assertRaises(census.Refused):
                        census.run_census(good_env(), "psql",
                                          lambda: datetime.now(timezone.utc),
                                          lambda seconds: None)
                self.assertEqual([script for script, _ in fake.calls],
                                 [census.IDENTITY_SQL])

    def test_set_role_denial_refuses(self):
        # A login that is not a member of the read-only group fails on the
        # SET ROLE line of the identity script: that is a credential-shape
        # refusal (like the plan binary's set-role-denied cause), not a
        # read failure, and nothing further is issued.
        calls = []

        def set_role_denied(command, input=None, **kwargs):
            calls.append(input)
            return subprocess.CompletedProcess(
                command, 3, "",
                "ERROR: permission denied to set role \"two_bot_migrator_ro\"\n")

        with mock.patch("subprocess.run", set_role_denied):
            with self.assertRaises(census.Refused) as raised:
                census.run_census(good_env(), "psql",
                                  lambda: datetime.now(timezone.utc),
                                  lambda seconds: None)
        self.assertIn("SET ROLE denied", str(raised.exception))
        self.assertEqual(calls, [census.IDENTITY_SQL])


class MainTests(unittest.TestCase):
    def test_success_writes_summary_and_prints_ok(self):
        fake = canned_run(standard_outputs())
        clock_values = iter((datetime(2026, 10, 20, 21, 0, 0, tzinfo=timezone.utc),
                             datetime(2026, 10, 20, 21, 15, 5, tzinfo=timezone.utc)))
        with tempfile.TemporaryDirectory() as tmp:
            summary = Path(tmp) / "summary.md"
            env = good_env(**{"GITHUB_STEP_SUMMARY": str(summary)})
            stdout, stderr = io.StringIO(), io.StringIO()
            with mock.patch("subprocess.run", fake):
                with redirect_stdout(stdout):
                    with mock.patch.object(sys, "stderr", stderr):
                        status = census.main(
                            [], env=env, clock=lambda: next(clock_values),
                            psql="psql", sleep_fn=lambda seconds: None)
            body = summary.read_text(encoding="utf-8")
        self.assertEqual(status, 0)
        self.assertIn("ok: census complete", stdout.getvalue())
        self.assertEqual(stderr.getvalue(), "")
        self.assertIn("## staging retention census", body)
        self.assertNotIn(PASSWORD, body)

    def test_refusal_exits_2_with_fixed_text(self):
        stdout, stderr = io.StringIO(), io.StringIO()
        with redirect_stdout(stdout):
            with mock.patch.object(sys, "stderr", stderr):
                status = census.main([], env=good_env(**{census.URL_ENV: ""}),
                                     clock=lambda: datetime.now(timezone.utc),
                                     psql="psql", sleep_fn=lambda seconds: None)
        self.assertEqual(status, 2)
        self.assertIn("refusing: no database login bound", stderr.getvalue())
        self.assertEqual(stdout.getvalue(), "")

    def test_unexpected_errors_stay_fixed_vocabulary(self):
        with mock.patch.object(census, "run_census",
                               side_effect=ValueError(f"boom {PASSWORD}")):
            stdout, stderr = io.StringIO(), io.StringIO()
            with redirect_stdout(stdout):
                with mock.patch.object(sys, "stderr", stderr):
                    status = census.main(
                        [], env=good_env(),
                        clock=lambda: datetime.now(timezone.utc),
                        psql="psql", sleep_fn=lambda seconds: None)
        self.assertEqual(status, 1)
        self.assertEqual(stderr.getvalue(),
                         "failed: unexpected ValueError; nothing was reported\n")
        self.assertEqual(stdout.getvalue(), "")

    def test_unknown_flags_are_rejected(self):
        with self.assertRaises(SystemExit):
            census.main(["--mode", "execute"], env=good_env(),
                        clock=lambda: datetime.now(timezone.utc),
                        psql="psql", sleep_fn=lambda seconds: None)


def csv_observation(*fields):
    """Quote hostile timestamp fields too: rejection must not depend on CSV arity."""
    output = io.StringIO(newline="")
    csv.writer(output, lineterminator="\n").writerow(fields)
    return output.getvalue()


def cohort_observation(count, minimum, maximum, with_digest, digest=DIGEST):
    fields = [count, minimum, maximum]
    if with_digest:
        fields.insert(1, digest)
    return csv_observation(*fields)


class ObservationAdmissionAcceptanceTests(unittest.TestCase):
    # These exercise the existing public parsers, not a replacement validator.
    # Prefix-only parsing, lexical comparison or dropping either bound check
    # must fail even when the CSV row has the expected shape.
    def test_supported_postgres_timestamps_are_canonicalized_in_every_position(self):
        cases = (
            ("2024-02-29 23:59:59+00", "2024-02-29 23:59:59+00"),
            ("2024-03-01 05:29:59+05:30", "2024-02-29 23:59:59+00"),
            ("2024-02-29 20:29:59-0330", "2024-02-29 23:59:59+00"),
            ("2024-03-01 00:59:59+01", "2024-02-29 23:59:59+00"),
            ("2024-02-29 23:59:59Z", "2024-02-29 23:59:59+00"),
            ("2024-02-29 23:59:59.1+00", "2024-02-29 23:59:59.100000+00"),
            ("2024-03-01 05:29:59.123456+0530",
             "2024-02-29 23:59:59.123456+00"),
            ("2024-03-01 00:00:14+00:00:15", "2024-02-29 23:59:59+00"),
        )
        for raw, canonical in cases:
            with self.subTest(timestamp=raw, parser="migration"):
                self.assertEqual(
                    census.parse_migrations(csv_observation(444, raw, "t"))[444],
                    (canonical, True))
            for with_digest in (True, False):
                with self.subTest(timestamp=raw, digest=with_digest):
                    parsed = census.parse_cohort(cohort_observation(
                        1, raw, raw, with_digest), with_digest)
                    self.assertEqual(parsed, (1, DIGEST if with_digest else None,
                                              canonical, canonical))

    def test_impossible_and_tainted_timestamps_are_rejected_in_every_position(self):
        invalid = (
            "2026-99-99 99:99:99+00", "2026-02-29 12:00:00+00",
            "1900-02-29 12:00:00+00", "2024-04-31 12:00:00+00",
            "2024-00-01 12:00:00+00", "2024-01-00 12:00:00+00",
            "2024-01-01 24:00:01+00", "2024-01-01 12:60:00+00",
            "2024-01-01 12:00:60+00", "2024-01-01 12:00:00+24",
            "2024-01-01 12:00:00+16:00", "2024-01-01 12:00:00+01:60",
            "2024-01-01 12:00:00+00:00:60", "2024-01-01 12:00:00",
            "2024-01-01 12:00:00+00 ATTACK_SENTINEL",
            "2024-01-01 12:00:00+00\x00ATTACK_SENTINEL",
            "2024-01-01 12:00:00+00\rATTACK_SENTINEL",
            "2024-01-01 12:00:00+00\nATTACK_SENTINEL",
            "2024-01-01 12:00:00+00\x1b[31mATTACK_SENTINEL",
            "2024-01-01 12:00:00+00 ", "infinity", "-infinity", "epoch",
        )
        for raw in invalid:
            with self.subTest(timestamp=repr(raw), parser="migration"):
                with self.assertRaises(census.ReadFailed):
                    census.parse_migrations(csv_observation(444, raw, "t"))
            for with_digest in (True, False):
                for position in (0, 1):
                    bounds = ["2024-01-01 12:00:00+00"] * 2
                    bounds[position] = raw
                    with self.subTest(timestamp=repr(raw), digest=with_digest,
                                      position=position):
                        with self.assertRaises(census.ReadFailed):
                            census.parse_cohort(cohort_observation(
                                1, *bounds, with_digest), with_digest)

    def test_cohort_cardinality_boundaries_preserve_empty_shape(self):
        for with_digest in (True, False):
            for count in (1, 1000):
                with self.subTest(count=count, digest=with_digest):
                    parsed = census.parse_cohort(cohort_observation(
                        count, "2024-02-29 12:00:00+00",
                        "2024-02-29 12:00:00+00", with_digest), with_digest)
                    self.assertEqual(parsed[0], count)
            with self.subTest(count=0, digest=with_digest):
                self.assertEqual(census.parse_cohort(cohort_observation(
                    0, "", "", with_digest, digest=""), with_digest),
                    (0, None, None, None))
            for count in (-1, 1001):
                with self.subTest(count=count, digest=with_digest):
                    with self.assertRaises(census.ReadFailed):
                        census.parse_cohort(cohort_observation(
                            count, "2024-02-29 12:00:00+00",
                            "2024-02-29 12:00:00+00", with_digest), with_digest)

    def test_bounds_compare_instants_not_wall_time_or_offset_spelling(self):
        cases = (
            # Equal instants, despite the first local clock sorting later.
            ("2024-03-01 01:00:00+01", "2024-03-01 00:00:00+00",
             "2024-03-01 00:00:00+00", "2024-03-01 00:00:00+00"),
            ("2024-02-29 23:00:00-01", "2024-03-01 00:00:00+00",
             "2024-03-01 00:00:00+00", "2024-03-01 00:00:00+00"),
            ("2024-03-01 01:00:00+02", "2024-03-01 00:00:00+00",
             "2024-02-29 23:00:00+00", "2024-03-01 00:00:00+00"),
        )
        for with_digest in (True, False):
            for minimum, maximum, want_min, want_max in cases:
                with self.subTest(minimum=minimum, digest=with_digest):
                    parsed = census.parse_cohort(cohort_observation(
                        1, minimum, maximum, with_digest), with_digest)
                    self.assertEqual(parsed[2:], (want_min, want_max))
            for minimum, maximum in (
                ("2024-03-01 00:00:01+00", "2024-03-01 00:00:00+00"),
                # Local clocks sort forward, but UTC instants are reversed.
                ("2024-03-01 00:00:00-01", "2024-03-01 00:30:00+00"),
                ("2024-03-01 00:00:00.000002+00",
                 "2024-03-01 00:00:00.000001+00"),
            ):
                with self.subTest(minimum=minimum, digest=with_digest):
                    with self.assertRaises(census.ReadFailed):
                        census.parse_cohort(cohort_observation(
                            1, minimum, maximum, with_digest), with_digest)

    def test_empty_and_nonempty_digest_shapes_cannot_be_reinterpreted(self):
        for with_digest in (True, False):
            for minimum, maximum in (("", "2024-02-29 12:00:00+00"),
                                     ("2024-02-29 12:00:00+00", ""),
                                     ("2024-02-29 12:00:00+00",) * 2):
                with self.subTest(digest=with_digest, empty_bounds=(minimum, maximum)):
                    with self.assertRaises(census.ReadFailed):
                        census.parse_cohort(cohort_observation(
                            0, minimum, maximum, with_digest, digest=""), with_digest)
            with self.subTest(digest=with_digest, nonempty_null_bounds=True):
                with self.assertRaises(census.ReadFailed):
                    census.parse_cohort(cohort_observation(
                        1, "", "", with_digest), with_digest)
        for digest in ("", "aa" * 31, "gg" * 32, DIGEST + "ATTACK_SENTINEL"):
            with self.subTest(digest=digest):
                with self.assertRaises(census.ReadFailed):
                    census.parse_cohort(cohort_observation(
                        1, "2024-02-29 12:00:00+00", "2024-02-29 12:00:00+00",
                        True, digest=digest), True)
        with self.assertRaises(census.ReadFailed):
            census.parse_cohort(cohort_observation(0, "", "", True), True)


class ObservationPublicationAcceptanceTests(unittest.TestCase):
    """Agent wire contract: --json emits one v1 document, never progress prose.

    Top-level allowlist: schema_version, status, observations, limitations, error.
    Observation records: id/code/value/next_action; limitation records omit value.
    Error records: code/retryable/next_action, with no raw reason/details field.
    Stable IDs below are shared with text/summary, not internal Python type names.
    Canonical DB instants use UTC 'YYYY-MM-DD HH:MM:SS[.ffffff]+00', preserving
    already-safe legacy spelling; capture clocks retain millisecond RFC3339.

    Offline command: python3 -m unittest discover -s scripts/ci \
        -p 'test_staging_retention_census.py' -v
    """

    def invoke(self, *, json_mode=False, pgcrypto=True, cohorts=None,
               overrides=None, read_error=None, outputs=None):
        fake = canned_run(standard_outputs() if outputs is None else outputs, pgcrypto)
        if cohorts is None:
            row = COHORT_OUT if pgcrypto else COHORT_NODIGEST_OUT
            cohorts = (row, row)
        captures = iter(cohorts)
        clocks = iter((datetime(2026, 10, 20, 21, 0, tzinfo=timezone.utc),
                       datetime(2026, 10, 20, 21, 15, 5, tzinfo=timezone.utc)))
        calls, sleeps = [], []

        def psql(command, input=None, **kwargs):
            calls.append(input)
            if read_error is not None:
                raise read_error
            if "cohort_min_event_time" in input:
                return subprocess.CompletedProcess(command, 0, next(captures), "")
            return fake(command, input=input, **kwargs)

        stdout, stderr = io.StringIO(), io.StringIO()
        with (mock.patch("subprocess.run", psql),
              mock.patch("subprocess.Popen", side_effect=AssertionError("real process")),
              mock.patch("socket.create_connection", side_effect=AssertionError("network")),
              mock.patch("socket.socket", side_effect=AssertionError("network")),
              mock.patch("time.sleep", side_effect=AssertionError("real sleep")),
              redirect_stdout(stdout), mock.patch.object(sys, "stderr", stderr)):
            try:
                status = census.main(
                    ["--json"] if json_mode else [], env=good_env(**(overrides or {})),
                    psql="psql", clock=lambda: next(clocks), sleep_fn=sleeps.append)
            except SystemExit as error:
                self.fail(f"documented output mode was rejected (exit {error.code})")
            except OSError:
                self.fail("publication failure escaped the protected CLI boundary")
        return status, stdout.getvalue(), stderr.getvalue(), calls, sleeps

    def document(self, invocation):
        status, stdout, stderr, calls, sleeps = invocation
        try:
            document = json.loads(stdout)
        except json.JSONDecodeError:
            self.fail("--json must publish exactly one JSON document, no progress prose")
        self.assertEqual(set(document),
                         {"schema_version", "status", "observations", "limitations", "error"})
        self.assertEqual(document["schema_version"], 1)
        self.assertIsInstance(document["observations"], list)
        self.assertIsInstance(document["limitations"], list)
        ids = []
        for record in document["observations"] + document["limitations"]:
            fields = {"id", "code", "next_action"}
            if record in document["observations"]:
                fields.add("value")
            self.assertEqual(set(record), fields)
            for key in ("id", "code"):
                self.assertRegex(record[key], r"^[a-z][a-z0-9_]*$")
            self.assertIsInstance(record["next_action"], str)
            self.assertTrue(record["next_action"].strip())
            ids.append(record["id"])
        self.assertEqual(len(ids), len(set(ids)), "observation IDs must be unique")
        return document

    def test_json_is_typed_allowlisted_and_text_summary_is_the_same_projection(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            summary = Path(tmp) / "summary.md"
            invocation = self.invoke(json_mode=True,
                                     overrides={"GITHUB_STEP_SUMMARY": str(summary)})
            self.assertEqual(invocation[0], 0)
            self.assertEqual(invocation[2], "")
            document = self.document(invocation)
            summary_text = summary.read_text(encoding="utf-8")
            text_run = self.invoke()
        self.assertEqual(text_run[0], 0)
        self.assertEqual(document["status"], "observed")
        self.assertIsNone(document["error"])
        records = {record["id"]: record for record in document["observations"]}
        self.assertEqual(set(records), {"observed_before", "observed_after", "migrations",
                                      "indexes", "counts", "cohort_before", "cohort_after",
                                      "digest_match", "mode", "counters"})
        expected = {
            "observed_before": "2026-10-20T21:00:00.000Z",
            "observed_after": "2026-10-20T21:15:05.000Z",
            "migrations": {
                "444": {"installed_on": "2026-10-19 23:19:00+00", "success": True},
                "445": {"installed_on": "2026-10-19 23:19:01+00", "success": True}},
            "indexes": {"idx_event_store_expiry_class_time_id": True,
                        "idx_event_store_expiry_event_time_id": False},
            "counts": {
                "GUILD_AUDIT_LOG_ENTRY_CREATE": {"eligible_old": 12, "fresh": 400, "total": 412},
                "PRESENCE_UPDATE": {"eligible_old": 7, "fresh": 900, "total": 907},
                "VOICE_SERVER_UPDATE": {"eligible_old": 0, "fresh": 50, "total": 50},
                "VOICE_STATE_UPDATE": {"eligible_old": 3, "fresh": 120, "total": 123}},
            "cohort_before": {"count": 3, "digest": DIGEST,
                              "minimum": "2026-10-20 20:00:00+00",
                              "maximum": "2026-10-20 20:10:00+00"},
            "cohort_after": {"count": 3, "digest": DIGEST,
                             "minimum": "2026-10-20 20:00:00+00",
                             "maximum": "2026-10-20 20:10:00+00"},
            "digest_match": True, "mode": None, "counters": None,
        }
        self.assertEqual({key: record["value"] for key, record in records.items()}, expected)
        for key, record in records.items():
            self.assertEqual(record["code"], "unobserved" if key in ("mode", "counters")
                             else "observed")
            # An ID's code/remediation must occur on its own record line,
            # not merely somewhere in an unrelated observation's prose.
            for rendered in (summary_text, text_run[1]):
                lines = [line for line in rendered.splitlines()
                         if re.search(r"\b" + re.escape(record["id"]) + r"\b", line)]
                self.assertTrue(any(record["code"] in line and
                                    record["next_action"] in line for line in lines),
                                f"missing shared projection for {record['id']}")
        for rendered in (summary_text, text_run[1]):
            self.assertIn("PRESENCE_UPDATE: eligible_old=7 fresh=900 total=907", rendered)
            self.assertIn(f"cohort before: count=3 digest={DIGEST}", rendered)
            self.assertIn("digest_match: true", rendered)
            self.assertNotIn("(3,", rendered, "raw tuple repr is not an agent contract")
        self.assertEqual(self.document(self.invoke(json_mode=True)), document)

    def test_empty_and_missing_crypto_are_limitations_not_preservation_pass(self):
        for crypto, rows, limitation in (
            (True, (EMPTY_COHORT_OUT,) * 2, "empty_cohort"),
            (False, (COHORT_NODIGEST_OUT,) * 2, "pgcrypto_absent"),
            (False, ("0,,\n",) * 2, "empty_cohort"),
        ):
            with self.subTest(crypto=crypto, limitation=limitation):
                invocation = self.invoke(json_mode=True, pgcrypto=crypto, cohorts=rows)
                self.assertEqual(invocation[0], 0)
                document = self.document(invocation)
                self.assertEqual(document["status"], "observed")
                codes = {row["code"] for row in document["limitations"]}
                self.assertIn(limitation, codes)
                if not crypto:
                    self.assertIn("pgcrypto_absent", codes)
                records = {row["id"]: row for row in document["observations"]}
                self.assertNotIn("digest_match", records)
                for key in ("mode", "counters"):
                    self.assertEqual(records[key]["code"], "unobserved")
                    self.assertIsNone(records[key]["value"])
                for text in (invocation[1], self.invoke(pgcrypto=crypto, cohorts=rows)[1]):
                    self.assertNotRegex(text.lower(), r"\bpass\b|retention succeeded|preservation proven")
                    self.assertIn("unobserved", text)

    def test_absent_families_and_migrations_remain_explicit_without_invented_success(self):
        outputs = [(marker, "" if marker in ("eligible_old", "_sqlx_migrations") else output)
                   for marker, output in standard_outputs()]
        invocation = self.invoke(json_mode=True, outputs=outputs)
        self.assertEqual(invocation[0], 0)
        document = self.document(invocation)
        self.assertIn("empty_families", {row["code"] for row in document["limitations"]})
        records = {row["id"]: row for row in document["observations"]}
        self.assertEqual(records["migrations"]["value"], {"444": None, "445": None})
        self.assertEqual(records["counts"]["value"], {
            "GUILD_AUDIT_LOG_ENTRY_CREATE": {"eligible_old": 0, "fresh": 0, "total": 0},
            "PRESENCE_UPDATE": {"eligible_old": 0, "fresh": 0, "total": 0},
            "VOICE_SERVER_UPDATE": {"eligible_old": 0, "fresh": 0, "total": 0},
            "VOICE_STATE_UPDATE": {"eligible_old": 0, "fresh": 0, "total": 0}})
        outputs = [(marker, "444,2026-10-19 23:19:00+00,f\n"
                    if marker == "_sqlx_migrations" else output)
                   for marker, output in standard_outputs()]
        invocation = self.invoke(json_mode=True, outputs=outputs)
        self.assertEqual(invocation[0], 0)
        document = self.document(invocation)
        self.assertEqual(document["status"], "observed")
        records = {row["id"]: row for row in document["observations"]}
        self.assertIs(records["migrations"]["value"]["444"]["success"], False)

    def test_first_capture_failure_does_not_sleep_or_modify_existing_summary(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            summary = Path(tmp) / "summary.md"
            summary.write_text("unrelated prior step\n", encoding="utf-8")
            for crypto in (True, False):
                bad = cohort_observation(1001, "2024-02-29 12:00:00+00",
                                         "2024-02-29 12:00:00+00", crypto)
                with self.subTest(crypto=crypto):
                    invocation = self.invoke(pgcrypto=crypto, cohorts=(bad,),
                                             overrides={"GITHUB_STEP_SUMMARY": str(summary)})
                    self.assertEqual(invocation[0], 1)
                    self.assertEqual(invocation[4], [])
                    self.assertNotIn("ok: census complete", invocation[1])
                    self.assertEqual(summary.read_text(encoding="utf-8"), "unrelated prior step\n")

    def test_digest_mismatch_remains_observed_data_not_a_read_failure(self):
        rows = (COHORT_OUT, COHORT_OUT.replace(DIGEST, "ab" * 32))
        invocation = self.invoke(json_mode=True, cohorts=rows)
        self.assertEqual(invocation[0], 0)
        document = self.document(invocation)
        self.assertEqual(document["status"], "observed")
        self.assertIsNone(document["error"])
        records = {row["id"]: row for row in document["observations"]}
        self.assertIs(records["digest_match"]["value"], False)
        self.assertEqual(records["digest_match"]["code"], "observed")
        self.assertIn("digest_match: false", self.invoke(cohorts=rows)[1])
        self.assertNotIn("PASS", invocation[1])

    def test_invalid_second_capture_never_publishes_a_partial_observation(self):
        for crypto in (True, False):
            good = COHORT_OUT if crypto else COHORT_NODIGEST_OUT
            for bad in (
                cohort_observation(1001, "2024-02-29 12:00:00+00",
                                   "2024-02-29 12:01:00+00", crypto),
                cohort_observation(1, "2024-02-29 12:01:00+00",
                                   "2024-02-29 12:00:00+00", crypto),
                cohort_observation(1, "2026-99-99 99:99:99+00",
                                   "2026-99-99 99:99:99+00", crypto),
            ):
                with self.subTest(crypto=crypto, row=bad):
                    invocation = self.invoke(pgcrypto=crypto, cohorts=(good, bad))
                    self.assertEqual(invocation[0], 1)
                    self.assertNotIn("ok: census complete", invocation[1])
                    self.assertNotIn("cohort before:", invocation[1])
                    document = self.document(self.invoke(
                        json_mode=True, pgcrypto=crypto, cohorts=(good, bad)))
                    self.assertEqual(document["status"], "read_failed")
                    self.assertEqual(document["observations"], [])
                    self.assertEqual(document["error"]["code"], "read_failed")

    def test_refusal_read_and_publication_failures_have_separate_safe_codes(self):
        sentinel = f"PRIVATE_OS_SENTINEL-{PASSWORD}-{HOST}"
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            # A real missing parent exercises publication, not a mocked renderer.
            summary = str(Path(tmp) / sentinel / "summary.md")
            cases = (
                (2, "refused", {"overrides": {census.URL_ENV: ""}}),
                (1, "read_failed", {"read_error": OSError(sentinel)}),
                (1, "read_failed", {"read_error": subprocess.TimeoutExpired(sentinel, 60)}),
                (1, "publication_failed", {"overrides": {"GITHUB_STEP_SUMMARY": summary}}),
            )
            for expected_status, code, kwargs in cases:
                for json_mode in (False, True):
                    with self.subTest(code=code, json_mode=json_mode):
                        invocation = self.invoke(json_mode=json_mode, **kwargs)
                        self.assertEqual(invocation[0], expected_status)
                        combined = invocation[1] + invocation[2]
                        for forbidden in (sentinel, PASSWORD, HOST, GOOD_URL, "Traceback",
                                          "FileNotFoundError", "No such file or directory"):
                            self.assertNotIn(forbidden, combined)
                        self.assertNotIn("ok: census complete", combined)
                        if json_mode:
                            document = self.document(invocation)
                            self.assertEqual(document["status"], code)
                            self.assertEqual(document["observations"], [])
                            error = document["error"]
                            self.assertEqual(set(error), {"code", "retryable", "next_action"})
                            self.assertEqual(error["code"], code)
                            self.assertIsInstance(error["retryable"], bool)
                            self.assertIsInstance(error["next_action"], str)
                            self.assertTrue(error["next_action"].strip())
                        else:
                            self.assertIn(code, combined)
                            self.assertIn("next_action", combined)

    def test_canonical_instants_not_input_suffixes_reach_text_or_json(self):
        raw = "2024-03-01 05:29:59.123456+05:30"
        row = cohort_observation(1, raw, raw, True)
        outputs = [(marker, csv_observation(444, raw, "t")
                    if marker == "_sqlx_migrations" else output)
                   for marker, output in standard_outputs()]
        for json_mode in (False, True):
            with self.subTest(json_mode=json_mode):
                invocation = self.invoke(json_mode=json_mode, outputs=outputs, cohorts=(row, row))
                self.assertEqual(invocation[0], 0)
                self.assertNotIn(raw, invocation[1])
                self.assertIn("2024-02-29 23:59:59.123456+00", invocation[1])

    def test_capture_scripts_are_byte_frozen_in_both_variants_without_real_sleep(self):
        expected = {
            "identity": "95912c0414bc720b6c61e2327b8aa9dc1818df3639473bf2238cad74e350c46b",
            "migrations": "df2acaab5b4b682a26d8666da89c1b7ecec2cf3eb05f8156ddaea05babec6d44",
            "indexes": "bb9815f58a3469ad076cb5e281ae3c0e17df07851c83e8c6119424c187648eff",
            "counts": "c9922344342c3164f0c50a1603330462669634cb9d5280b73a9e285bdc4258da",
            "probe": "26ba45917d31173fcff38f48e5538f66c075a2ffd6ac7878e95700457fc1a0fc",
            "anchor": "caacf43a797c2d2cd6c7b935a0719cbfba65e27e664bcf402b3d680d4af5079a",
            "cohort": "730616f136fa20924cdf9f07df2080622694c85c6a698987a6a5e16abf4bedd0",
            "cohort_nodigest": "2c66de6a4d65109d0e23951f1eba4b012e38f1c77e73da0b51262efd82101b14",
        }
        for crypto in (True, False):
            with self.subTest(crypto=crypto):
                invocation = self.invoke(pgcrypto=crypto)
                self.assertEqual(invocation[0], 0)
                self.assertEqual(invocation[4], [900])
                scripts = invocation[3]
                cohorts = [script for script in scripts if "cohort_min_event_time" in script]
                self.assertEqual(len(cohorts), 2)
                self.assertEqual(cohorts[0], cohorts[1])
                actual = [hashlib.sha256(script.encode()).hexdigest() for script in scripts]
                cohort_key = "cohort" if crypto else "cohort_nodigest"
                self.assertCountEqual(actual, [expected[key] for key in
                                      ("identity", "migrations", "indexes", "counts",
                                       "probe", "anchor", cohort_key, cohort_key)])


if __name__ == "__main__":
    unittest.main()
