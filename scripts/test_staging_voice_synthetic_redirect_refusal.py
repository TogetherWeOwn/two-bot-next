"""Protected acceptance tests: staging voice-synthetic token readers refuse redirects.

Offline only. The transport is faked at ``http.client`` so the real urllib
opener/handler chain runs above it: on the unfixed readers a 3xx response is
followed to a second destination, and a redirected 2xx is then reported as a
successful read. Correct readers refuse every redirect before any second
destination is contacted.

Covers all four token-bearing call sites in ``staging_voice_synthetic.py``:
``metrics_snapshot``, ``rest``, ``rest_patch`` and ``metrics_value``.

No source strings are asserted, and ``urlopen`` is deliberately not mocked:
the tests exercise whichever opener construction the readers use, so long as
it stays on the stdlib urllib/http.client stack.
"""

import email.message
import http.client
import importlib
import io
import json
import unittest
import urllib.request
from pathlib import Path
from unittest import mock
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
synth = importlib.import_module("staging_voice_synthetic")

TOKEN_SENTINEL = "synthetic-token-not-a-secret"
EVIL = "https://evil.example/collect"
BODY_SENTINEL = "response-body-must-never-leak"
REDIRECT_STATUSES = (301, 302, 303, 307, 308)

STAGING_URL = "https://two-bot-next-staging.example.workers.dev"
CHANNEL_PATH = "/channels/5001"


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


def headers_for(**items):
    message = email.message.Message()
    for name, value in items.items():
        message[name.replace("_", "-")] = value
    return message


class FakeTransport:
    """Replaces http.client connections; records every attempted destination."""

    def __init__(self, script):
        # script: list of (status, headers, body) returned in request order.
        self.script = list(script)
        self.requests = []  # (scheme, host, method, selector, body, timeout)

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
                    (scheme, self.host, method, selector, body, self.timeout))
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


class RedirectRefusalTests(unittest.TestCase):
    def setUp(self):
        self._saved_opener = urllib.request._opener
        urllib.request._opener = None
        self.addCleanup(setattr, urllib.request, "_opener", self._saved_opener)
        proxies = mock.patch.object(urllib.request, "getproxies", return_value={})
        proxies.start()
        self.addCleanup(proxies.stop)

    def run_with_transport(self, transport, func, *args, **kwargs):
        with mock.patch.object(http.client, "HTTPSConnection",
                               transport.connection_class("https")), \
                mock.patch.object(http.client, "HTTPConnection",
                                  transport.connection_class("http")):
            return func(*args, **kwargs)

    def redirect_transport(self, status, location, follow_body=b'{"id": "5001"}'):
        # A followed redirect would land on a scripted 200: exactly the false
        # "successful read" outcome these tests exist to forbid.
        return FakeTransport([
            (status, headers_for(Location=location), BODY_SENTINEL.encode()),
            (200, headers_for(), follow_body),
        ])

    def assert_no_leak(self, text, location=EVIL):
        # Refusal messages carry a status only: never the token, the
        # redirect target, or the response body.
        for sentinel in (TOKEN_SENTINEL, location, BODY_SENTINEL):
            self.assertNotIn(sentinel, text)

    def test_each_reader_refuses_each_redirect_before_a_second_destination(self):
        readers = (
            ("metrics_snapshot",
             lambda: synth.metrics_snapshot(STAGING_URL, TOKEN_SENTINEL)),
            ("metrics_value",
             lambda: synth.metrics_value(STAGING_URL, TOKEN_SENTINEL)),
            ("rest",
             lambda: synth.rest(TOKEN_SENTINEL, CHANNEL_PATH)),
            ("rest_patch",
             lambda: synth.rest_patch(TOKEN_SENTINEL, CHANNEL_PATH, {"name": "x"})),
        )
        for name, call in readers:
            for status in REDIRECT_STATUSES:
                with self.subTest(reader=name, status=status):
                    transport = self.redirect_transport(status, EVIL)
                    with self.assertRaises(synth.RedirectRefused) as caught:
                        self.run_with_transport(transport, call)
                    message = str(caught.exception)
                    self.assertIn(str(status), message, name)
                    self.assert_no_leak(message)
                    self.assertEqual(len(transport.requests), 1, transport.requests)

    def test_redirect_to_same_origin_is_still_refused(self):
        # The policy is "never follow redirects", not "re-validate the target":
        # even a same-origin Location must not be contacted.
        same_origin = STAGING_URL + "/ops/metrics?retry=1"
        transport = self.redirect_transport(302, same_origin)
        with self.assertRaises(synth.RedirectRefused) as caught:
            self.run_with_transport(
                transport, lambda: synth.metrics_snapshot(STAGING_URL, TOKEN_SENTINEL))
        self.assert_no_leak(str(caught.exception), location=same_origin)
        self.assertEqual(len(transport.requests), 1, transport.requests)

    def test_redirect_error_is_fixed_vocabulary(self):
        transport = self.redirect_transport(302, EVIL)
        with self.assertRaises(synth.RedirectRefused) as caught:
            self.run_with_transport(
                transport, lambda: synth.rest(TOKEN_SENTINEL, CHANNEL_PATH))
        exc = caught.exception
        # Fixed vocabulary: the type name and message shape are stable, and the
        # message carries only the status, never the token, URL or body.
        self.assertIn("302", str(exc))
        self.assert_no_leak(str(exc))
        self.assertEqual(len(transport.requests), 1, transport.requests)

    def test_valid_reads_still_succeed_once(self):
        body = json.dumps({"id": "5001"}).encode()
        transport = FakeTransport([(200, headers_for(), body)])
        result = self.run_with_transport(
            transport, lambda: synth.rest(TOKEN_SENTINEL, CHANNEL_PATH))
        self.assertEqual(result["id"], "5001")
        self.assertEqual(len(transport.requests), 1, transport.requests)

    def test_ordinary_http_error_still_uses_fail_open_paths(self):
        # Metrics readers SKIP (None) on ordinary HTTP errors; rest_patch
        # reports the status; rest re-raises. None of these may contact a
        # second destination.
        metrics_transport = FakeTransport([(500, headers_for(), BODY_SENTINEL.encode())])
        self.assertEqual(
            self.run_with_transport(
                metrics_transport,
                lambda: synth.metrics_snapshot(STAGING_URL, TOKEN_SENTINEL)),
            (None, None))
        self.assertEqual(len(metrics_transport.requests), 1)

        value_transport = FakeTransport([(500, headers_for(), b"oops")])
        self.assertIsNone(
            self.run_with_transport(
                value_transport,
                lambda: synth.metrics_value(STAGING_URL, TOKEN_SENTINEL)))
        self.assertEqual(len(value_transport.requests), 1)

        patch_transport = FakeTransport([(403, headers_for(), b"{}")])
        self.assertEqual(
            self.run_with_transport(
                patch_transport,
                lambda: synth.rest_patch(TOKEN_SENTINEL, CHANNEL_PATH, {"name": "x"})),
            (403, None))
        self.assertEqual(len(patch_transport.requests), 1)


if __name__ == "__main__":
    unittest.main()
