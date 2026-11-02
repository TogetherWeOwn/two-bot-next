#!/usr/bin/env python3
"""Protected acceptance: staging evidence readers refuse duplicate JSON keys.

Offline only (stdlib, fake fetches, loopback socketpairs, no network). Four
verdict-grading parsers over untrusted input use bare ``json.loads``, so a
repeated key resolves last-wins and can flip a verdict:

* ``staging_voice_smoke.py`` ``get_json`` feeds identity (``me.get("id")``),
  registration checks, health grading and ``parse_worker_readyz``;
* ``staging_probe.py`` ``parse_json`` grades /health and /readyz verdicts;
* ``sanitize_evidence_rows.py`` ``sanitize`` parses operator-pasted SQL rows;
* ``staging_voice_synthetic.py`` frame reassembly, ``rest`` and ``rest_patch``
  parse Discord REST and gateway reads into verdicts.

The combined fix must retain this file unedited (the Builder never edits
protected test files; corrections return to this card's author). It follows
the in-repo ``object_pairs_hook=_reject_duplicate_keys`` precedent: repeats
at any depth refuse with fixed vocabulary naming no key, value, path, body
or header. Valid bodies stay byte-identical.

Fixed vocabulary pinned here (exact equality, not substring):

* smoke Discord reads: ``interactions endpoint answered 200 with duplicate
  JSON object keys`` (a ``SmokeError`` from ``get_json``)
* smoke Worker reads: ``GET /health carries duplicate JSON object keys`` and
  ``GET /readyz carries duplicate JSON object keys`` (FAIL ``Result`` reasons)
* probe: ``GET /health: carries duplicate JSON object keys`` and
  ``GET /readyz: carries duplicate JSON object keys`` (probe failures)
* sanitize: ``rows carry duplicate JSON object keys`` (a ``Refused`` message)
* synthetic REST: ``Discord REST read carries duplicate JSON object keys``
  (a ``ValueError`` from ``rest``/``rest_patch``)
* synthetic gateway frames: ``gateway frame carries duplicate JSON object
  keys`` (a ``ValueError`` from ``WebSocket.recv``)

Synthetic refusals are asserted as ``ValueError`` so the Builder may use the
precedent ``DuplicateKeyError(ValueError)`` or any ``ValueError`` subclass;
the message is the contract. Every other refusal uses the module's existing
error type, so a RED run is behavioral (accepted last-wins, or refused with
the wrong vocabulary), never an import or attribute error.
"""

import io
import json
import socket
import struct
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sanitize_evidence_rows as sanitizer
import staging_probe as probe
import staging_voice_smoke as smoke
import staging_voice_synthetic as synth

SMOKE_GET_JSON_DUP = "interactions endpoint answered 200 with duplicate JSON object keys"
SMOKE_HEALTH_DUP = "GET /health carries duplicate JSON object keys"
SMOKE_READYZ_DUP = "GET /readyz carries duplicate JSON object keys"
PROBE_HEALTH_DUP = "GET /health: carries duplicate JSON object keys"
PROBE_READYZ_DUP = "GET /readyz: carries duplicate JSON object keys"
SANITIZE_DUP = "rows carry duplicate JSON object keys"
SYNTH_REST_DUP = "Discord REST read carries duplicate JSON object keys"
SYNTH_FRAME_DUP = "gateway frame carries duplicate JSON object keys"

# The shared shape token every refusal must carry (and the only
# duplicate-related wording allowed: no key, value, path or body).
DUP_TOKEN = "duplicate JSON object keys"

HOSTILE_KEY = "SENTINEL_dupkey_F3_hostile_9f31"
HOSTILE_VALUE = "SENTINEL_dupkey_F3_value_7c42"

STAGING_APP = smoke.STAGING_APPLICATION_ID
OTHER_APP = "1111111111111111111"
STAGING_GUILD = "1545644954272137297"
TOKEN = "RAW_SECRET_SENTINEL_never_disclose_bot_token"
ROOM = "1546211378430345286"

READY_COMPONENTS = '[["process", "ready"], ["gateway", "ready"]]'
DOWN_COMPONENTS = '[["process", "ready"], ["gateway", "down"]]'


def smoke_fetch(routes, requested):
    def fake(url):
        requested.append(url)
        if url not in routes:
            raise AssertionError(f"unexpected offline fixture route: {url}")
        response = routes[url]
        if isinstance(response, Exception):
            raise response
        return response
    return fake


def smoke_body(value):
    if isinstance(value, bytes):
        return 200, value
    return 200, json.dumps(value).encode()


class SmokeIdentityDuplicateTests(unittest.TestCase):
    """Duplicated identity keys refuse before the application check."""

    def setUp(self):
        guard = mock.patch.object(smoke.urllib.request.OpenerDirector, "open",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)

    def identity_url(self):
        return f"{smoke.API}/users/@me"

    def assert_get_json_refuses(self, raw):
        fetch = smoke_fetch({self.identity_url(): (200, raw)}, [])
        with self.assertRaises(smoke.SmokeError) as caught:
            smoke.get_json(fetch, self.identity_url())
        self.assertEqual(str(caught.exception), SMOKE_GET_JSON_DUP)
        self.assertIn(DUP_TOKEN, str(caught.exception))

    def test_id_conflicting_orders_refused(self):
        # Last-wins would pass one order (correct id wins) and fail the
        # other with the wrong vocabulary ("not the staging application").
        # Both must refuse with the duplicate vocabulary instead.
        good = f'{{"id": "{STAGING_APP}"}}'
        bad = f'{{"id": "{OTHER_APP}"}}'
        self.assert_get_json_refuses(
            ('{"id": "%s", "username": "x", "id": "%s"}' % (OTHER_APP, STAGING_APP)).encode())
        self.assert_get_json_refuses(
            ('{"id": "%s", "username": "x", "id": "%s"}' % (STAGING_APP, OTHER_APP)).encode())
        self.assertNotEqual(good, bad)  # the orders genuinely conflict

    def test_id_identical_repeat_refused(self):
        self.assert_get_json_refuses(
            ('{"id": "%s", "username": "x", "id": "%s"}' % (STAGING_APP, STAGING_APP)).encode())

    def test_id_nested_duplicate_refused(self):
        # The top-level id is correct, so last-wins would authenticate;
        # a nested repeat must still refuse before any grading.
        self.assert_get_json_refuses(
            ('{"id": "%s", "meta": {"a": 1, "a": 2}}' % STAGING_APP).encode())

    def test_check_identity_refuses_with_duplicate_vocabulary(self):
        for raw in (
            ('{"id": "%s", "id": "%s"}' % (OTHER_APP, STAGING_APP)).encode(),
            ('{"id": "%s", "id": "%s"}' % (STAGING_APP, OTHER_APP)).encode(),
            ('{"id": "%s", "id": "%s"}' % (STAGING_APP, STAGING_APP)).encode(),
        ):
            with self.subTest(raw=raw):
                fetch = smoke_fetch({self.identity_url(): (200, raw)}, [])
                with self.assertRaises(smoke.SmokeError) as caught:
                    smoke.check_identity(fetch)
                self.assertEqual(str(caught.exception), SMOKE_GET_JSON_DUP)

    def test_valid_identity_still_authenticates(self):
        fetch = smoke_fetch({self.identity_url(): smoke_body(
            {"id": STAGING_APP, "username": "fixture"})}, [])
        self.assertEqual(smoke.check_identity(fetch), STAGING_APP)

    def test_hostile_key_refuses_without_echo(self):
        raw = ('{"id": "%s", "%s": 1, "%s": 2}' % (STAGING_APP, HOSTILE_KEY, HOSTILE_KEY)).encode()
        fetch = smoke_fetch({self.identity_url(): (200, raw)}, [])
        with self.assertRaises(smoke.SmokeError) as caught:
            smoke.get_json(fetch, self.identity_url())
        self.assertEqual(str(caught.exception), SMOKE_GET_JSON_DUP)
        self.assertNotIn(HOSTILE_KEY, str(caught.exception))


class SmokeRegistrationDuplicateTests(unittest.TestCase):
    """Duplicated registration keys refuse instead of grading last-wins."""

    def setUp(self):
        guard = mock.patch.object(smoke.urllib.request.OpenerDirector, "open",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)

    def test_detail_name_duplicate_raises_before_grading(self):
        # The list entry names "rank"; the detail repeats "name" with a
        # conflicting second value. Last-wins would grade "resource
        # disagrees"; the duplicate must raise before any comparison.
        entry = {"id": "3000000000000000000", "name": "rank", "type": 1,
                 "guild_id": STAGING_GUILD, "version": "4000000000000000000", "options": []}
        for raw in (
            b'{"id": "3000000000000000000", "name": "rank", "name": "other",'
            b' "guild_id": "' + STAGING_GUILD.encode() + b'"}',
            b'{"id": "3000000000000000000", "name": "other", "name": "rank",'
            b' "guild_id": "' + STAGING_GUILD.encode() + b'"}',
            b'{"id": "3000000000000000000", "name": "rank", "name": "rank",'
            b' "guild_id": "' + STAGING_GUILD.encode() + b'"}',
        ):
            with self.subTest(raw=raw):
                detail_url = (f"{smoke.API}/applications/{STAGING_APP}/guilds/"
                              f"{STAGING_GUILD}/commands/3000000000000000000")
                fetch = smoke_fetch({detail_url: (200, raw)}, [])
                with self.assertRaises(smoke.SmokeError) as caught:
                    smoke.check_command(fetch, STAGING_APP, STAGING_GUILD, [entry], "rank")
                self.assertEqual(str(caught.exception), SMOKE_GET_JSON_DUP)
                self.assertNotIn("other", str(caught.exception))

    def test_detail_nested_duplicate_raises_before_grading(self):
        entry = {"id": "3000000000000000000", "name": "rank", "type": 1,
                 "guild_id": STAGING_GUILD, "version": "4000000000000000000", "options": []}
        raw = (b'{"id": "3000000000000000000", "name": "rank",'
               b' "guild_id": "' + STAGING_GUILD.encode() + b'",'
               b' "meta": {"a": 1, "a": 2}}')
        detail_url = (f"{smoke.API}/applications/{STAGING_APP}/guilds/"
                      f"{STAGING_GUILD}/commands/3000000000000000000")
        fetch = smoke_fetch({detail_url: (200, raw)}, [])
        with self.assertRaises(smoke.SmokeError) as caught:
            smoke.check_command(fetch, STAGING_APP, STAGING_GUILD, [entry], "rank")
        self.assertEqual(str(caught.exception), SMOKE_GET_JSON_DUP)

    def test_valid_detail_still_grades(self):
        entry = {"id": "3000000000000000000", "name": "rank", "type": 1,
                 "guild_id": STAGING_GUILD, "version": "4000000000000000000", "options": []}
        detail_url = (f"{smoke.API}/applications/{STAGING_APP}/guilds/"
                      f"{STAGING_GUILD}/commands/3000000000000000000")
        fetch = smoke_fetch({detail_url: smoke_body(dict(entry))}, [])
        result, detail = smoke.check_command(fetch, STAGING_APP, STAGING_GUILD, [entry], "rank")
        self.assertEqual(result.verdict, "pass")
        self.assertEqual(detail["name"], "rank")


class SmokeWorkerDuplicateTests(unittest.TestCase):
    """Duplicated Worker health/readyz keys refuse with zero grading."""

    def check(self, health_body, readyz_body=None):
        def fake_fetch_worker(url):
            if url.endswith("/health"):
                return 200, health_body
            if url.endswith("/readyz"):
                return 200, readyz_body if readyz_body is not None else b"{}"
            raise AssertionError(f"unexpected Worker GET {url}")

        with mock.patch.object(smoke, "fetch_worker", side_effect=fake_fetch_worker):
            return smoke.check_worker("https://two-bot-next-staging.5150.workers.dev")

    def test_health_status_conflicting_orders_refused(self):
        for raw in (b'{"status": "bad", "status": "ok"}',
                    b'{"status": "ok", "status": "bad"}'):
            with self.subTest(raw=raw):
                results, info = self.check(raw)
                self.assertIsNone(info)
                self.assertGreaterEqual(len(results), 1)
                self.assertEqual(results[0].verdict, "fail")
                self.assertEqual(results[0].reason, SMOKE_HEALTH_DUP)
                self.assertNotIn("bad", results[0].reason)
                self.assertEqual(len(results), 1)

    def test_health_status_identical_repeat_refused(self):
        results, info = self.check(b'{"status": "ok", "status": "ok"}')
        self.assertIsNone(info)
        self.assertEqual(results[0].verdict, "fail")
        self.assertEqual(results[0].reason, SMOKE_HEALTH_DUP)

    def test_health_nested_duplicate_refused(self):
        # Last-wins decodes an object that is not exactly {"status": "ok"},
        # so today this FAILs with the generic shape message; it must FAIL
        # with the duplicate vocabulary instead.
        results, info = self.check(b'{"status": "ok", "meta": {"a": 1, "a": 2}}')
        self.assertIsNone(info)
        self.assertEqual(results[0].verdict, "fail")
        self.assertEqual(results[0].reason, SMOKE_HEALTH_DUP)

    def test_readyz_components_conflicting_orders_refused(self):
        for components in (
            '{"components": %s, "components": %s}' % (DOWN_COMPONENTS, READY_COMPONENTS),
            '{"components": %s, "components": %s}' % (READY_COMPONENTS, DOWN_COMPONENTS),
        ):
            with self.subTest(components=components):
                results, info = self.check(b'{"status": "ok"}', components.encode())
                self.assertIsNone(info)
                self.assertEqual(len(results), 2)
                self.assertEqual(results[0].verdict, "pass")
                self.assertEqual(results[1].verdict, "fail")
                self.assertEqual(results[1].reason, SMOKE_READYZ_DUP)

    def test_readyz_components_identical_repeat_refused(self):
        body = ('{"components": %s, "components": %s}'
                % (READY_COMPONENTS, READY_COMPONENTS)).encode()
        results, info = self.check(b'{"status": "ok"}', body)
        self.assertIsNone(info)
        self.assertEqual(results[1].verdict, "fail")
        self.assertEqual(results[1].reason, SMOKE_READYZ_DUP)

    def test_readyz_nested_duplicate_refused(self):
        body = ('{"components": %s, "meta": {"a": 1, "a": 2}}' % READY_COMPONENTS).encode()
        results, info = self.check(b'{"status": "ok"}', body)
        self.assertIsNone(info)
        self.assertEqual(results[1].verdict, "fail")
        self.assertEqual(results[1].reason, SMOKE_READYZ_DUP)

    def test_valid_worker_reads_still_pass_byte_identical(self):
        body = ('{"components": %s, "build_revision": "abc123"}' % READY_COMPONENTS).encode()
        results, info = self.check(b'{"status": "ok"}', body)
        self.assertIsNotNone(info)
        self.assertTrue(all(r.verdict == "pass" for r in results))
        self.assertEqual(info["components"], {"process": "ready", "gateway": "ready"})
        self.assertEqual(info["build_revision"], "abc123")

    def test_hostile_key_refuses_without_echo(self):
        raw = ('{"status": "ok", "%s": 1, "%s": 2}' % (HOSTILE_KEY, HOSTILE_KEY)).encode()
        results, info = self.check(raw)
        self.assertIsNone(info)
        self.assertEqual(results[0].reason, SMOKE_HEALTH_DUP)
        self.assertNotIn(HOSTILE_KEY, results[0].reason)


class SmokeMainDuplicateTests(unittest.TestCase):
    """End to end: a refused identity grades nothing and writes no receipt."""

    def setUp(self):
        guard = mock.patch.object(smoke.urllib.request.OpenerDirector, "open",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")

    def test_duplicate_identity_fails_closed_with_no_receipt(self):
        import os

        raw = ('{"id": "%s", "id": "%s"}' % (OTHER_APP, STAGING_APP)).encode()
        # Last-wins would authenticate (the staging id wins) and proceed to
        # the registry; the empty list keeps that wrong path inside the
        # fixture so the failure lands on the contract assertions below,
        # not on the offline guard.
        app_url = (f"{smoke.API}/applications/{STAGING_APP}/guilds/"
                   f"{STAGING_GUILD}/commands")
        routes = {f"{smoke.API}/users/@me": (200, raw), app_url: (200, b"[]")}
        requested = []
        with mock.patch.dict(os.environ, {"DISCORD_STAGING_BOT_TOKEN": TOKEN}, clear=True):
            out = io.StringIO()
            with redirect_stdout(out):
                code = smoke.main(["--guild-id", STAGING_GUILD, "--staging-url", "",
                                   "--evidence", self.evidence],
                                  fetch_fn=smoke_fetch(routes, requested))
        text = out.getvalue()
        self.assertEqual(code, 1)
        self.assertIn(f"FAIL interactions: {SMOKE_GET_JSON_DUP}", text)
        self.assertNotIn("Traceback", text)
        self.assertNotIn(TOKEN, text)
        self.assertNotIn(OTHER_APP, text)
        self.assertFalse(Path(self.evidence).exists())


class ProbeDuplicateTests(unittest.TestCase):
    """Duplicated probe bodies refuse before health/readyz grading."""

    def reply(self, body, status=200):
        return probe.Reply(status, {}, body)

    def run_with(self, health=None, readyz=None):
        def fetch(method, path):
            if path == "/health" and health is not None:
                return self.reply(health)
            if path == "/readyz" and readyz is not None:
                return self.reply(readyz)
            if path == "/health":
                return self.reply('{"status": "ok"}')
            if path == "/readyz":
                return self.reply('{"components": %s}' % READY_COMPONENTS)
            if path == "/healthz":
                return self.reply("ok\n") if method == "GET" else self.reply("")
            return probe.Reply(404, {}, "not found\n")

        return {r["name"]: r for r in probe.run_probes(fetch, unknown_slug="probe-0123456789ab")}

    def test_health_status_conflicting_orders_refused(self):
        for raw in ('{"status": "bad", "status": "ok"}',
                    '{"status": "ok", "status": "bad"}'):
            with self.subTest(raw=raw):
                results = self.run_with(health=raw)
                self.assertEqual(results["health"]["verdict"], "FAIL")
                self.assertIn(PROBE_HEALTH_DUP, results["health"]["failures"])
                self.assertNotIn("bad", json.dumps(results["health"]["failures"]))

    def test_health_status_identical_repeat_refused(self):
        results = self.run_with(health='{"status": "ok", "status": "ok"}')
        self.assertEqual(results["health"]["verdict"], "FAIL")
        self.assertIn(PROBE_HEALTH_DUP, results["health"]["failures"])

    def test_health_nested_duplicate_refused(self):
        results = self.run_with(health='{"status": "ok", "meta": {"a": 1, "a": 2}}')
        self.assertEqual(results["health"]["verdict"], "FAIL")
        self.assertIn(PROBE_HEALTH_DUP, results["health"]["failures"])

    def test_readyz_components_conflicting_orders_refused(self):
        for raw in ('{"components": %s, "components": %s}' % (DOWN_COMPONENTS, READY_COMPONENTS),
                    '{"components": %s, "components": %s}' % (READY_COMPONENTS, DOWN_COMPONENTS)):
            with self.subTest(raw=raw):
                results = self.run_with(readyz=raw)
                self.assertEqual(results["readyz"]["verdict"], "FAIL")
                self.assertIn(PROBE_READYZ_DUP, results["readyz"]["failures"])
                # Zero grading: no component breakdown is observed.
                self.assertNotIn("components", results["readyz"].get("observed", {}))
                self.assertNotIn("ready", json.dumps(results["readyz"].get("observed", {})))

    def test_readyz_components_identical_repeat_refused(self):
        raw = '{"components": %s, "components": %s}' % (READY_COMPONENTS, READY_COMPONENTS)
        results = self.run_with(readyz=raw)
        self.assertEqual(results["readyz"]["verdict"], "FAIL")
        self.assertIn(PROBE_READYZ_DUP, results["readyz"]["failures"])
        self.assertNotIn("components", results["readyz"].get("observed", {}))

    def test_readyz_nested_duplicate_refused(self):
        raw = '{"components": %s, "meta": {"a": 1, "a": 2}}' % READY_COMPONENTS
        results = self.run_with(readyz=raw)
        self.assertEqual(results["readyz"]["verdict"], "FAIL")
        self.assertIn(PROBE_READYZ_DUP, results["readyz"]["failures"])
        self.assertNotIn("components", results["readyz"].get("observed", {}))

    def test_valid_bodies_still_pass_byte_identical(self):
        results = self.run_with()
        self.assertEqual(results["health"]["verdict"], "PASS")
        self.assertEqual(results["readyz"]["verdict"], "PASS")
        self.assertEqual(results["readyz"]["observed"]["components"],
                         {"process": "ready", "gateway": "ready"})
        self.assertEqual(results["readyz"]["observed"]["ready"], True)

    def test_hostile_key_refuses_without_echo(self):
        raw = '{"status": "ok", "%s": 1, "%s": 2}' % (HOSTILE_KEY, HOSTILE_KEY)
        results = self.run_with(health=raw)
        self.assertEqual(results["health"]["verdict"], "FAIL")
        self.assertIn(PROBE_HEALTH_DUP, results["health"]["failures"])
        self.assertNotIn(HOSTILE_KEY, json.dumps(results))


class SanitizeDuplicateTests(unittest.TestCase):
    """Duplicated row keys refuse before any sanitized row is produced."""

    KEY = "100000000000000001:900000000000001111:member_join:RAW_KEY_SENTINEL"
    TS = "2026-10-09T20:00:01+00:00"

    def row_json(self, *pairs):
        return "[" + "{" + ", ".join('%s: %s' % (json.dumps(k), json.dumps(v))
                                     for k, v in pairs) + "}" + "]"

    def test_event_type_conflicting_orders_refused(self):
        for first, second in (("member_join", "first_message"),
                              ("first_message", "member_join")):
            with self.subTest(first=first, second=second):
                raw = self.row_json(("idempotency_key", self.KEY),
                                    ("event_type", first),
                                    ("event_type", second),
                                    ("recorded_at", self.TS))
                with self.assertRaises(sanitizer.Refused) as caught:
                    sanitizer.sanitize(raw)
                self.assertEqual(str(caught.exception), SANITIZE_DUP)

    def test_event_type_identical_repeat_refused(self):
        raw = self.row_json(("idempotency_key", self.KEY),
                            ("event_type", "member_join"),
                            ("event_type", "member_join"),
                            ("recorded_at", self.TS))
        with self.assertRaises(sanitizer.Refused) as caught:
            sanitizer.sanitize(raw)
        self.assertEqual(str(caught.exception), SANITIZE_DUP)

    def test_recorded_at_conflicting_orders_refused(self):
        early, late = "2026-10-09T20:00:01+00:00", "2026-10-09T21:00:01+00:00"
        for first, second in ((early, late), (late, early)):
            with self.subTest(first=first, second=second):
                raw = self.row_json(("idempotency_key", self.KEY),
                                    ("event_type", "member_join"),
                                    ("recorded_at", first),
                                    ("recorded_at", second))
                with self.assertRaises(sanitizer.Refused) as caught:
                    sanitizer.sanitize(raw)
                self.assertEqual(str(caught.exception), SANITIZE_DUP)

    def test_nested_duplicate_refused_with_duplicate_vocabulary(self):
        # Today this is refused as "unknown event type" (the nested object
        # fails validation after last-wins decoding). The duplicate must be
        # detected first, with the duplicate vocabulary.
        raw = ('[{"idempotency_key": "%s", "event_type": {"a": 1, "a": 2},'
               ' "recorded_at": "%s"}]' % (self.KEY, self.TS))
        with self.assertRaises(sanitizer.Refused) as caught:
            sanitizer.sanitize(raw)
        self.assertEqual(str(caught.exception), SANITIZE_DUP)

    def test_valid_rows_stay_byte_identical(self):
        raw = json.dumps([{"idempotency_key": self.KEY, "event_type": "member_join",
                           "recorded_at": self.TS}])
        self.assertEqual(sanitizer.sanitize(raw),
                         {"rows": [{"ordinal": 0, "event_type": "member_join",
                                     "recorded_at": "2026-10-09T20:00:01.000Z"}],
                          "truncated": False})

    def test_hostile_key_refuses_without_echo(self):
        raw = ('[{"idempotency_key": "%s", "event_type": "member_join",'
               ' "%s": 1, "%s": 2, "recorded_at": "%s"}]'
               % (self.KEY, HOSTILE_KEY, HOSTILE_KEY, self.TS))
        with self.assertRaises(sanitizer.Refused) as caught:
            sanitizer.sanitize(raw)
        self.assertEqual(str(caught.exception), SANITIZE_DUP)
        self.assertNotIn(HOSTILE_KEY, str(caught.exception))
        self.assertNotIn(self.KEY, str(caught.exception))

    def test_cli_refusal_writes_no_artifact_and_no_echo(self):
        import os
        import subprocess

        script = Path(__file__).with_name("sanitize_evidence_rows.py")
        raw = ('[{"idempotency_key": "%s", "event_type": "member_join",'
               ' "event_type": "first_message", "recorded_at": "%s"}]'
               % (self.KEY, self.TS)).encode()
        env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}
        done = subprocess.run([sys.executable, str(script)], input=raw,
                              capture_output=True, env=env, check=False, timeout=10)
        self.assertEqual(done.returncode, 2)
        self.assertEqual(done.stdout, b"")
        self.assertNotIn(self.KEY.encode(), done.stderr)
        self.assertNotIn(HOSTILE_VALUE.encode(), done.stderr)


class SyntheticRestDuplicateTests(unittest.TestCase):
    """Duplicated Discord REST keys refuse instead of returning last-wins."""

    def response(self, payload, seen, status=200):
        class _Resp:
            def read(self, n):
                seen.append(n)
                return payload

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        resp = _Resp()
        resp.status = status
        return resp

    def assert_rest_refuses(self, raw):
        seen = []
        with mock.patch.object(synth, "_open_no_redirect",
                               return_value=self.response(raw, seen)):
            with self.assertRaises(ValueError) as caught:
                synth.rest("t", f"/channels/{ROOM}")
        self.assertEqual(str(caught.exception), SYNTH_REST_DUP)
        self.assertIn(DUP_TOKEN, str(caught.exception))
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def assert_patch_refuses(self, raw):
        seen = []
        with mock.patch.object(synth, "_open_no_redirect",
                               return_value=self.response(raw, seen)):
            with self.assertRaises(ValueError) as caught:
                synth.rest_patch("t", f"/channels/{ROOM}", {"name": "x"})
        self.assertEqual(str(caught.exception), SYNTH_REST_DUP)
        self.assertEqual(seen, [synth.MAX_BODY_BYTES + 1])

    def test_rest_id_conflicting_orders_refused(self):
        other = "9999999999999999999"
        self.assert_rest_refuses(
            ('{"id": "%s", "name": "x", "id": "%s"}' % (ROOM, other)).encode())
        self.assert_rest_refuses(
            ('{"id": "%s", "name": "x", "id": "%s"}' % (other, ROOM)).encode())

    def test_rest_id_identical_repeat_refused(self):
        self.assert_rest_refuses(
            ('{"id": "%s", "name": "x", "id": "%s"}' % (ROOM, ROOM)).encode())

    def test_rest_nested_duplicate_refused(self):
        self.assert_rest_refuses(
            ('{"id": "%s", "meta": {"a": 1, "a": 2}}' % ROOM).encode())

    def test_patch_conflicting_orders_refused(self):
        other = "9999999999999999999"
        self.assert_patch_refuses(
            ('{"id": "%s", "name": "x", "id": "%s"}' % (ROOM, other)).encode())
        self.assert_patch_refuses(
            ('{"id": "%s", "name": "x", "id": "%s"}' % (other, ROOM)).encode())

    def test_patch_identical_and_nested_refused(self):
        self.assert_patch_refuses(
            ('{"id": "%s", "name": "x", "id": "%s"}' % (ROOM, ROOM)).encode())
        self.assert_patch_refuses(
            ('{"id": "%s", "meta": {"a": 1, "a": 2}}' % ROOM).encode())

    def test_valid_rest_and_patch_stay_byte_identical(self):
        body = json.dumps({"id": ROOM, "name": "Create a Lobby"}).encode()
        with mock.patch.object(synth, "_open_no_redirect",
                               return_value=self.response(body, [])):
            self.assertEqual(synth.rest("t", f"/channels/{ROOM}")["id"], ROOM)
        with mock.patch.object(synth, "_open_no_redirect",
                               return_value=self.response(body, [], status=200)):
            status, parsed = synth.rest_patch("t", f"/channels/{ROOM}", {"name": "x"})
        self.assertEqual(status, 200)
        self.assertEqual(parsed, {"id": ROOM, "name": "Create a Lobby"})

    def test_hostile_key_refuses_without_echo(self):
        raw = ('{"id": "%s", "%s": 1, "%s": 2}' % (ROOM, HOSTILE_KEY, HOSTILE_KEY)).encode()
        seen = []
        with mock.patch.object(synth, "_open_no_redirect",
                               return_value=self.response(raw, seen)):
            with self.assertRaises(ValueError) as caught:
                synth.rest("t", f"/channels/{ROOM}")
        self.assertEqual(str(caught.exception), SYNTH_REST_DUP)
        self.assertNotIn(HOSTILE_KEY, str(caught.exception))
        self.assertNotIn(ROOM, str(caught.exception))


class SyntheticFrameDuplicateTests(unittest.TestCase):
    """Duplicated gateway-frame keys refuse instead of reassembling last-wins."""

    def frame(self, opcode, payload, fin=True):
        head = bytes([(0x80 if fin else 0) | opcode])
        n = len(payload)
        if n < 126:
            head += bytes([n])
        elif n < 65536:
            head += bytes([126]) + struct.pack(">H", n)
        else:
            head += bytes([127]) + struct.pack(">Q", n)
        return head + payload

    def recv_once(self, payload):
        ours, theirs = socket.socketpair()
        self.addCleanup(ours.close)
        self.addCleanup(theirs.close)
        ws = synth.WebSocket.__new__(synth.WebSocket)
        ws.sock, ws.lock = ours, threading.Lock()
        theirs.sendall(self.frame(0x1, payload))
        return ws.recv()

    def assert_frame_refuses(self, payload):
        with self.assertRaises(ValueError) as caught:
            self.recv_once(payload)
        self.assertEqual(str(caught.exception), SYNTH_FRAME_DUP)
        self.assertIn(DUP_TOKEN, str(caught.exception))

    def test_op_conflicting_orders_refused(self):
        self.assert_frame_refuses(b'{"op": 0, "t": "X", "op": 10}')
        self.assert_frame_refuses(b'{"op": 10, "t": "X", "op": 0}')

    def test_op_identical_repeat_refused(self):
        self.assert_frame_refuses(b'{"op": 10, "t": "X", "op": 10}')

    def test_nested_d_duplicate_refused(self):
        self.assert_frame_refuses(b'{"op": 0, "t": "X", "d": {"a": 1, "a": 2}}')

    def test_fragmented_duplicate_refused(self):
        # Reassembly joins fragments before parsing, so a key split across
        # fragments still counts as a repeat.
        message = b'{"op": 0, "t": "X", "op": 10}'
        ours, theirs = socket.socketpair()
        self.addCleanup(ours.close)
        self.addCleanup(theirs.close)
        ws = synth.WebSocket.__new__(synth.WebSocket)
        ws.sock, ws.lock = ours, threading.Lock()
        theirs.sendall(self.frame(0x1, message[:12], fin=False) + self.frame(0x0, message[12:]))
        with self.assertRaises(ValueError) as caught:
            ws.recv()
        self.assertEqual(str(caught.exception), SYNTH_FRAME_DUP)

    def test_valid_frame_still_reassembles(self):
        got = self.recv_once(b'{"op": 10, "d": {"heartbeat_interval": 41250}}')
        self.assertEqual(got["op"], 10)
        self.assertEqual(got["d"]["heartbeat_interval"], 41250)

    def test_hostile_key_refuses_without_echo(self):
        payload = ('{"op": 10, "%s": 1, "%s": 2}' % (HOSTILE_KEY, HOSTILE_KEY)).encode()
        with self.assertRaises(ValueError) as caught:
            self.recv_once(payload)
        self.assertEqual(str(caught.exception), SYNTH_FRAME_DUP)
        self.assertNotIn(HOSTILE_KEY, str(caught.exception))


if __name__ == "__main__":
    unittest.main()
