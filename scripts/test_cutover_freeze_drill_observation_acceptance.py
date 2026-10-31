"""Protected acceptance: acknowledged writes are not freeze/cleanup observations.

Offline only. Exercise run_drill and the real HTTP adapter against the existing
loopback helper, seeded from its mutable mock fixture. No implementation source
or diff is used to derive expectations. The combined fix must retain this file.
"""

import copy
import json
import unittest
import urllib.request
from http.server import BaseHTTPRequestHandler
from unittest import mock
from urllib.parse import urlsplit

import test_cutover_freeze_drill as fixtures


drill = fixtures.drill
GUILD = fixtures.STAGING_GUILD
CHANNEL = fixtures.CHANNEL
CHANNEL_PATH = fixtures.CHANNEL_PATH
NOTICE_PATH = fixtures.NOTICE_PATH
OVERWRITE_PATH = fixtures.OVERWRITE_PATH
COMMANDS_PATH = fixtures.COMMANDS_PATH
NOTICE_ID = fixtures.NOTICE_ID
LOCK_BITS = 2048 | 64 | (1 << 35) | (1 << 36) | (1 << 38)
FOREIGN = {"id": "5555555555555555555", "type": 0, "allow": "999", "deny": "888"}
SEED = {"id": GUILD, "type": 0, "allow": "6144", "deny": "1024"}
DECOY_ID = "9000000000000000002"


class ObservedDiscord:
    """Mutable HTTP fixture: successful writes can deliberately have no effect.

    Only the exact notice's GET answers Discord's Unknown Message (10008) when
    absent. Generic 404s, permission errors and malformed replies are not absence.
    Requests and returned channel snapshots make observation ordering inspectable.
    """

    def __init__(self):
        self.state = copy.deepcopy(drill.mock_transport().state)
        self.state["channel"].update({
            "id": CHANNEL, "rate_limit_per_user": 7,
            "permission_overwrites": [copy.deepcopy(FOREIGN), copy.deepcopy(SEED)],
        })
        self.baseline = copy.deepcopy(self.state["channel"])
        self.state["notices"] = {DECOY_ID: {"id": DECOY_ID, "channel_id": CHANNEL}}
        self.events = []
        self.slowmode_inert = False
        self.lockdown_inert = False
        self.restore_inert = None
        self.notice_inert = False
        self.notice_inert_once = False
        self.notice_read_status = None
        self.notice_read_body = None
        self.frozen_reply = None
        self.frozen_read_status = None
        self.freeze_started = False
        self.restoring = False
        self.delete_count = 0
        self.origin = fixtures.LoopbackDiscord()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def serve(self):
                length = int(self.headers.get("Content-Length") or 0)
                payload = json.loads(self.rfile.read(length)) if length else None
                status, body = outer.respond(self.command, self.path, payload)
                data = json.dumps(body).encode() if body is not None else b""
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = serve

            def log_message(self, *args):
                pass

        self.origin.server.RequestHandlerClass = Handler

    def respond(self, method, path, payload):
        event = {"method": method, "path": path, "payload": payload}
        self.events.append(event)
        channel = self.state["channel"]
        if (method, path) == ("GET", CHANNEL_PATH):
            if self.freeze_started and not self.restoring and self.frozen_read_status:
                return self.frozen_read_status, {"message": fixtures.TOKEN, "code": 0}
            result = copy.deepcopy(channel)
            if self.freeze_started and not self.restoring and self.frozen_reply:
                result = self.frozen_reply(result)
            event["channel"] = copy.deepcopy(result)
            return 200, result
        if (method, path) == ("POST", fixtures.MESSAGES_PATH):
            self.state["notices"][NOTICE_ID] = {"id": NOTICE_ID, "channel_id": CHANNEL}
            return 200, copy.deepcopy(self.state["notices"][NOTICE_ID])
        if (method, path) == ("PATCH", CHANNEL_PATH):
            seconds = payload["rate_limit_per_user"]
            if seconds == 30 and not self.restoring:
                self.freeze_started = True
                inert = self.slowmode_inert
            else:
                self.restoring = True
                inert = self.restore_inert == "slowmode"
            if not inert:
                channel["rate_limit_per_user"] = seconds
            # An ACK payload claims success even for an inert write.
            return 200, {**copy.deepcopy(channel), "rate_limit_per_user": seconds}
        if (method, path) == ("PUT", OVERWRITE_PATH):
            inert = (self.restore_inert == "overwrite" if self.restoring
                     else self.lockdown_inert)
            if not inert:
                rows = channel["permission_overwrites"]
                rows[:] = [r for r in rows if r["id"] != GUILD]
                rows.append({"id": GUILD, "type": 0,
                             "allow": payload["allow"], "deny": payload["deny"]})
            return 204, None
        if (method, path) == ("DELETE", OVERWRITE_PATH):
            if self.restore_inert != "overwrite":
                channel["permission_overwrites"][:] = [
                    r for r in channel["permission_overwrites"] if r["id"] != GUILD]
            return 204, None
        if (method, path) == ("DELETE", NOTICE_PATH):
            self.delete_count += 1
            if not self.notice_inert and not (self.notice_inert_once and self.delete_count == 1):
                self.state["notices"].pop(NOTICE_ID, None)
            return 204, None
        if (method, path) == ("GET", NOTICE_PATH):
            if self.notice_read_status is not None:
                return self.notice_read_status, {"message": fixtures.TOKEN, "code": 0}
            if self.notice_read_body is not None:
                return 200, copy.deepcopy(self.notice_read_body)
            if NOTICE_ID in self.state["notices"]:
                return 200, copy.deepcopy(self.state["notices"][NOTICE_ID])
            return 404, {"code": 10008, "message": "Unknown Message"}
        if (method, path) == ("GET", COMMANDS_PATH):
            return 200, copy.deepcopy(self.state["commands"])
        return 404, {"code": 0, "message": "unexpected fixture route"}


class FreezeObservationAcceptance(unittest.TestCase):
    def setUp(self):
        self.discord = ObservedDiscord()
        self.addCleanup(self.discord.origin.close)
        original_open = urllib.request.OpenerDirector.open
        allowed = urlsplit(self.discord.origin.base)

        def loopback_only(opener, request, *args, **kwargs):
            target = urlsplit(request.full_url if hasattr(request, "full_url") else request)
            self.assertEqual((target.scheme, target.netloc), (allowed.scheme, allowed.netloc),
                             "acceptance tests must never leave their loopback fixture")
            return original_open(opener, request, *args, **kwargs)

        guard = mock.patch("urllib.request.OpenerDirector.open", new=loopback_only)
        guard.start()
        self.addCleanup(guard.stop)
        for patcher in (
            mock.patch.dict("os.environ", {"NO_PROXY": "*", "no_proxy": "*"}),
            mock.patch.object(drill, "API", self.discord.origin.base + "/api/v10"),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def run_drill(self, guild=GUILD):
        result = drill.run_drill(guild, CHANNEL, fixtures.REASON,
                                 drill.live_transport(fixtures.TOKEN))
        blob = json.dumps(result)
        for forbidden in (fixtures.TOKEN, "DISCORD_STAGING_BOT_TOKEN", NOTICE_ID, DECOY_ID):
            self.assertNotIn(forbidden, blob)
        for step in result["steps"]:
            self.assertGreaterEqual(step["duration_ms"], 0)
            self.assertRegex(step["started_utc"], r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")
        return result

    def assert_refused(self, result):
        self.assertEqual(result["result"], "fail", "unobserved/mismatched state must not PASS")
        self.assertTrue(any(s["result"] == "fail" for s in result["steps"]), result)

    def assert_cleanup_attempted(self):
        writes = [e for e in self.discord.events if e["method"] != "GET"]
        self.assertTrue(any(e["method"] == "PATCH" and
                            e["payload"].get("rate_limit_per_user") == 7 for e in writes))
        self.assertTrue(any(e["method"] == "PUT" and e["payload"].get("allow") == "6144"
                            and e["payload"].get("deny") == "1024" for e in writes))
        self.assertTrue(any(e["method"] == "DELETE" and e["path"] == NOTICE_PATH for e in writes))

    def assert_baseline_restored(self):
        self.assertEqual(self.discord.state["channel"], self.discord.baseline)
        self.assertNotIn(NOTICE_ID, self.discord.state["notices"])
        self.assertIn(DECOY_ID, self.discord.state["notices"])

    def assert_notice_observed_after_delete(self):
        events = self.discord.events
        deletes = [i for i, e in enumerate(events)
                   if e["method"] == "DELETE" and e["path"] == NOTICE_PATH]
        self.assertTrue(deletes, "the exact created notice must be deleted")
        self.assertTrue(any(e["method"] == "GET" and e["path"] == NOTICE_PATH
                            for e in events[deletes[-1] + 1:]),
                        "delete ACK is not an independent notice-absence observation")

    def test_ordinary_success_control_restores_nonzero_seed_and_preserves_other_notice(self):
        result = self.run_drill()
        self.assertEqual(result["result"], "pass")
        self.assertTrue(all(s["result"] == "pass" for s in result["steps"]))
        self.assertTrue(result["restore"]["restored"])
        self.assertTrue(result["restore"]["notice_removed"])
        self.assertEqual(result["restore"]["pre_hash"], result["restore"]["post_hash"])
        self.assert_baseline_restored()

    def test_absent_overwrite_control_restores_absence(self):
        self.discord.state["channel"]["permission_overwrites"] = [copy.deepcopy(FOREIGN)]
        self.discord.baseline = copy.deepcopy(self.discord.state["channel"])
        result = self.run_drill()
        self.assertEqual(result["result"], "pass")
        self.assertTrue(result["restore"]["restored"])
        self.assert_baseline_restored()
        self.assertTrue(any(e["method"] == "DELETE" and e["path"] == OVERWRITE_PATH
                            for e in self.discord.events))

    def test_failed_frozen_read_is_not_success_and_still_restores(self):
        self.discord.frozen_read_status = 503
        result = self.run_drill()
        self.assert_cleanup_attempted()
        self.assert_baseline_restored()
        self.assert_refused(result)

    def test_full_drill_observes_frozen_semantics_before_command_check_and_restores(self):
        result = self.run_drill()
        self.assertEqual(result["result"], "pass")
        self.assertTrue(result["restore"]["restored"])
        self.assertTrue(result["restore"]["notice_removed"])
        self.assertEqual(result["restore"]["pre_hash"], result["restore"]["post_hash"])
        events = self.discord.events
        command_index = next(i for i, e in enumerate(events) if e["path"] == COMMANDS_PATH)
        reads = [e["channel"] for e in events[:command_index] if "channel" in e]
        self.assertTrue(any(c["rate_limit_per_user"] == 30 and
                            c["permission_overwrites"] == [FOREIGN, {
                                "id": GUILD, "type": 0, "allow": "4096",
                                "deny": str(LOCK_BITS | 1024),
                            }] for c in reads),
                        "a frozen readback must precede command-surface success")
        self.assert_cleanup_attempted()
        self.assert_baseline_restored()
        self.assert_notice_observed_after_delete()

    def test_slowmode_ack_without_effect_is_not_success(self):
        self.discord.slowmode_inert = True
        result = self.run_drill()
        self.assert_cleanup_attempted()
        self.assert_baseline_restored()
        self.assert_refused(result)
        self.assertFalse(any(s["name"] == "slowmode-on" and s["result"] == "pass"
                             for s in result["steps"]))

    def test_lockdown_ack_without_effect_is_not_success(self):
        self.discord.lockdown_inert = True
        result = self.run_drill()
        self.assert_cleanup_attempted()
        self.assert_baseline_restored()
        self.assert_refused(result)
        self.assertFalse(any(s["name"] == "lockdown" and s["result"] == "pass"
                             for s in result["steps"]))

    def test_wrong_or_malformed_frozen_readbacks_refuse_and_restore(self):
        cases = {
            "wrong guild": {"guild_id": fixtures.OTHER_GUILD},
            "missing guild": {"guild_id": None},
            "wrong channel": {"id": "1000000000000000003"},
            "missing channel": {"id": None},
            "unparseable slowmode": {"rate_limit_per_user": "broken"},
            "wrong slowmode": {"rate_limit_per_user": 29},
            "missing overwrites": {"permission_overwrites": None},
            "malformed mask": {"permission_overwrites": [
                FOREIGN, {"id": GUILD, "type": 0, "allow": "broken", "deny": "0"}]},
            "partial deny": {"permission_overwrites": [
                FOREIGN, {"id": GUILD, "type": 0, "allow": "4096", "deny": "3072"}]},
            "send still allowed": {"permission_overwrites": [
                FOREIGN, {"id": GUILD, "type": 0, "allow": "6144",
                          "deny": str(LOCK_BITS | 1024)}]},
            "unrelated bit lost": {"permission_overwrites": [
                FOREIGN, {"id": GUILD, "type": 0, "allow": "0",
                          "deny": str(LOCK_BITS | 1024)}]},
        }
        for name, changes in cases.items():
            with self.subTest(case=name):
                self.discord.frozen_reply = lambda channel, changes=changes: {**channel, **changes}
                self.discord.events.clear()
                self.discord.freeze_started = self.discord.restoring = False
                result = self.run_drill()
                self.assert_cleanup_attempted()
                self.assert_baseline_restored()
                self.assert_refused(result)

    def test_retained_notice_after_successful_delete_is_not_removed(self):
        self.discord.notice_inert = True
        result = self.run_drill()
        self.assert_cleanup_attempted()
        self.assertEqual(self.discord.state["channel"], self.discord.baseline)
        self.assertIn(NOTICE_ID, self.discord.state["notices"])
        self.assert_refused(result)
        self.assertFalse(result["restore"]["notice_removed"])
        self.assert_notice_observed_after_delete()

    def test_notice_identity_survives_inert_delete_for_best_effort_cleanup(self):
        self.discord.notice_inert_once = True
        result = self.run_drill()
        self.assert_refused(result)
        self.assertGreaterEqual(self.discord.delete_count, 2,
                                "do not forget the notice ID on an unverified DELETE ACK")
        self.assert_baseline_restored()
        self.assert_notice_observed_after_delete()

    def test_notice_absence_read_errors_do_not_become_absence(self):
        for status in (403, 404, 429, 503):
            with self.subTest(status=status):
                self.discord.notice_read_status = status
                self.discord.events.clear()
                self.discord.freeze_started = self.discord.restoring = False
                result = self.run_drill()
                self.assert_cleanup_attempted()
                self.assert_baseline_restored()
                self.assert_refused(result)
                self.assertFalse(result["restore"]["notice_removed"])
                self.assert_notice_observed_after_delete()

    def test_malformed_or_wrong_notice_readback_never_proves_absence(self):
        for body in ([], {}, {"id": DECOY_ID, "channel_id": CHANNEL},
                     {"id": NOTICE_ID, "channel_id": "1000000000000000003"}):
            with self.subTest(body=body):
                self.discord.notice_read_body = body
                self.discord.events.clear()
                self.discord.freeze_started = self.discord.restoring = False
                result = self.run_drill()
                self.assert_cleanup_attempted()
                self.assert_refused(result)
                self.assertFalse(result["restore"]["notice_removed"])

    def test_restoration_mismatch_is_not_pass_even_when_writes_acknowledged(self):
        for inert in ("slowmode", "overwrite"):
            with self.subTest(inert=inert):
                self.discord.state["channel"] = copy.deepcopy(self.discord.baseline)
                self.discord.restore_inert = inert
                self.discord.events.clear()
                self.discord.freeze_started = self.discord.restoring = False
                result = self.run_drill()
                self.assert_cleanup_attempted()
                self.assert_refused(result)
                self.assertFalse(result["restore"]["restored"])
                self.assertNotEqual(self.discord.state["channel"], self.discord.baseline)
                self.assert_notice_observed_after_delete()

    def test_command_failure_still_observes_restoration_and_notice_absence(self):
        self.discord.state["commands"] = []
        result = self.run_drill()
        self.assert_refused(result)
        self.assert_cleanup_attempted()
        self.assert_baseline_restored()
        self.assert_notice_observed_after_delete()
        events = self.discord.events
        restore_index = max(i for i, e in enumerate(events) if e["method"] == "PUT")
        self.assertTrue(any(e.get("channel") == self.discord.baseline
                            for e in events[restore_index + 1:]),
                        "failure cleanup needs a channel read, not only write ACKs")

    def test_initially_locked_baseline_refuses_before_mutation(self):
        self.discord.state["channel"]["permission_overwrites"][1].update({
            "allow": "4096", "deny": str(LOCK_BITS | 1024),
        })
        before = copy.deepcopy(self.discord.state)
        result = self.run_drill()
        self.assert_refused(result)
        self.assertTrue(self.discord.events)
        self.assertTrue(all(e["method"] == "GET" for e in self.discord.events),
                        "unsupported locked baselines must refuse before any mutation")
        self.assertEqual(self.discord.state, before)

    def test_foreign_guild_channel_refuses_before_mutation(self):
        self.discord.state["channel"]["guild_id"] = fixtures.OTHER_GUILD
        result = self.run_drill()
        self.assert_refused(result)
        self.assertTrue(all(e["method"] == "GET" for e in self.discord.events))

    def test_wrong_baseline_channel_identity_refuses_before_mutation(self):
        self.discord.state["channel"]["id"] = "1000000000000000003"
        result = self.run_drill()
        self.assert_refused(result)
        self.assertTrue(all(e["method"] == "GET" for e in self.discord.events))

    def test_live_and_unknown_guilds_refuse_without_http(self):
        for guild in (fixtures.LIVE_GUILD, fixtures.OTHER_GUILD, ""):
            with self.subTest(guild=guild):
                with self.assertRaises(drill.DrillError):
                    self.run_drill(guild)
                self.assertEqual(self.discord.events, [])


if __name__ == "__main__":
    unittest.main()
