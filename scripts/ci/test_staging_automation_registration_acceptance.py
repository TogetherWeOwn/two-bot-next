"""Protected offline registration acceptance: exercise the CLI, not handlers.

Discord contract: https://docs.discord.com/developers/interactions/application-commands
and https://docs.discord.com/developers/reference (consulted 2026-10-19).
Command id, application_id and version are required snowflakes (uint64 strings).
Type may be omitted (defaults to 1); guild_id is optional on the object, but
must agree with the fenced guild when supplied. Both reads must be independently
valid. Versions may drift between GETs; the receipt reports the detail version,
not a claim that the two responses form an atomic snapshot.

The existing check.yml scripts/ci unittest discovery executes this separate file.
Only transport is faked; parsing, admission, grading, stdout and artifact writes
are real. A removed admission/identity/projection check must break these tests.
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
smoke = importlib.import_module("staging_automation_read_smoke")

API = "https://discord.com/api/v10"
GUILD = "1545644954272137297"
APP = "1469137636663758888"
OTHER_APP = "1111111111111111111"
OTHER_GUILD = "9999999999999999999"
IDENTITY = API + "/users/@me"
LIST = API + "/applications/" + APP + "/guilds/" + GUILD + "/commands"
TOKEN = "fixture-bot-token-not-a-real-credential"
SENTINEL = "REMOTE_REGISTRATION_SENTINEL"
NAMES = ("rank", "command-list", "schedule-list", "feed-list")
IDS = ("1549621908675366953", "2000000000000000001",
       "2000000000000000002", "2000000000000000003")
VERSIONS = ("1000000000000000001", "1000000000000000002",
            "1000000000000000003", "1000000000000000004")
MISSING = object()


def commands():
    return [{"id": command_id, "name": name, "type": 1,
             "application_id": APP, "guild_id": GUILD,
             "version": version, "description": "Offline registration fixture"}
            for name, command_id, version in zip(NAMES, IDS, VERSIONS)]


def body(value):
    return 200, json.dumps(value).encode()


def duplicate(raw, key, first, last):
    """Retain a JSON duplicate on the wire; never round-trip it through a dict."""
    value = json.loads(raw)
    value.pop(key, None)
    rest = json.dumps(value)[1:]
    return ("{" + json.dumps(key) + ":" + json.dumps(first) + ","
            + json.dumps(key) + ":" + json.dumps(last) + "," + rest).encode()


class RegistrationAcceptanceTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch.object(
            smoke.urllib.request.OpenerDirector, "open",
            side_effect=AssertionError("real network forbidden in offline acceptance"))
        guard.start()
        self.addCleanup(guard.stop)

    def routes(self, rows=None):
        rows = commands() if rows is None else rows
        routes = {IDENTITY: body({"id": APP, "username": "offline-fixture", "bot": True}),
                  LIST: body(rows)}
        for row in rows:
            routes[LIST + "/" + str(row["id"])] = body(row)
        return routes

    def run_cli(self, routes, guild=GUILD, token=TOKEN, transport=False):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get(
            "PAPERCLIP_SCRATCH_DIR")
        tmp = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(tmp.cleanup)
        evidence = Path(tmp.name) / "registration.json"
        self.requested = []
        self.transport_requests = []
        self.read_sizes = []

        def fetch(url):
            self.requested.append(url)
            if url in routes:
                return routes[url]
            # Let malformed detail URLs reach a successful fake response so
            # zero-request assertions detect URL admission, not a routing error.
            if url.startswith(LIST + "/"):
                return body(commands()[0])
            raise AssertionError("unexpected offline request")

        owner = self

        class Response(io.BytesIO):
            def __init__(self, status, payload):
                super().__init__(payload)
                self.status = status

            def read(self, size=-1):
                owner.read_sizes.append(size)
                return super().read(size)

        class Opener:
            def open(self, request, timeout=None):
                owner.transport_requests.append(request)
                status, payload = fetch(request.full_url)
                return Response(status, payload)

        with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": token}, clear=True):
            with mock.patch.object(smoke.urllib.request, "build_opener", return_value=Opener()):
                out = io.StringIO()
                with redirect_stdout(out):
                    args = ["--guild-id", guild, "--evidence", str(evidence)]
                    code = smoke.main(args) if transport else smoke.main(args, fetch_fn=fetch)
        artifact = evidence.read_text() if evidence.exists() else ""
        receipt = json.loads(artifact) if artifact else None
        self.assertNotIn(TOKEN, out.getvalue())
        self.assertNotIn(TOKEN, artifact)
        return code, out.getvalue(), artifact, receipt

    def assert_refused(self, result, target="rank", sentinel=SENTINEL):
        code, out, artifact, receipt = result
        # Independent subtests expose both grading and projection defects on RED.
        with self.subTest(property="non-PASS exit"):
            self.assertEqual(code, 1, out)
        with self.subTest(property="no remote stdout echo"):
            self.assertNotIn(sentinel, out)
        with self.subTest(property="no remote artifact echo"):
            self.assertNotIn(sentinel, artifact)
        with self.subTest(property="no command PASS"):
            self.assertNotIn("PASS " + target + ":", out)
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
                            self.assertIsInstance(value, str)
                            self.assertRegex(value, r"\A[1-9][0-9]{0,19}\Z")
                            self.assertLessEqual(int(value), 18446744073709551615)
        self.assertNotIn("Traceback", out)

    def assert_no_details(self):
        self.assertEqual([url for url in self.requested if url.startswith(LIST + "/")], [])

    def changed(self, stage, key, value):
        rows = commands()
        routes = self.routes(rows)
        row = copy.deepcopy(rows[0])
        if value is MISSING:
            row.pop(key, None)
        else:
            row[key] = value
        if stage == "list":
            rows[0] = row
            routes[LIST] = body(rows)
        else:
            routes[LIST + "/" + IDS[0]] = body(row)
        return routes

    def test_valid_registration_records_only_verified_command_ids_and_detail_versions(self):
        code, out, _, receipt = self.run_cli(self.routes())
        self.assertEqual(code, 0, out)
        self.assertEqual(self.requested, [IDENTITY, LIST] + [LIST + "/" + i for i in IDS])
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual(receipt["application_id"], APP)
        self.assertEqual(receipt["guild_id"], GUILD)
        rows = {row["command"]: row for row in receipt["commands"]}
        self.assertEqual(set(rows), set(NAMES))
        for name, command_id, version in zip(NAMES, IDS, VERSIONS):
            self.assertIn("PASS " + name + ":", out)
            self.assertEqual(rows[name]["verdict"], "pass")
            self.assertEqual(rows[name]["command_id"], command_id)
            self.assertEqual(rows[name]["version"], version)

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
                   "18446744073709551616", "9" * 21, "9" * 2048,
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
        self.assert_refused(result, target="feed-list")
        self.assert_no_details()

    def test_invalid_list_id_failure_reason_is_fixed_not_remote_text(self):
        reasons = []
        for suffix in ("_A", "_B"):
            result = self.run_cli(self.changed("list", "id", "1?" + SENTINEL + suffix))
            self.assert_refused(result)
            receipt = result[3]
            if receipt:
                reasons.append([row["detail"] for row in receipt["commands"]
                                if row["command"] == "rank"])
        self.assertEqual(len(reasons), 2)
        self.assertEqual(reasons[0], reasons[1])

    def test_detail_id_must_equal_the_admitted_list_id(self):
        for value in ("2000000000000000099", "0", "01", "18446744073709551616",
                      "1/" + SENTINEL, None, True, 1, MISSING):
            with self.subTest(value=value):
                self.assert_refused(self.run_cli(self.changed("detail", "id", value)))

    def test_list_and_detail_require_the_staging_application_identity(self):
        for stage in ("list", "detail"):
            for value in (OTHER_APP, "0", "01", "1/" + SENTINEL, None, True, MISSING):
                with self.subTest(stage=stage, value=value):
                    self.assert_refused(self.run_cli(self.changed(stage, "application_id", value)))

    def test_explicit_guild_fields_must_agree_with_the_fenced_endpoint(self):
        for stage in ("list", "detail"):
            for value in (OTHER_GUILD, "0", GUILD + "?" + SENTINEL, None, True):
                with self.subTest(stage=stage, value=value):
                    self.assert_refused(self.run_cli(self.changed(stage, "guild_id", value)))

    def test_optional_guild_field_can_be_omitted_on_guild_scoped_responses(self):
        rows = commands()
        for row in rows:
            del row["guild_id"]
        code, out, _, receipt = self.run_cli(self.routes(rows))
        self.assertEqual(code, 0, out)
        self.assertEqual(receipt["guild_id"], GUILD)
        self.assertEqual(receipt["result"], "pass")

    def test_explicit_command_type_must_be_integer_slash_type_on_both_reads(self):
        for stage in ("list", "detail"):
            for value in (2, 3, 4, 0, True, 1.0, "1", None):
                with self.subTest(stage=stage, value=value):
                    self.assert_refused(self.run_cli(self.changed(stage, "type", value)))

    def test_omitted_type_uses_the_documented_slash_default(self):
        rows = commands()
        for row in rows:
            del row["type"]
        code, out, _, receipt = self.run_cli(self.routes(rows))
        self.assertEqual(code, 0, out)
        self.assertEqual(receipt["result"], "pass")

    def test_detail_name_must_equal_the_selected_target_without_echo(self):
        result = self.run_cli(self.changed("detail", "name", SENTINEL))
        self.assert_refused(result)

    def test_duplicate_target_names_are_not_first_match_wins(self):
        for same_id in (False, True):
            for first in (False, True):
                with self.subTest(same_id=same_id, first=first):
                    rows = commands()
                    extra = dict(rows[0], id=IDS[0] if same_id else "2000000000000000099")
                    rows.insert(0 if first else len(rows), extra)
                    self.assert_refused(self.run_cli(self.routes(rows)))

    def test_one_resource_id_cannot_identify_two_target_names(self):
        rows = commands()
        rows[1]["id"] = rows[0]["id"]
        routes = self.routes(rows)
        # Each detail independently looks valid to a name-first inspector.
        routes[LIST + "/" + IDS[0]] = body(rows[0])
        self.assert_refused(self.run_cli(routes))

    def test_target_id_alias_under_an_unrelated_name_is_ambiguous(self):
        rows = commands()
        rows.append(dict(rows[0], name="unrelated-command"))
        routes = self.routes(rows)
        routes[LIST + "/" + IDS[0]] = body(rows[0])
        self.assert_refused(self.run_cli(routes))

    def test_unrelated_unique_registration_does_not_poison_valid_targets(self):
        rows = commands()
        rows.append(dict(rows[0], id="2000000000000000099", name="unrelated-command", type=2))
        code, out, _, receipt = self.run_cli(self.routes(rows))
        self.assertEqual(code, 0, out)
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual(self.requested, [IDENTITY, LIST] + [LIST + "/" + i for i in IDS])

    def test_versions_may_drift_but_receipt_uses_the_independently_validated_detail(self):
        for detail_version in ("1", "18446744073709551615"):
            with self.subTest(detail_version=detail_version):
                code, out, _, receipt = self.run_cli(
                    self.changed("detail", "version", detail_version))
                self.assertEqual(code, 0, out)
                rank = next(row for row in receipt["commands"] if row["command"] == "rank")
                self.assertEqual(rank["version"], detail_version)
                self.assertIn("PASS rank:", out)

    def test_both_list_and_detail_versions_must_be_canonical_bounded_snowflakes(self):
        invalid = ("", "0", "01", "-1", "+1", " 1", "1.0", "1e3", "１",
                   "18446744073709551616", "9" * 2048,
                   SENTINEL + "\nPASS rank: forged\x1b[2J", None, True, 1, {}, [], MISSING)
        for stage in ("list", "detail"):
            for value in invalid:
                with self.subTest(stage=stage, value=value):
                    self.assert_refused(self.run_cli(self.changed(stage, "version", value)))

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
                    self.assert_refused(self.run_cli(self.changed(stage, key, TOKEN)))

    def test_duplicate_json_keys_are_refused_at_identity_list_and_detail_before_grading(self):
        for stage in ("identity", "list", "detail"):
            for identical in (False, True):
                with self.subTest(stage=stage, identical=identical):
                    routes = self.routes()
                    if stage == "identity":
                        raw = routes[IDENTITY][1]
                        routes[IDENTITY] = (200, duplicate(raw, "id", APP if identical else SENTINEL, APP))
                    else:
                        raw = body(commands()[0])[1]
                        raw = duplicate(raw, "id", IDS[0] if identical else SENTINEL, IDS[0])
                        if stage == "detail":
                            routes[LIST + "/" + IDS[0]] = (200, raw)
                        else:
                            tail = b",".join(body(row)[1] for row in commands()[1:])
                            routes[LIST] = (200, b"[" + raw + b"," + tail + b"]")
                    self.assert_refused(self.run_cli(routes))
                    if stage != "detail":
                        self.assert_no_details()
                    if stage == "identity":
                        self.assertEqual(self.requested, [IDENTITY])

    def test_duplicate_keys_in_nested_json_are_not_silently_collapsed(self):
        for url in (IDENTITY, LIST, LIST + "/" + IDS[0]):
            with self.subTest(url=url):
                routes = self.routes()
                nested = b'{"note":"' + SENTINEL.encode() + b'","note":"safe"}'
                if url == LIST:
                    row = body(commands()[0])[1][:-1] + b',"extra":' + nested + b"}"
                    routes[url] = (200, b"[" + row + b"," + b",".join(
                        body(item)[1] for item in commands()[1:]) + b"]")
                else:
                    routes[url] = (200, routes[url][1][:-1] + b',"extra":' + nested + b"}")
                self.assert_refused(self.run_cli(routes))

    def test_body_and_decoder_depth_bounds_remain_non_echoing_at_each_read(self):
        for url in (IDENTITY, LIST, LIST + "/" + IDS[0]):
            for oversized in (False, True):
                with self.subTest(url=url, oversized=oversized):
                    routes = self.routes()
                    if oversized:
                        payload = routes[url][1]
                        payload += b" " * (smoke.BODY_CAP + 1 - len(payload))
                    else:
                        payload = b"[" * 32768 + b"]" * 32768
                        self.assertLessEqual(len(payload), smoke.BODY_CAP)
                    routes[url] = (200, payload)
                    self.assert_refused(self.run_cli(routes))
                    if url == IDENTITY:
                        self.assertEqual(self.requested, [IDENTITY])
                    elif url == LIST:
                        self.assert_no_details()

    def test_redirect_and_auth_refusals_are_non_echoing_without_retries(self):
        for status in (301, 302, 303, 307, 308, 401, 403, 429):
            with self.subTest(status=status):
                routes = self.routes()
                routes[IDENTITY] = (status, SENTINEL.encode())
                self.assert_refused(self.run_cli(routes))
                self.assertEqual(self.requested, [IDENTITY])

    def test_staging_guild_and_missing_token_fences_still_send_nothing(self):
        for guild, token in (("326474832151838730", TOKEN), (OTHER_GUILD, TOKEN),
                             ("", TOKEN), (GUILD, "")):
            with self.subTest(guild=guild, missing_token=not token):
                code, out, artifact, receipt = self.run_cli(self.routes(), guild=guild, token=token)
                self.assertEqual(code, 2, out)
                self.assertEqual(self.requested, [])
                self.assertEqual(artifact, "")
                self.assertIsNone(receipt)

    def test_wrong_token_identity_stops_before_list_or_details(self):
        routes = self.routes()
        routes[IDENTITY] = body({"id": OTHER_APP})
        self.assert_refused(self.run_cli(routes))
        self.assertEqual(self.requested, [IDENTITY])

    def test_actual_transport_only_builds_bounded_authenticated_gets_for_the_pinned_routes(self):
        code, out, _, receipt = self.run_cli(self.routes(), transport=True)
        self.assertEqual(code, 0, out)
        self.assertEqual(receipt["result"], "pass")
        self.assertEqual([request.full_url for request in self.transport_requests],
                         [IDENTITY, LIST] + [LIST + "/" + i for i in IDS])
        self.assertEqual([request.get_method() for request in self.transport_requests], ["GET"] * 6)
        self.assertEqual([request.data for request in self.transport_requests], [None] * 6)
        self.assertEqual([request.get_header("Authorization") for request in self.transport_requests],
                         ["Bot " + TOKEN] * 6)
        self.assertEqual(self.read_sizes, [smoke.BODY_CAP + 1] * 6)


if __name__ == "__main__":
    unittest.main()
