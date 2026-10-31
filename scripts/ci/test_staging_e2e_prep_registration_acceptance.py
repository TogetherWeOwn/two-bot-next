"""Protected offline registration acceptance for staging_guild_e2e_prep (E1).

Port of the merged automation-copy admission contract to the e2e-prep copy:
admit every targeted entry before the first resource read (exactly-one
name match, canonical snowflake id), bind the detail id back to the
admitted entry, and never project hostile versions into reason text or
receipts. Valid registrations render byte-identically to today.

Only transport is faked; parsing, admission, grading, stdout and artifact
writes are real. A removed admission/identity/projection check must break
these tests. This file is protected: the builder must not edit it.

Offline only: no network, no tokens. Run:
  PYTHONDONTWRITEBYTECODE=1 python3 -m unittest scripts.ci.test_staging_e2e_prep_registration_acceptance -v
from the repo root, or:
  PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_staging_e2e_prep_registration_acceptance.py' -v
"""

import copy
import importlib
import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
prep = importlib.import_module("staging_guild_e2e_prep")

API = "https://discord.com/api/v10"
GUILD = "1545644954272137297"
APP = "1469137636663758888"
OTHER_GUILD = "9999999999999999999"
IDENTITY = API + "/users/@me"
LIST = API + "/applications/" + APP + "/guilds/" + GUILD + "/commands"
TOKEN = "fixture-bot-token-not-a-real-credential"
SENTINEL = "REMOTE_REGISTRATION_SENTINEL"
MISSING = object()

NAMES = ("rank", "leaderboard")
IDS = ("1549621908675366953", "2000000000000000005")
VERSIONS = ("1000000000000000001", "1000000000000000005")


def commands():
    return [{"id": command_id, "name": name, "type": 1,
             "guild_id": GUILD, "version": version}
            for name, command_id, version in zip(NAMES, IDS, VERSIONS)]


def body(value):
    return 200, json.dumps(value).encode()


def is_snowflake(value):
    return (isinstance(value, str) and len(value) >= 1 and len(value) <= 20
            and value[0] != "0" and value.isascii() and value.isdigit()
            and int(value) <= 18446744073709551615)


class E2EPrepRegistrationAcceptanceTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)

    def routes(self, rows=None):
        rows = commands() if rows is None else rows
        routes = {IDENTITY: body({"id": APP, "username": "offline-fixture"}),
                  LIST: body(rows)}
        for row in rows:
            routes[LIST + "/" + str(row.get("id"))] = body(row)
        return routes

    def run_cli(self, routes, guild=GUILD):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get(
            "PAPERCLIP_SCRATCH_DIR")
        tmp = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(tmp.cleanup)
        evidence = Path(tmp.name) / "e2e-prep-registration.json"
        self.requested = []

        def fetch(url):
            self.requested.append(url)
            if url in routes:
                return routes[url]
            # Let malformed detail URLs reach a successful fake response so
            # zero-request assertions detect URL admission, not a routing error.
            if url.startswith(LIST + "/"):
                return body(commands()[0])
            raise AssertionError("unexpected offline request")

        def counting(url):
            return fetch(url)

        env = {"DISCORD_STAGING_BOT_TOKEN": TOKEN,
               "DISCORD_STAGING_GUILD_ID": "env-guild-never-used-by-flag-runs"}
        with mock.patch.dict(os.environ, env, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = prep.main(["--guild-id", guild, "--mock",
                                  "--evidence", str(evidence)],
                                 fetch_fn=counting)
        artifact = evidence.read_text() if evidence.exists() else ""
        receipt = json.loads(artifact) if artifact else None
        self.assertNotIn(TOKEN, out.getvalue())
        self.assertNotIn(TOKEN, artifact)
        return code, out.getvalue(), artifact, receipt

    def assert_refused(self, result, target="rank", sentinel=SENTINEL):
        code, out, artifact, receipt = result
        with self.subTest(property="non-PASS exit"):
            self.assertEqual(code, 1, out)
        with self.subTest(property="no remote stdout echo"):
            self.assertNotIn(sentinel, out)
        with self.subTest(property="no remote artifact echo"):
            self.assertNotIn(sentinel, artifact)
        with self.subTest(property="no command PASS"):
            self.assertNotIn("PASS " + target + ":", out)
        with self.subTest(property="target is FAIL"):
            self.assertIn("FAIL " + target + ":", out)
        with self.subTest(property="receipt non-PASS"):
            if receipt is not None:
                self.assertEqual(receipt["result"], "fail")
                rows = {row["command"]: row for row in receipt["commands"]}
                if target in rows:
                    self.assertEqual(rows[target]["verdict"], "fail")
        with self.subTest(property="only admitted identifiers and versions in receipt"):
            if receipt is not None:
                for row in receipt["commands"]:
                    for key in ("command_id", "version"):
                        value = row[key]
                        if value is not None:
                            self.assertTrue(is_snowflake(value),
                                            f"{row['command']}.{key}={value!r} is not a snowflake")
        self.assertNotIn("Traceback", out)

    def assert_no_details(self):
        self.assertEqual([url for url in self.requested if url.startswith(LIST + "/")], [])

    def changed(self, stage, key, value, target_index=0):
        rows = commands()
        routes = self.routes(rows)
        row = copy.deepcopy(rows[target_index])
        if value is MISSING:
            row.pop(key, None)
        else:
            row[key] = value
        if stage == "list":
            rows[target_index] = row
            routes[LIST] = body(rows)
        else:
            routes[LIST + "/" + str(rows[target_index]["id"])] = body(row)
        return routes

    def test_valid_registration_records_byte_identical_verdicts(self):
        rows = commands()
        code, out, _, receipt = self.run_cli(self.routes(rows))
        self.assertEqual(code, 0, out)
        self.assertEqual(self.requested,
                         [IDENTITY, LIST] + [LIST + "/" + r["id"] for r in rows])
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual(receipt["application_id"], APP)
        self.assertEqual(receipt["guild_id"], GUILD)
        checks = {row["command"]: row for row in receipt["commands"]}
        for row in rows:
            name, command_id, version = row["name"], row["id"], row["version"]
            expected = f"registered in the staging guild (version {version})"
            with self.subTest(command=name):
                self.assertIn("PASS " + name + ": " + expected, out)
                self.assertEqual(checks[name]["verdict"], "pass")
                self.assertEqual(checks[name]["command_id"], command_id)
                self.assertEqual(checks[name]["version"], version)
                self.assertEqual(checks[name]["detail"], expected)

    def test_canonical_uint64_boundaries_are_not_overrestricted(self):
        for value in ("1", "18446744073709551615"):
            with self.subTest(value=value):
                rows = commands()
                rows[0].update(id=value, version=value)
                code, out, _, receipt = self.run_cli(self.routes(rows))
                self.assertEqual(code, 0, out)
                self.assertIn(LIST + "/" + value, self.requested)
                rank = next(row for row in receipt["commands"] if row["command"] == "rank")
                self.assertEqual(rank["command_id"], value)
                self.assertEqual(rank["version"], value)

    def test_invalid_list_ids_refuse_before_every_detail_request(self):
        invalid = ("", "0", "01", "-1", "+1", " 1", "1 ", "1.0", "1e3", "１", "١",
                   "1١", "18446744073709551616", "9" * 21, "9" * 2048,
                   "1/" + SENTINEL, "1?" + SENTINEL + "=1", "1#" + SENTINEL,
                   "%31/" + SENTINEL, "1\\" + SENTINEL, None, True, 1, {}, [], MISSING)
        for value in invalid:
            with self.subTest(value=value):
                result = self.run_cli(self.changed("list", "id", value))
                self.assert_refused(result)
                self.assert_no_details()

    def test_invalid_id_in_last_target_still_prevents_all_detail_requests(self):
        rows = commands()
        rows[-1]["id"] = "1/" + SENTINEL
        result = self.run_cli(self.routes(rows))
        self.assert_refused(result, target="leaderboard")
        self.assert_no_details()

    def test_detail_id_must_equal_the_admitted_list_id(self):
        for value in ("2000000000000000099", "0", "01", "18446744073709551616",
                      "1/" + SENTINEL, None, True, 1, MISSING):
            with self.subTest(value=value):
                self.assert_refused(self.run_cli(self.changed("detail", "id", value)))

    def test_explicit_guild_fields_must_agree_with_the_fenced_endpoint(self):
        for stage in ("list", "detail"):
            for value in (OTHER_GUILD, "0", GUILD + "?" + SENTINEL, None, True):
                with self.subTest(stage=stage, value=value):
                    result = self.run_cli(self.changed(stage, "guild_id", value))
                    self.assert_refused(result)
                    if stage == "list":
                        self.assert_no_details()

    def test_optional_guild_field_can_be_omitted_on_guild_scoped_responses(self):
        rows = commands()
        for row in rows:
            del row["guild_id"]
        code, out, _, receipt = self.run_cli(self.routes(rows))
        self.assertEqual(code, 0, out)
        self.assertEqual(receipt["guild_id"], GUILD)
        self.assertEqual(receipt["result"], "pass")

    def test_duplicate_target_names_are_not_first_match_wins(self):
        for same_id in (False, True):
            for first in (False, True):
                with self.subTest(same_id=same_id, first=first):
                    rows = commands()
                    extra = dict(rows[0],
                                 id=rows[0]["id"] if same_id else "2000000000000000099")
                    rows.insert(0 if first else len(rows), extra)
                    result = self.run_cli(self.routes(rows))
                    self.assert_refused(result)
                    self.assert_no_details()

    def test_one_resource_id_cannot_identify_two_target_names(self):
        rows = commands()
        rows[1]["id"] = rows[0]["id"]
        routes = self.routes(rows)
        # Each detail independently looks valid to a name-first inspector.
        routes[LIST + "/" + rows[0]["id"]] = body(rows[0])
        result = self.run_cli(routes)
        self.assert_refused(result)
        self.assert_no_details()

    def test_target_id_alias_under_an_unrelated_name_is_ambiguous(self):
        rows = commands()
        rows.append(dict(rows[0], name="unrelated-command"))
        routes = self.routes(rows)
        routes[LIST + "/" + rows[0]["id"]] = body(rows[0])
        result = self.run_cli(routes)
        self.assert_refused(result)
        self.assert_no_details()

    def test_missing_prep_entry_fails_only_that_command(self):
        # A missing name fails only itself; the control still reads and passes.
        rows = [r for r in commands() if r["name"] == "rank"]
        code, out, _, receipt = self.run_cli(self.routes(rows))
        self.assertEqual(code, 1, out)
        self.assertIn("PASS rank:", out)
        self.assertIn("FAIL leaderboard: not registered in the staging guild", out)
        self.assertIn(LIST + "/" + IDS[0], self.requested)
        checks = {row["command"]: row for row in receipt["commands"]}
        self.assertEqual(checks["rank"]["verdict"], "pass")
        self.assertEqual(checks["leaderboard"]["verdict"], "fail")
        self.assertIsNone(checks["leaderboard"]["command_id"])

    def test_both_list_and_detail_versions_must_be_canonical_bounded_snowflakes(self):
        invalid = ("", "0", "01", "-1", "1.0", "１", "١", "1١",
                   "18446744073709551616", "9" * 2048,
                   SENTINEL + "\nPASS rank: forged", None, True, 1, {}, [], MISSING)
        for stage in ("list", "detail"):
            for value in invalid:
                with self.subTest(stage=stage, value=value):
                    result = self.run_cli(self.changed(stage, "version", value))
                    self.assert_refused(result)
                    if stage == "list":
                        self.assert_no_details()

    def test_hostile_version_diagnostics_are_fixed_and_never_echoed(self):
        reasons = []
        for suffix in ("_A", "_B"):
            result = self.run_cli(self.changed("detail", "version", SENTINEL + suffix))
            self.assert_refused(result)
            if result[3]:
                reasons.append([row["detail"] for row in result[3]["commands"]
                                if row["command"] == "rank"])
        self.assertEqual(len(reasons), 2)
        self.assertEqual(reasons[0], reasons[1])

    def test_remote_id_or_version_cannot_project_the_configured_token(self):
        for stage in ("list", "detail"):
            for key in ("id", "version"):
                with self.subTest(stage=stage, key=key):
                    result = self.run_cli(self.changed(stage, key, TOKEN))
                    self.assert_refused(result, sentinel=TOKEN)
                    if stage == "list" and key == "id":
                        self.assert_no_details()


if __name__ == "__main__":
    unittest.main()
