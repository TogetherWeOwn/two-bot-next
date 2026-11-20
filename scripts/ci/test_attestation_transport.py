"""Synthetic transport regressions; no network requests are made."""
import http.client
import importlib.util
import io
from pathlib import Path
import unittest
from unittest import mock
from urllib.error import HTTPError, URLError


spec = importlib.util.spec_from_file_location(
    "attestation_transport", Path(__file__).resolve().parents[2] / ".github/scripts/attestation_transport.py"
)
transport = importlib.util.module_from_spec(spec)
spec.loader.exec_module(transport)


class MetadataTransport(unittest.TestCase):
    def setUp(self):
        self.reader = transport.RestReader("org/repo", "synthetic-token")
        self.reader._opener = mock.MagicMock()
        self.response = self.reader._opener.open.return_value.__enter__.return_value
        self.response.status = 200
        self.response.length = None
        self.response.read.return_value = b'{"ok": true}'

    def refused(self, route):
        with self.assertRaises(transport.TransportError) as error:
            self.reader(route)
        self.assertEqual(str(error.exception), "Metadata request refused")
        self.assertIsNone(error.exception.__cause__)

    def test_allowed_routes_and_bound_request(self):
        for suffix in ("pulls/1", "pulls/1/files?per_page=100&page=1",
                       "pulls/123/files?per_page=100&page=31", "git/ref/heads/main"):
            with self.subTest(suffix=suffix):
                is_files = "/files?" in suffix
                self.response.read.return_value = b'[]' if is_files else b'{"ok": true}'
                self.assertEqual(self.reader("repos/org/repo/" + suffix), [] if is_files else {"ok": True})
                args, kwargs = self.reader._opener.open.call_args
                self.assertEqual(args[0].full_url, "https://api.github.com/repos/org/repo/" + suffix)
                self.assertEqual(args[0].get_method(), "GET")
                self.assertEqual(kwargs["timeout"], transport.TIMEOUT_SECONDS)
                self.response.read.assert_called_with(transport.MAX_RESPONSE_BYTES + 1)

    def test_bad_routes_never_reach_transport(self):
        for route in (None, "repos/other/repo/pulls/1", "repos/org/repo/pulls/0",
                      "repos/org/repo/pulls/01", "repos/org/repo/pulls/1/../1",
                      "repos/org/repo/pulls/1%2Ffiles", "repos/org/repo/pulls/1#x",
                      "repos/org/repo/pulls/1?x=secret", "repos/org/repo/git/ref/heads/dev",
                      "repos/org/repo/git/ref/heads/%6dain", "repos/org/repo/pulls/1/files?per_page=100&page=32",
                      "repos/org/repo/pulls/1/files?per_page=100&page=1&x=y"):
            with self.subTest(route=route):
                self.refused(route)
        self.reader._opener.open.assert_not_called()

    def test_invalid_constructor_inputs(self):
        for repository, token in (("../repo", "t"), ("org/..", "t"), ("org/repo/other", "t"),
                                  ("org/repo\n", "t"), ("org/repo", ""),
                                  ("org/repo", "token\nheader"), ("org/repo", "x" * 4097)):
            with self.subTest(repository=repository), self.assertRaises(transport.TransportError):
                transport.RestReader(repository, token)

    def test_malformed_responses_and_errors_are_redacted(self):
        route = "repos/org/repo/pulls/1"
        for raw in (b'', b'\xff', b'{"a":1,"a":2}', b'NaN', b'"private text" trailing',
                    b'x' * (transport.MAX_RESPONSE_BYTES + 1)):
            with self.subTest(size=len(raw)):
                self.response.read.return_value = raw
                self.refused(route)
        for exc in (TimeoutError("synthetic-token"), URLError("private url"),
                    HTTPError("private url", 302, "private text", {}, None), OSError("private text")):
            with self.subTest(error=type(exc).__name__):
                self.reader._opener.open.side_effect = exc
                self.refused(route)
        self.reader._opener.open.side_effect = None
        self.response.read.return_value = b'{}'
        self.assertEqual(self.reader(route), {})
        self.response.status = 302
        self.refused(route)

    def test_fixed_length_response_must_be_complete(self):
        for length, succeeds in ((2, True), (100, False)):
            with self.subTest(length=length):
                sock = mock.Mock()
                sock.makefile.return_value = io.BytesIO(
                    b'HTTP/1.1 200 OK\r\nContent-Length: ' + str(length).encode() + b'\r\n\r\n{}'
                )
                response = http.client.HTTPResponse(sock)
                response.begin()
                self.reader._opener.open.return_value = response
                if succeeds:
                    self.assertEqual(self.reader('repos/org/repo/pulls/1'), {})
                else:
                    self.refused('repos/org/repo/pulls/1')

    def test_http_error_closes_stream_when_exception_is_retained(self):
        stream = io.BytesIO(b'private body')
        self.reader._opener.open.side_effect = HTTPError('private url', 403, 'private', {}, stream)
        with self.assertRaises(transport.TransportError) as caught:
            self.reader('repos/org/repo/pulls/1')
        self.assertEqual(str(caught.exception), 'Metadata request refused')
        self.assertTrue(stream.closed)

    def test_redirect_handler_refuses_redirects(self):
        handler = transport._NoRedirect()
        self.assertIsNone(handler.redirect_request(None, None, 302, "", {}, "https://example.invalid"))


if __name__ == "__main__":
    unittest.main()
