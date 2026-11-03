#!/usr/bin/env python3
"""Acceptance tests for the staging help-embed smoke.

Offline only: Discord is never contacted. The transport is faked at
``http.client`` (the same FakeTransport pattern as
``test_staging_voice_synthetic_redirect_refusal.py``) so the real urllib
opener/handler chain runs above it, and ``run_smoke`` is driven with a
stubbed REST layer for the post/read/assert/delete flow.

Contract under test (``scripts/staging_help_embed_smoke.py``):

* ``evaluate`` passes a fully branded run: both messages present, every
  embed colored 0xFF0E52, footer text `Together We Own`, the footer icon URL
  exactly the Icon 1 asset, the icon fetch 200 image/png, provenance booleans
  recorded. Any deviation fails exactly its own check name, never a blanket.
* The brand expectations are pinned to ``crates/discord/src/brand.rs``: the
  test reads the consts out of the Rust source so a brand change that updates
  the source but not the smoke (or vice versa) fails loudly.
* Fences fail closed before any post: a non-staging channel refuses and
  sends nothing; a non-digit id refuses; the live guild refuses.
* The footer icon fetch never carries the bot token: only the intended URL
  is contacted, with no Authorization header, and a 3xx is refused without
  touching the redirect target.
* Every response read is capped at 64 KiB and refuses duplicate JSON keys;
  refusals use fixed vocabulary and never echo tokens, bodies or URLs.
* Cleanup is best effort: a failed delete records `delete_failed` without
  hiding the brand verdict; both message ids reach the evidence JSON and
  the step summary together with every assertion.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_staging_help_embed_smoke.py' -v
"""

import email.message
import http.client
import importlib
import io
import json
import os
import sys
import unittest
import urllib.request
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
smoke = importlib.import_module("staging_help_embed_smoke")

COLOR = smoke.EXPECTED_COLOR
TEXT = smoke.EXPECTED_FOOTER_TEXT
ICON = smoke.EXPECTED_ICON_URL
GUILD = smoke.STAGING_GUILD_ID
LIVE = smoke.LIVE_GUILD_ID

TOKEN = "help-embed-fake-token-not-a-secret"
EVIL = "https://evil.example/collect"
BODY_SENTINEL = "response-body-must-never-leak"
REDIRECT_STATUSES = (301, 302, 303, 307, 308)


def branded_message(message_id):
    return {
        "id": message_id,
        "embeds": [
            {
                "color": COLOR,
                "footer": {"text": TEXT, "icon_url": ICON},
            }
        ],
    }


def good_meta(**overrides):
    meta = {"help_builder_embedded": True, "help_detail_builder_embedded": False,
            "help_topic": "some-topic"}
    meta.update(overrides)
    return meta


class EvaluateTest(unittest.TestCase):
    def test_a_fully_branded_run_passes_every_check(self):
        checks = smoke.evaluate(branded_message("11"), branded_message("22"),
                                200, "image/png", good_meta())
        self.assertTrue(all(item["verdict"] == "PASS" for item in checks))
        names = [item["check"] for item in checks]
        self.assertEqual(names, [
            "help_posted", "help_color", "help_footer_text", "help_footer_icon_url",
            "help_detail_posted", "help_detail_color", "help_detail_footer_text",
            "help_detail_footer_icon_url", "footer_icon_fetch", "provenance_recorded",
        ])

    def test_a_wrong_color_fails_only_its_own_check(self):
        bad = branded_message("11")
        bad["embeds"][0]["color"] = 0
        checks = smoke.evaluate(bad, branded_message("22"), 200, "image/png", good_meta())
        failed = [item["check"] for item in checks if item["verdict"] == "FAIL"]
        self.assertEqual(failed, ["help_color"])

    def test_a_wrong_footer_text_fails_only_its_own_check(self):
        bad = branded_message("11")
        bad["embeds"][0]["footer"]["text"] = "Someone Else"
        checks = smoke.evaluate(bad, branded_message("22"), 200, "image/png", good_meta())
        failed = [item["check"] for item in checks if item["verdict"] == "FAIL"]
        self.assertEqual(failed, ["help_footer_text"])

    def test_a_wrong_footer_icon_url_fails_only_its_own_check(self):
        bad = branded_message("22")
        bad["embeds"][0]["footer"]["icon_url"] = "https://example.invalid/other.png"
        checks = smoke.evaluate(branded_message("11"), bad, 200, "image/png", good_meta())
        failed = [item["check"] for item in checks if item["verdict"] == "FAIL"]
        self.assertEqual(failed, ["help_detail_footer_icon_url"])

    def test_icon_fetch_must_be_200_image_png(self):
        for status, content_type in ((404, "image/png"), (200, "text/html")):
            with self.subTest(status=status, content_type=content_type):
                checks = smoke.evaluate(branded_message("11"), branded_message("22"),
                                        status, content_type, good_meta())
                failed = [item["check"] for item in checks if item["verdict"] == "FAIL"]
                self.assertEqual(failed, ["footer_icon_fetch"])
        checks = smoke.evaluate(branded_message("11"), branded_message("22"),
                                200, "image/png; charset=binary", good_meta())
        self.assertEqual([item["check"] for item in checks if item["verdict"] == "FAIL"], [])

    def test_missing_ids_fail_their_posted_checks(self):
        checks = smoke.evaluate({"embeds": []}, branded_message("22"),
                                200, "image/png", good_meta())
        failed = [item["check"] for item in checks if item["verdict"] == "FAIL"]
        self.assertEqual(failed, ["help_posted", "help_color", "help_footer_text",
                                  "help_footer_icon_url"])

    def test_missing_provenance_fails_the_provenance_check(self):
        checks = smoke.evaluate(branded_message("11"), branded_message("22"),
                                200, "image/png", {})
        failed = [item["check"] for item in checks if item["verdict"] == "FAIL"]
        self.assertEqual(failed, ["provenance_recorded"])

    def test_brand_expectations_match_the_renderer_source_of_truth(self):
        brand = Path(__file__).resolve().parent.parent / "crates" / "discord" / "src" / "brand.rs"
        source = brand.read_text(encoding="utf-8")
        self.assertIn(f"0x{COLOR:06X}", source)
        self.assertIn(ICON, source)
        self.assertIn(f'"{TEXT}"', source)

    def test_summary_names_ids_verdicts_and_provenance(self):
        checks = smoke.evaluate(branded_message("11"), branded_message("22"),
                                200, "image/png", good_meta())
        summary = smoke.summarize(checks, "11", "22", "some-topic", good_meta())
        self.assertIn("message `11`", summary)
        self.assertIn("message `22`", summary)
        self.assertIn("All brand assertions passed.", summary)
        self.assertIn("help_builder_embedded=True", summary)
        self.assertIn("detail_builder_embedded=False", summary)
        bad = dict(checks[1])
        bad["verdict"] = "FAIL"
        summary = smoke.summarize([bad], "11", "22", "some-topic", good_meta())
        self.assertIn("FAILED: help_color", summary)


class FenceTest(unittest.TestCase):
    def _payloads(self):
        embed = {"color": COLOR, "description": "x",
                 "footer": {"text": TEXT, "icon_url": ICON}}
        return {"help": {"topic": "help", "posted_embeds": [embed]},
                "help_detail": {"topic": "t", "posted_embeds": [embed]},
                "meta": {"help_builder_embedded": True,
                         "help_detail_builder_embedded": False}}

    def test_a_non_digit_channel_id_refuses(self):
        with self.assertRaises(smoke.Refusal):
            smoke.run_smoke(TOKEN, "not-a-channel", self._payloads())

    def test_a_non_staging_channel_refuses_before_any_post(self):
        calls = []

        def fake_rest(_token, method, path, _payload=None):
            calls.append((method, path))
            if method == "GET" and path.startswith("/channels/"):
                return 200, {"id": "9", "guild_id": LIVE}
            raise AssertionError("no post may follow a fence refusal")

        with mock.patch.object(smoke, "_rest", side_effect=fake_rest):
            with self.assertRaises(smoke.Refusal):
                smoke.run_smoke(TOKEN, "123", self._payloads())
        self.assertEqual(calls, [("GET", "/channels/123")])

    def test_a_guildless_channel_refuses(self):
        with mock.patch.object(smoke, "_rest", return_value=(200, {"id": "9"})):
            with self.assertRaises(smoke.Refusal):
                smoke.run_smoke(TOKEN, "123", self._payloads())

    def test_unconfigured_smoke_skips_with_notice_not_failure(self):
        evidence = Path("help-embed-smoke-skip-evidence.json")
        self.addCleanup(lambda: evidence.unlink(missing_ok=True))
        with mock.patch.dict(os.environ, {}, clear=True):
            stdout = io.StringIO()
            with mock.patch.object(sys, "stdout", stdout):
                code = smoke.main(["--payloads", "x", "--evidence", str(evidence),
                                   "--channel-id", "", "--summary", ""])
        self.assertEqual(code, 0)
        self.assertIn("::notice::", stdout.getvalue())
        saved = json.loads(evidence.read_text(encoding="utf-8"))
        self.assertEqual(saved["verdict"], "SKIP")

    def test_required_but_unconfigured_smoke_refuses(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            stdout = io.StringIO()
            with mock.patch.object(sys, "stdout", stdout):
                code = smoke.main(["--payloads", "x", "--evidence", "x",
                                   "--channel-id", "", "--required", "true",
                                   "--summary", ""])
        self.assertEqual(code, 2)
        self.assertIn("::error::", stdout.getvalue())


class OversizeAndDuplicateKeyTest(unittest.TestCase):
    def test_an_oversize_payload_file_refuses(self):
        path = Path("oversize-payloads.json")
        path.write_bytes(b" " * (smoke.MAX_BODY_BYTES + 1))
        self.addCleanup(lambda: path.unlink(missing_ok=True))
        with self.assertRaises(smoke.BodyTooLarge):
            smoke._load_payloads(str(path))

    def test_an_exact_cap_payload_file_decodes(self):
        body = b'{"help": 1}'
        path = Path("exact-cap-payloads.json")
        path.write_bytes(body + b" " * (smoke.MAX_BODY_BYTES - len(body)))
        self.addCleanup(lambda: path.unlink(missing_ok=True))
        self.assertEqual(smoke._load_payloads(str(path)), {"help": 1})

    def test_duplicate_payload_keys_refuse(self):
        path = Path("dup-key-payloads.json")
        path.write_text('{"help": 1, "help": 2}', encoding="utf-8")
        self.addCleanup(lambda: path.unlink(missing_ok=True))
        with self.assertRaises(smoke.DuplicateKeyError):
            smoke._load_payloads(str(path))

    def test_a_unicode_escaped_duplicate_key_refuses(self):
        with self.assertRaises(smoke.DuplicateKeyError):
            json.loads('{"a": 1, "\\u0061": 2}',
                       object_pairs_hook=smoke._reject_duplicate_keys)


def headers_for(**items):
    message = email.message.Message()
    for name, value in items.items():
        message[name.replace("_", "-")] = value
    return message


class FakeResponse:
    """Minimal HTTPResponse stand-in for the urllib machinery above it."""

    def __init__(self, status, headers, body):
        self.status = status
        self.code = status
        self.reason = "fake"
        self.msg = self.reason
        self._headers = headers
        self._body = io.BytesIO(body)
        self.url = None

    def info(self):
        return self._headers

    def getheaders(self):
        return list(self._headers.items())

    def read(self, size=-1):
        return self._body.read(size)

    def close(self):
        self._body.close()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()
        return False


class FakeTransport:
    """Replaces http.client connections; records every attempted destination."""

    def __init__(self, script):
        # script: list of (status, headers, body) returned in request order.
        self.script = list(script)
        self.requests = []  # (scheme, host, method, selector, body, headers)

    def connection_class(self, scheme):
        transport = self

        class FakeConnection(http.client.HTTPConnection):

            def __init__(self, host, timeout=None, **_kwargs):
                self.host = host
                self.timeout = timeout
                self.sock = None
                self._response = None

            def set_debuglevel(self, _level):
                pass

            def set_tunnel(self, *_args, **_kwargs):
                raise AssertionError("proxy tunnel attempted in an offline test")

            def request(self, method, selector, body=None, headers=None,
                        encode_chunked=False):
                transport.requests.append(
                    (scheme, self.host, method, selector, body, headers))
                if not transport.script:
                    raise AssertionError(
                        f"unscripted {method} request to {self.host}{selector}")
                status, response_headers, response_body = transport.script.pop(0)
                self._response = FakeResponse(status, response_headers, response_body)

            def getresponse(self):
                return self._response

            def close(self):
                pass

        return FakeConnection


class TransportTest(unittest.TestCase):
    def setUp(self):
        proxies = mock.patch.object(urllib.request, "getproxies", return_value={})
        proxies.start()
        self.addCleanup(proxies.stop)

    def run_with_transport(self, transport, func, *args, **kwargs):
        with mock.patch.object(http.client, "HTTPSConnection",
                               transport.connection_class("https")), \
                mock.patch.object(http.client, "HTTPConnection",
                                  transport.connection_class("http")):
            return func(*args, **kwargs), transport

    def redirect_transport(self, status, location, follow_body=b'{"id": "5001"}'):
        # A followed redirect would land on a scripted 200: exactly the false
        # "successful read" outcome these tests exist to forbid.
        return FakeTransport([
            (status, headers_for(Location=location), BODY_SENTINEL.encode()),
            (200, headers_for(), follow_body),
        ])

    def assert_no_leak(self, text, location=EVIL):
        # Refusal messages carry a fixed status only: never the token, the
        # redirect target, or the response body.
        for sentinel in (TOKEN, location, BODY_SENTINEL):
            self.assertNotIn(sentinel, text)

    def test_each_reader_refuses_each_redirect_before_a_second_destination(self):
        for status in REDIRECT_STATUSES:
            with self.subTest(status=status):
                transport = self.redirect_transport(status, EVIL)
                with self.assertRaises(smoke.RedirectRefused) as caught:
                    self.run_with_transport(
                        transport, smoke._rest, TOKEN, "GET", "/channels/123")
                self.assert_no_leak(str(caught.exception))
                self.assertEqual(len(transport.requests), 1)

    def test_the_icon_fetch_refuses_redirects_without_credentials(self):
        for status in REDIRECT_STATUSES:
            with self.subTest(status=status):
                transport = self.redirect_transport(status, EVIL)
                with self.assertRaises(smoke.RedirectRefused) as caught:
                    self.run_with_transport(transport, smoke._fetch_icon)
                self.assert_no_leak(str(caught.exception))
                [(_scheme, host, _method, _selector, _body, headers)] = transport.requests
                self.assertEqual(host, "raw.githubusercontent.com")
                self.assertNotIn("authorization",
                                 {str(name).lower() for name, _ in headers.items()})

    def test_oversize_bodies_are_refused(self):
        transport = FakeTransport([
            (200, headers_for(), b"x" * (smoke.MAX_BODY_BYTES + 1)),
        ])
        with self.assertRaises(smoke.BodyTooLarge):
            self.run_with_transport(transport, smoke._rest, TOKEN, "GET", "/channels/123")

    def test_an_exact_cap_body_decodes(self):
        body = b'{"id": "9"}' + b" " * (smoke.MAX_BODY_BYTES - len(b'{"id": "9"}'))
        transport = FakeTransport([(200, headers_for(), body)])
        (status, decoded), _transport = self.run_with_transport(
            transport, smoke._rest, TOKEN, "GET", "/channels/123")
        self.assertEqual((status, decoded), (200, {"id": "9"}))

    def test_duplicate_rest_keys_are_refused(self):
        transport = FakeTransport([
            (200, headers_for(), b'{"id": "1", "id": "2"}'),
        ])
        with self.assertRaises(smoke.DuplicateKeyError):
            self.run_with_transport(transport, smoke._rest, TOKEN, "GET", "/channels/123")


class RunSmokeFlowTest(unittest.TestCase):
    def _payloads(self):
        embed = {"color": COLOR, "description": "x",
                 "footer": {"text": TEXT, "icon_url": ICON}}
        return {"help": {"topic": "help", "posted_embeds": [embed]},
                "help_detail": {"topic": "t", "posted_embeds": [embed]},
                "meta": {"help_builder_embedded": True,
                         "help_detail_builder_embedded": False}}

    def _drive(self, fail_delete_second=False):
        calls = []
        state = {"posted": 0}
        payloads = self._payloads()

        def fake_rest(token, method, path, payload=None):
            self.assertEqual(token, TOKEN)
            calls.append((method, path))
            if method == "GET" and path == "/channels/42":
                return 200, {"id": "42", "guild_id": GUILD}
            if method == "POST" and path == "/channels/42/messages":
                expected = payloads["help" if state["posted"] == 0 else "help_detail"]
                self.assertEqual(payload["embeds"], expected["posted_embeds"])
                state["posted"] += 1
                return 200, {"id": f"msg-{state['posted']}"}
            if method == "GET" and path.startswith("/channels/42/messages/"):
                return 200, branded_message(path.rsplit("/", 1)[1])
            if method == "DELETE":
                self.assertIsNone(payload)
                if fail_delete_second and path.endswith("msg-2"):
                    raise smoke.Refusal("DELETE failed")
                return 204, {}
            raise AssertionError(f"unexpected call {method} {path}")

        with mock.patch.object(smoke, "_rest", side_effect=fake_rest), \
             mock.patch.object(smoke, "_fetch_icon",
                               return_value=(200, "image/png")):
            evidence = smoke.run_smoke(TOKEN, "42", payloads)
        return evidence, calls

    def test_a_live_run_posts_reads_asserts_and_deletes_both(self):
        evidence, calls = self._drive()
        self.assertEqual(evidence["help_message_id"], "msg-1")
        self.assertEqual(evidence["help_detail_message_id"], "msg-2")
        self.assertTrue(all(item["verdict"] == "PASS" for item in evidence["checks"]))
        deletes = sorted(path for method, path in calls if method == "DELETE")
        self.assertEqual(deletes, ["/channels/42/messages/msg-1",
                                   "/channels/42/messages/msg-2"])

    def test_a_failed_delete_is_recorded_without_hiding_the_brand_verdict(self):
        evidence, _calls = self._drive(fail_delete_second=True)
        by_name = {item["check"]: item["verdict"] for item in evidence["checks"]}
        self.assertEqual(by_name["cleanup_msg-2"], "FAIL")
        self.assertEqual(by_name["help_color"], "PASS")
        self.assertEqual(by_name["help_detail_color"], "PASS")


if __name__ == "__main__":
    unittest.main()
