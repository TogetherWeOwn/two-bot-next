"""Protected acceptance tests: the incident issue sink must refuse redirects.

Offline only. The transport is faked at ``http.client`` so the real urllib
opener/handler chain runs above it: an unfixed sender would follow a 3xx
response to a second destination (resending the GitHub token there) and then
report a redirected 2xx as a written incident issue. A correct sender refuses
every redirect before any second destination is contacted.

No source strings are asserted, and ``urlopen`` is deliberately not mocked:
the tests exercise whichever opener construction the sender uses, so long as
it stays on the stdlib urllib/http.client stack.
"""

import email.message
import http.client
import importlib
import io
import json
import os
import sys
import tempfile
import unittest
import urllib.request
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
alert = importlib.import_module("synthetic_alert")

TOKEN_SENTINEL = "synthetic-token-not-a-secret"
ENV = {"GITHUB_TOKEN": TOKEN_SENTINEL, "GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next"}
LIST_URL = ("https://api.github.com/repos/TogetherWeOwn/two-bot-next/issues"
            "?state=open&labels=incident&per_page=100&page=1")
CREATE_URL = "https://api.github.com/repos/TogetherWeOwn/two-bot-next/issues"
EVIL = "https://evil.example/collect"
BODY_SENTINEL = "response-body-must-never-leak"
REDIRECT_STATUSES = (301, 302, 303, 307, 308)

EVIDENCE = {
    "origin": "https://two-bot-next-staging.example.workers.dev",
    "utc": "2026-10-16T00:00:00Z",
    "checks": [{"check": "readyz", "verdict": "fail", "shape": "readyz 503: parked"}],
    "build_revision": "abc123",
    "build_id": "build-1",
    "result": "fail",
}


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

        # Subclassing keeps every class attribute/method urllib reads off the
        # connection class; only the wire-touching methods are overridden,
        # so no socket is ever opened.
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
        # Deterministic opener construction: no proxies from the environment,
        # and no urllib global opener cached by an earlier test in the process.
        self._saved_opener = urllib.request._opener
        urllib.request._opener = None
        self.addCleanup(setattr, urllib.request, "_opener", self._saved_opener)
        proxies = mock.patch.object(urllib.request, "getproxies", return_value={})
        proxies.start()
        self.addCleanup(proxies.stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")
        Path(self.evidence).write_text(json.dumps(EVIDENCE))

    def run_post(self, transport):
        with mock.patch.object(http.client, "HTTPSConnection",
                               transport.connection_class("https")), \
                mock.patch.object(http.client, "HTTPConnection",
                                  transport.connection_class("http")):
            return alert.post(ENV, "staging", "alert", "content")

    def redirect_transport(self, status, location):
        # A followed redirect would land on a scripted 204: exactly the false
        # "delivered" outcome these tests exist to forbid.
        return FakeTransport([
            (status, headers_for(Location=location), BODY_SENTINEL.encode()),
            (204, headers_for(), b""),
        ])

    def assert_no_leak(self, text, location=EVIL):
        for sentinel in (TOKEN_SENTINEL, location, BODY_SENTINEL):
            self.assertNotIn(sentinel, text)

    def test_each_redirect_status_is_refused_before_a_second_destination(self):
        for status in REDIRECT_STATUSES:
            with self.subTest(status=status):
                transport = self.redirect_transport(status, EVIL)
                with self.assertRaises(alert.AlertError) as caught:
                    self.run_post(transport)
                message = str(caught.exception)
                self.assertIn(str(status), message)
                self.assert_no_leak(message)
                self.assertEqual(len(transport.requests), 1, transport.requests)
                scheme, host, method, selector, _body, _timeout = transport.requests[0]
                self.assertEqual(f"{scheme}://{host}{selector}", LIST_URL)
                self.assertEqual(method, "GET")

    def test_redirect_to_another_github_api_url_is_still_refused(self):
        # The policy is "never follow redirects", not "re-validate the target":
        # even an api.github.com Location must not be contacted.
        other = "https://api.github.com/repos/someone/else/issues"
        transport = self.redirect_transport(302, other)
        with self.assertRaises(alert.AlertError) as caught:
            self.run_post(transport)
        self.assert_no_leak(str(caught.exception), location=other)
        self.assertEqual(len(transport.requests), 1, transport.requests)

    def test_redirect_fails_delivery_through_main_without_leaking(self):
        transport = self.redirect_transport(302, EVIL)
        env = {**ENV, "ALERT_EVENT": "alert",
               "GIT_SHA": "deadbeef",
               "GITHUB_SERVER_URL": "https://github.com",
               "GITHUB_RUN_ID": "999"}
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, env, clear=True), \
                redirect_stdout(out), redirect_stderr(err), \
                mock.patch.object(http.client, "HTTPSConnection",
                                  transport.connection_class("https")), \
                mock.patch.object(http.client, "HTTPConnection",
                                  transport.connection_class("http")):
            code = alert.main(["--env", "staging", "--evidence", self.evidence,
                               "--post-only"])
        self.assertEqual(code, 1)
        self.assertIn("302", err.getvalue())
        self.assert_no_leak(out.getvalue() + err.getvalue())
        self.assertEqual(len(transport.requests), 1, transport.requests)

    def test_valid_responses_open_one_labelled_issue_with_a_timeout(self):
        transport = FakeTransport([(200, headers_for(), b"[]"),
                                   (201, headers_for(), b'{"number": 5}'),
                                   (200, headers_for(), b'[{"number": 5, "body": null}]')])
        self.assertEqual(self.run_post(transport), "opened #5")
        self.assertEqual(len(transport.requests), 3, transport.requests)
        first, (scheme, host, method, selector, body, timeout), last = transport.requests
        for _s, _h, read, read_selector, _b, _t in (first, last):
            self.assertEqual((read, f"https://api.github.com{read_selector}"), ("GET", LIST_URL))
        self.assertEqual(f"{scheme}://{host}{selector}", CREATE_URL)
        self.assertEqual(method, "POST")
        payload = json.loads(body.decode())
        self.assertEqual(payload["labels"], ["incident"])
        self.assertIn("content", payload["body"])
        self.assertIsInstance(timeout, (int, float), timeout)
        self.assertGreater(timeout, 0)

    def test_ordinary_http_error_reports_status_only_and_stops(self):
        transport = FakeTransport([(400, headers_for(), BODY_SENTINEL.encode())])
        with self.assertRaises(alert.AlertError) as caught:
            self.run_post(transport)
        message = str(caught.exception)
        self.assertIn("400", message)
        self.assert_no_leak(message)
        self.assertEqual(len(transport.requests), 1, transport.requests)


if __name__ == "__main__":
    unittest.main()
