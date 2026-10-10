"""Offline staging smoke-run fixtures; stdlib only, no network access."""

import io
import json
import os
import re
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_run_record  # noqa: E402
import staging_smoke_run as smoke  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
ORIGIN = "https://two-bot-next-staging.example-sub.workers.dev"
STAGING_GUILD = "1545644954272137297"
LIVE_GUILD = "326474832151838730"
OTHER_GUILD = "9999999999999999999"
STAGING_APP = "1469137636663758888"
OTHER_APP = "1111111111111111111"
TOKEN = "RAW_SECRET_SENTINEL_never_disclose_bot_token"
SHA = "d111423bd7501e332ce6d1583f352f0028650ca1"
OTHER_SHA = "0123456789abcdef0123456789abcdef01234567"
WORKER_VERSION = "6a4624df-01a0-4979-968e-87324616b8f6"
API = "https://discord.com/api/v10"
LIST_URL = f"{API}/applications/{STAGING_APP}/guilds/{STAGING_GUILD}/commands"

READY = {
    "components": [["process", "ready"], ["gateway", "ready"],
                   ["database", "ready"], ["token_invalid", "ready"]],
    "jobs": {},
    "build_revision": SHA,
    "build_id": "37512552064-1",
}
PUBLISHED = json.loads(smoke.PUBLISH_SET.read_text(encoding="utf-8"))["builtins_in_publish_order"]


def command(name, index):
    return {"id": str(2000000000000000000 + index), "name": name, "type": 1,
            "guild_id": STAGING_GUILD, "version": str(1000000000000000000 + index)}


def commands(*names):
    return [command(name, index) for index, name in enumerate(names)]


def json_body(value, status=200):
    return status, json.dumps(value).encode()


class SmokeRunTests(unittest.TestCase):
    def setUp(self):
        # Any real HTTP is a test failure: the run only gets fake fetches here.
        for opener in (smoke.health_probe.urllib.request.OpenerDirector,):
            guard = mock.patch.object(opener, "open",
                                      side_effect=AssertionError("network access in a unit test"))
            guard.start()
            self.addCleanup(guard.stop)
        self.health_requested = []
        self.discord_requested = []
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.record = Path(tmp.name) / "record.json"

    def health_fetch(self, health=None, readyz=None, headers=None):
        health = health if health is not None else (200, {}, b'{"status":"ok"}')
        readyz = readyz if readyz is not None else (
            200, headers if headers is not None else {"x-two-worker-version": WORKER_VERSION},
            json.dumps(READY).encode())

        def fake(url):
            self.health_requested.append(url)
            response = {ORIGIN + "/health": health, ORIGIN + "/readyz": readyz}[url]
            if isinstance(response, Exception):
                raise response
            return response
        return fake

    def discord_fetch(self, names=None, identity=None, list_response=None, detail=None):
        names = PUBLISHED if names is None else names
        entries = commands(*names)
        by_name = {entry["name"]: entry for entry in entries}
        routes = {f"{API}/users/@me": identity or json_body({"id": STAGING_APP}),
                  LIST_URL: list_response or json_body(entries)}
        for name, entry in by_name.items():
            routes[f"{LIST_URL}/{entry['id']}"] = (detail or {}).get(name) or json_body(entry)

        def fake(url):
            self.discord_requested.append(url)
            if url not in routes:
                raise AssertionError(f"unexpected offline fixture route: {url}")
            response = routes[url]
            if isinstance(response, Exception):
                raise response
            return response
        return fake

    def drive(self, *, health=None, discord=None, guild=STAGING_GUILD, extra=(), token=TOKEN):
        out = io.StringIO()
        argv = ["--staging-url", ORIGIN, "--guild-id", guild, "--record", str(self.record),
                "--tester", "QA", *extra]
        with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": token}, clear=True), \
                redirect_stdout(out):
            code = smoke.main(argv, health_fetch=health or self.health_fetch(),
                              discord_fetch=discord or self.discord_fetch())
        return code, out.getvalue()

    def written(self):
        return json.loads(self.record.read_text(encoding="utf-8"))

    def rows(self, record):
        return {row["name"]: row for row in record["commands"]}

    # -- fences: nothing is sent --------------------------------------------

    def assert_no_request(self):
        self.assertEqual(self.health_requested, [])
        self.assertEqual(self.discord_requested, [])
        self.assertFalse(self.record.exists())

    def test_live_guild_aborts_before_any_request(self):
        code, out = self.drive(guild=LIVE_GUILD)
        self.assertEqual(code, 2)
        self.assertIn("live guild id must never be smoked", out)
        self.assert_no_request()

    def test_live_guild_refuses_even_when_everything_else_is_missing(self):
        out = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), redirect_stdout(out):
            code = smoke.main(["--guild-id", LIVE_GUILD, "--record", str(self.record)],
                              health_fetch=self.health_fetch(), discord_fetch=self.discord_fetch())
        self.assertEqual(code, 2)
        self.assertIn("live guild id must never be smoked", out.getvalue())
        self.assert_no_request()

    def test_unknown_or_missing_guild_refuses(self):
        for guild in (OTHER_GUILD, "", "01545644954272137297", " " + STAGING_GUILD):
            with self.subTest(guild=guild):
                code, _ = self.drive(guild=guild)
                self.assertEqual(code, 2)
                self.assert_no_request()

    def test_missing_token_refuses(self):
        code, out = self.drive(token="")
        self.assertEqual(code, 2)
        self.assertIn("no staging bot token", out)
        self.assert_no_request()

    def test_non_staging_origin_refuses(self):
        for url in ("https://go.two.gg", "https://two-bot-next.5150.workers.dev",
                    "http://two-bot-next-staging.x.workers.dev", ""):
            with self.subTest(url=url):
                out = io.StringIO()
                argv = ["--staging-url", url, "--guild-id", STAGING_GUILD,
                        "--record", str(self.record)]
                with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": TOKEN}, clear=True), \
                        redirect_stdout(out):
                    code = smoke.main(argv, health_fetch=self.health_fetch(),
                                      discord_fetch=self.discord_fetch())
                self.assertEqual(code, 2)
                self.assert_no_request()

    def test_malformed_sha_or_run_id_refuses(self):
        for extra in (("--expected-sha", "abc123"), ("--expected-sha", SHA.upper()),
                      ("--deploy-run-id", "12x")):
            with self.subTest(extra=extra):
                code, _ = self.drive(extra=extra)
                self.assertEqual(code, 2)
                self.assert_no_request()

    # -- healthy staging -----------------------------------------------------

    def test_healthy_run_passes_and_writes_a_valid_record(self):
        code, out = self.drive(extra=("--expected-sha", SHA))
        self.assertEqual(code, 0, out)
        record = self.written()
        schema = json.loads(check_run_record.DEFAULT_SCHEMA.read_text(encoding="utf-8"))
        self.assertEqual(check_run_record.validate(record, schema), [])
        self.assertNotIn("mock", record)
        self.assertEqual(record["verdict"]["disposition"], "PASS")
        self.assertEqual(record["deployment"], {"revision": SHA,
                                                "deploy_staging_run_id": "37512552064",
                                                "worker_version": WORKER_VERSION})
        self.assertEqual(record["environment"]["guild_id"], STAGING_GUILD)
        self.assertEqual(record["cleanup"]["result"], "not_applicable")
        rows = self.rows(record)
        for name in ("GET /health", "GET /readyz", "readyz build identity",
                     "identity and command list", "/rank (registry read)",
                     "/leaderboard (registry read)", "/help (registry read)"):
            self.assertEqual(rows[name]["result"], "pass", name)
        self.assertEqual(len(record["commands"]), 3 + 1 + len(PUBLISHED))
        self.assertTrue(all(row["duration_ms"] >= 0 for row in record["commands"]))

    def test_run_is_get_only_and_reads_only_the_expected_resources(self):
        self.drive()
        self.assertEqual(self.health_requested, [ORIGIN + "/health", ORIGIN + "/readyz"])
        self.assertEqual(self.discord_requested[:2], [f"{API}/users/@me", LIST_URL])
        # One resource read per core surface, nothing for the gated rows.
        self.assertEqual(len(self.discord_requested), 2 + len(smoke.CORE_SURFACES))
        self.assertTrue(all(url.startswith(API) for url in self.discord_requested))

    def test_explicit_deploy_run_id_overrides_the_build_id_prefix(self):
        code, _ = self.drive(extra=("--deploy-run-id", "42"))
        self.assertEqual(code, 0)
        self.assertEqual(self.written()["deployment"]["deploy_staging_run_id"], "42")

    def test_unpublished_gated_surface_is_skipped_not_failed(self):
        names = [name for name in PUBLISHED if name != "attendance"]
        code, _ = self.drive(discord=self.discord_fetch(names=names))
        self.assertEqual(code, 0)
        record = self.written()
        row = self.rows(record)["/attendance (registry read)"]
        self.assertEqual(row["result"], "skipped")
        self.assertNotIn("failure_signature", row)
        self.assertEqual(record["verdict"]["disposition"], "PASS")
        self.assertTrue(any("attendance" in ref for ref in record["verdict"]["follow_up_refs"]))

    def test_extra_live_commands_do_not_fail_the_run(self):
        code, _ = self.drive(discord=self.discord_fetch(names=[*PUBLISHED, "faq"]))
        self.assertEqual(code, 0)

    # -- failures are loud ---------------------------------------------------

    def test_container_down_fails_clearly_with_a_record(self):
        down = self.health_fetch(health=(503, {}, b"upstream down"),
                                 readyz=(503, {}, b"upstream down"))
        code, out = self.drive(health=down, extra=("--expected-sha", SHA, "--deploy-run-id", "7"))
        self.assertEqual(code, 1)
        record = self.written()
        self.assertEqual(record["verdict"]["disposition"], "NEEDS WORK")
        rows = self.rows(record)
        self.assertEqual(rows["GET /health"]["result"], "fail")
        self.assertEqual(rows["GET /health"]["failure_signature"], smoke.SIGNATURE_HEALTH)
        self.assertEqual(rows["GET /readyz"]["failure_signature"], smoke.SIGNATURE_READYZ)
        self.assertIn("FAIL    GET /health", out)

    def test_unreachable_container_fails(self):
        down = self.health_fetch(health=OSError("refused"), readyz=OSError("refused"))
        code, _ = self.drive(health=down, extra=("--expected-sha", SHA, "--deploy-run-id", "7"))
        self.assertEqual(code, 1)
        self.assertEqual(self.rows(self.written())["GET /health"]["result"], "fail")

    def test_parked_container_is_a_failure_not_a_pass(self):
        parked = dict(READY, components=[["process", "ready"], ["gateway", "down"],
                                         ["database", "ready"], ["token_invalid", "ready"]])
        down = self.health_fetch(readyz=(503, {}, json.dumps(parked).encode()))
        code, _ = self.drive(health=down)
        self.assertEqual(code, 1)
        self.assertEqual(self.rows(self.written())["GET /readyz"]["result"], "fail")

    def test_checkpoint_read_failure_has_an_observed_step_signature(self):
        failed_read = dict(READY, components=[["process", "ready"], ["gateway", "down"],
                                              ["database", "ready"], ["token_invalid", "ready"]],
                           gateway_failure={"phase": "durable_gateway",
                                            "class": "checkpoint_load_failed"})
        code, out = self.drive(health=self.health_fetch(
            readyz=(503, {}, json.dumps(failed_read).encode())))
        self.assertEqual(code, 1)
        record = self.written()
        row = self.rows(record)["GET /readyz"]
        self.assertEqual(record["verdict"]["disposition"], "NEEDS WORK")
        self.assertEqual(row["failure_signature"], smoke.SIGNATURE_CHECKPOINT_READ)
        self.assertIn("checkpoint read failed", row["actual"])
        self.assertIn("root cause unverified", row["actual"])
        for text in (out, json.dumps(record)):
            self.assertNotIn("db-behind-binary", text)
            self.assertNotIn("SMOKE-READYZ-DB-BEHIND", text)
            self.assertNotIn("migrate before", text)

    def test_contradictory_checkpoint_class_keeps_the_generic_signature(self):
        for status, gateway, expected in ((200, "down", 503), (503, "ready", 200)):
            with self.subTest(status=status, gateway=gateway):
                contradictory = dict(
                    READY, components=[["process", "ready"], ["gateway", gateway],
                                       ["database", "ready"], ["token_invalid", "ready"]],
                    gateway_failure={"phase": "durable_gateway",
                                     "class": "checkpoint_load_failed",
                                     "detail": "sensitive-fixture-detail"})
                code, out = self.drive(health=self.health_fetch(
                    readyz=(status, {}, json.dumps(contradictory).encode())))
                self.assertEqual(code, 1)
                record = self.written()
                row = self.rows(record)["GET /readyz"]
                self.assertEqual(record["verdict"]["disposition"], "NEEDS WORK")
                self.assertEqual(row["result"], "fail")
                self.assertEqual(row["failure_signature"], smoke.SIGNATURE_READYZ)
                self.assertIn(f"contradicts the component breakdown (expected {expected})",
                              row["actual"])
                self.assertEqual(self.rows(record)["readyz build identity"]["result"], "pass")
                for text in (out, json.dumps(record)):
                    self.assertNotIn(smoke.SIGNATURE_CHECKPOINT_READ, text)
                    self.assertNotIn("checkpoint read failed", text)
                    self.assertNotIn("root cause unverified", text)
                    self.assertNotIn("sensitive-fixture-detail", text)

    def test_ready_checkpoint_class_does_not_create_a_failure_signature(self):
        ready = dict(READY, gateway_failure={"phase": "durable_gateway",
                                            "class": "checkpoint_load_failed"})
        code, _ = self.drive(health=self.health_fetch(
            readyz=(200, {}, json.dumps(ready).encode())))
        self.assertEqual(code, 0)
        record = self.written()
        self.assertEqual(record["verdict"]["disposition"], "PASS")
        row = self.rows(record)["GET /readyz"]
        self.assertEqual(row["result"], "pass")
        self.assertNotIn("failure_signature", row)

    def test_build_revision_mismatch_fails(self):
        code, _ = self.drive(extra=("--expected-sha", OTHER_SHA, "--deploy-run-id", "42"))
        self.assertEqual(code, 1)
        record = self.written()
        self.assertEqual(self.rows(record)["readyz build identity"]["failure_signature"],
                         smoke.SIGNATURE_BUILD)
        # The record names the revision the tester meant to verify and the
        # run the tester named for it.
        self.assertEqual(record["deployment"]["revision"], OTHER_SHA)
        self.assertEqual(record["deployment"]["deploy_staging_run_id"], "42")

    def test_mismatched_revision_never_borrows_the_serving_builds_run_id(self):
        # The readyz build id is the run that deployed the serving revision.
        # Pairing it with a different expected revision would misattribute the run.
        code, out = self.drive(extra=("--expected-sha", OTHER_SHA))
        self.assertEqual(code, 1)
        self.assertFalse(self.record.exists())
        self.assertIn("no run record written", out)
        self.assertIn("FAIL    readyz build identity", out)

    def test_matching_expected_revision_may_use_the_build_id_run(self):
        code, _ = self.drive(extra=("--expected-sha", SHA))
        self.assertEqual(code, 0)
        self.assertEqual(self.written()["deployment"]["deploy_staging_run_id"], "37512552064")

    def test_missing_core_surface_fails_the_run(self):
        for missing in smoke.CORE_SURFACES:
            with self.subTest(missing=missing):
                names = [name for name in PUBLISHED if name != missing]
                code, _ = self.drive(discord=self.discord_fetch(names=names))
                self.assertEqual(code, 1)
                row = self.rows(self.written())[f"/{missing} (registry read)"]
                self.assertEqual(row["result"], "fail")
                self.assertEqual(row["failure_signature"], smoke.SIGNATURE_MISSING)

    def test_disagreeing_command_resource_fails(self):
        detail = {"rank": json_body(command("leaderboard", 99))}
        code, _ = self.drive(discord=self.discord_fetch(detail=detail))
        self.assertEqual(code, 1)
        row = self.rows(self.written())["/rank (registry read)"]
        self.assertEqual(row["failure_signature"], smoke.SIGNATURE_DETAIL)

    def test_foreign_application_token_is_refused_and_not_retried(self):
        discord = self.discord_fetch(identity=json_body({"id": OTHER_APP}))
        code, _ = self.drive(discord=discord)
        self.assertEqual(code, 1)
        row = self.rows(self.written())["identity and command list"]
        self.assertEqual(row["result"], "fail")
        self.assertEqual(row["failure_signature"], smoke.SIGNATURE_DISCORD)
        self.assertEqual(self.discord_requested, [f"{API}/users/@me"])

    def test_rejected_token_stops_at_one_request(self):
        discord = self.discord_fetch(identity=(401, b'{"message":"401: Unauthorized"}'))
        code, out = self.drive(discord=discord)
        self.assertEqual(code, 1)
        self.assertIn("rotation is an operator decision", out)
        self.assertEqual(self.discord_requested, [f"{API}/users/@me"])

    def test_discord_outage_fails_without_hiding_health_results(self):
        discord = self.discord_fetch(identity=OSError("unreachable"))
        code, _ = self.drive(discord=discord)
        self.assertEqual(code, 1)
        rows = self.rows(self.written())
        self.assertEqual(rows["GET /health"]["result"], "pass")
        self.assertEqual(rows["identity and command list"]["result"], "fail")

    # -- record boundaries ---------------------------------------------------

    def test_no_record_without_a_tested_revision_or_deploy_run(self):
        down = self.health_fetch(health=OSError("refused"), readyz=OSError("refused"))
        for extra in ((), ("--expected-sha", SHA), ("--deploy-run-id", "7")):
            with self.subTest(extra=extra):
                code, out = self.drive(health=down, extra=extra)
                self.assertEqual(code, 1)
                self.assertFalse(self.record.exists())
                self.assertIn("no run record written", out)
                self.assertIn("FAIL    GET /health", out)

    def test_token_never_reaches_output_or_record(self):
        discord = self.discord_fetch(identity=(401, TOKEN.encode()))
        for case in (self.discord_fetch(), discord):
            with self.subTest(case=case is discord):
                self.record.unlink(missing_ok=True)
                _, out = self.drive(discord=case)
                self.assertNotIn(TOKEN, out)
                if self.record.exists():
                    self.assertNotIn(TOKEN, self.record.read_text(encoding="utf-8"))

    def test_record_is_public_safe(self):
        self.drive(extra=("--expected-sha", SHA))
        text = self.record.read_text(encoding="utf-8")
        self.assertIsNone(re.search(r"\b(TOG|PAP)-\d+\b", text))

    # -- drift guards --------------------------------------------------------

    def test_core_surfaces_match_the_matrix_always_rows(self):
        matrix = (ROOT / "docs/staging-e2e-command-matrix.md").read_text(encoding="utf-8")
        always = re.findall(r"^\| \d+ \| `/([a-z-]+)` \| always \|", matrix, re.MULTILINE)
        self.assertEqual(sorted(always), sorted(smoke.CORE_SURFACES))
        self.assertTrue(set(smoke.CORE_SURFACES) <= set(PUBLISHED))

    def test_identity_constants_have_one_authority(self):
        # The run reuses the existing smoke's pinned ids; no second list here.
        self.assertEqual(smoke.discord_read.STAGING_GUILD_ID, STAGING_GUILD)
        self.assertEqual(smoke.discord_read.LIVE_GUILD_ID, LIVE_GUILD)
        self.assertEqual(smoke.discord_read.STAGING_APPLICATION_ID, STAGING_APP)


if __name__ == "__main__":
    unittest.main()
