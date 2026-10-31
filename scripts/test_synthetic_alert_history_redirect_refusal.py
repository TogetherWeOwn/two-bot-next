"""Protected acceptance tests: the alert-history reader must refuse redirects.

Offline only. The transport is faked at ``http.client`` so the real urllib
opener/handler chain runs above it: on the unfixed reader a 3xx response is
followed to a second destination, resending the GitHub Bearer token there,
and a redirected 2xx is then parsed as workflow history. A correct reader
refuses every redirect before any second destination is contacted and
surfaces it as a fixed-vocabulary HistoryError, preserving the documented
fail-open policy.

No source strings are asserted, and ``urlopen`` is deliberately not mocked:
the tests exercise whichever opener construction the reader uses, so long as
it stays on the stdlib urllib/http.client stack.
"""

import email.message
import http.client
import importlib
import io
import json
import sys
import unittest
import urllib.request
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
history = importlib.import_module("synthetic_alert_history")
alert = importlib.import_module("synthetic_alert")

TOKEN_SENTINEL = "synthetic-token-not-a-secret"
EVIL = "https://evil.example/collect"
BODY_SENTINEL = "response-body-must-never-leak"
REDIRECT_STATUSES = (301, 302, 303, 307, 308)
PATH = "/repos/owner/repo/actions/workflows/synthetic-user-flow.yml/runs?per_page=15"


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
        self.requests = []  # (scheme, host, method, selector, headers, timeout)

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
                    (scheme, self.host, method, selector, dict(headers or {}),
                     self.timeout))
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


class HistoryRedirectRefusalTests(unittest.TestCase):
    def setUp(self):
        # Deterministic opener construction: no proxies from the environment,
        # and no urllib global opener cached by an earlier test in the process.
        self._saved_opener = urllib.request._opener
        urllib.request._opener = None
        self.addCleanup(setattr, urllib.request, "_opener", self._saved_opener)
        proxies = mock.patch.object(urllib.request, "getproxies", return_value={})
        proxies.start()
        self.addCleanup(proxies.stop)

    def run_get(self, transport, path=PATH, token=TOKEN_SENTINEL):
        with mock.patch.object(http.client, "HTTPSConnection",
                               transport.connection_class("https")), \
                mock.patch.object(http.client, "HTTPConnection",
                                  transport.connection_class("http")):
            return history._get(path, token)

    def redirect_transport(self, status, location):
        # A followed redirect would land on a scripted 200 with valid JSON:
        # exactly the false "known history" outcome these tests exist to
        # forbid.
        return FakeTransport([
            (status, headers_for(Location=location), BODY_SENTINEL.encode()),
            (200, headers_for(Content_Type="application/json"),
             json.dumps({"workflow_runs": []}).encode()),
        ])

    def assert_no_leak(self, text, location=EVIL):
        for sentinel in (TOKEN_SENTINEL, location, BODY_SENTINEL):
            self.assertNotIn(sentinel, text)

    def test_each_redirect_status_is_refused_before_a_second_destination(self):
        for status in REDIRECT_STATUSES:
            with self.subTest(status=status):
                transport = self.redirect_transport(status, EVIL)
                with self.assertRaises(history.HistoryError) as caught:
                    self.run_get(transport)
                message = str(caught.exception)
                self.assertIn(str(status), message)
                self.assert_no_leak(message)
                self.assertEqual(len(transport.requests), 1, transport.requests)
                scheme, host, method, selector, headers, _timeout = transport.requests[0]
                self.assertEqual(scheme, "https")
                self.assertEqual(host, "api.github.com")
                self.assertEqual(method, "GET")
                self.assertIn(PATH, f"{selector}")
                # The bearer token reaches only the first destination, which
                # is the GitHub API host itself.
                authorization = next(
                    (value for name, value in headers.items()
                     if name.lower() == "authorization"), "")
                self.assertIn(TOKEN_SENTINEL, authorization)

    def test_redirect_to_same_host_is_still_refused(self):
        # The policy is "never follow redirects", not "stay on the same
        # host": even an api.github.com Location must not be contacted.
        same_host = "https://api.github.com/repos/owner/repo/other"
        transport = self.redirect_transport(302, same_host)
        with self.assertRaises(history.HistoryError) as caught:
            self.run_get(transport)
        self.assert_no_leak(str(caught.exception), location=same_host)
        self.assertEqual(len(transport.requests), 1, transport.requests)

    def test_redirect_surfaces_as_unknown_history_and_stays_fail_open(self):
        # A refused redirect is unknown history: the decide seam must
        # fail open verbatim -- alert on a real failure, stay silently
        # green on a healthy sample -- without leaking the token or URL.
        transport = self.redirect_transport(302, EVIL)
        with mock.patch.object(http.client, "HTTPSConnection",
                               transport.connection_class("https")), \
                mock.patch.object(http.client, "HTTPConnection",
                                  transport.connection_class("http")):
            env = {"GITHUB_TOKEN": TOKEN_SENTINEL,
                   "GITHUB_REPOSITORY": "owner/repo",
                   "GITHUB_RUN_ID": "99999", "PROBE_OUTCOME": "failure",
                   "FORCE_FAIL": "false"}
            with self.assertRaises(history.HistoryError) as caught:
                history.previous_probe_outcomes(env, "staging")
            self.assertIn("302", str(caught.exception))
            self.assert_no_leak(str(caught.exception))
            # Fail-open through the decide seam on the same refused read.
            with mock.patch.object(history, "_get",
                                   side_effect=history.HistoryError(
                                       "could not read recent synthetic workflow "
                                       "history (redirect refused with status 302)")):
                self.assertEqual(
                    alert.decide_event({**env, "PROBE_OUTCOME": "failure"},
                                       "staging"), "alert")
                self.assertIsNone(
                    alert.decide_event({**env, "PROBE_OUTCOME": "success"},
                                       "staging"))
        self.assertEqual(len(transport.requests), 1, transport.requests)

    def test_valid_200_still_reads_through_real_opener(self):
        payload = json.dumps({"workflow_runs": []}).encode()
        transport = FakeTransport([(200, headers_for(Content_Type="application/json"),
                                    payload)])
        body = self.run_get(transport)
        self.assertEqual(body, {"workflow_runs": []})
        self.assertEqual(len(transport.requests), 1, transport.requests)
        scheme, host, method, _selector, _headers, timeout = transport.requests[0]
        self.assertEqual(f"{scheme}://{host}", "https://api.github.com")
        self.assertEqual(method, "GET")
        self.assertIsInstance(timeout, (int, float), timeout)
        self.assertGreater(timeout, 0)

    def test_ordinary_http_error_reports_fixed_vocabulary_and_stops(self):
        transport = FakeTransport([(400, headers_for(), BODY_SENTINEL.encode())])
        with self.assertRaises(history.HistoryError) as caught:
            self.run_get(transport)
        message = str(caught.exception)
        self.assertIn("HTTPError", message)
        self.assert_no_leak(message)
        self.assertEqual(len(transport.requests), 1, transport.requests)


if __name__ == "__main__":
    unittest.main()
