"""Offline fixtures for shared JSON, stream and HTTP intake."""

from email.message import Message
from io import BytesIO
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.request import HTTPHandler, HTTPSHandler, Request
from urllib.response import addinfourl

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from script_intake import decode_json, decode_legacy_json, no_redirect_opener, read_with_overflow
sys.path.pop(0)


class ScriptIntakeTests(unittest.TestCase):
    def test_strict_json_accepts_valid_encoded_input_and_quoted_brackets(self):
        for encoding in ("utf-8", "utf-8-sig", "utf-16", "utf-32-le"):
            with self.subTest(encoding=encoding):
                raw = '{"k": ["]", 12, 0.5]}'.encode(encoding)
                self.assertEqual(decode_json(raw, max_depth=2), {"k": ["]", 12, 0.5]})

    def test_strict_json_rejects_duplicates_bad_numbers_and_malformed_input(self):
        for raw in (r'{"d": [{"k": 1, "\u006b": 2}]}', '[NaN]', '[Infinity]',
                    '[-Infinity]', '[1e999]', '{"k":'):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                decode_json(raw)
        with self.assertRaises(RecursionError):
            decode_json('{"k": [[0]]}'.encode("utf-16-be"), max_depth=2)
        self.assertEqual(decode_legacy_json('{"k": 0, "k": 1}'), {"k": 1})

    def test_overflow_read_includes_one_extra_byte(self):
        with BytesIO(b"abcde") as stream:
            self.assertEqual(read_with_overflow(stream, 2), b"abc")
            self.assertEqual(stream.tell(), 3)
            self.assertEqual(read_with_overflow(stream, 2), b"de")
            with self.assertRaises(ValueError):
                read_with_overflow(stream, -1)

    def test_opener_returns_safe_response_and_preserves_https_handler(self):
        url = "http://origin.example/"
        response = addinfourl(BytesIO(b'{"ok":true}'), Message(), url, 200)
        response.msg = "OK"
        https_handler = HTTPSHandler()
        with patch.object(HTTPHandler, "http_open", return_value=response) as send:
            opener = no_redirect_opener(https_handler=https_handler, use_proxy=False)
            with opener.open(url, timeout=2) as received:
                self.assertEqual(decode_json(read_with_overflow(received, 11)), {"ok": True})
            self.assertIn(https_handler, opener.handlers)
            self.assertEqual(send.call_args.args[0].timeout, 2)

    def test_opener_refuses_redirect_without_replaying_request(self):
        url = "http://origin.example/"
        for code in (301, 302, 303, 307, 308):
            with self.subTest(code=code):
                response = self.redirect(url, code, "https://other.example/")
                req = Request(url, headers={"Authorization": "Bearer synthetic-fixture"})
                with patch.object(HTTPHandler, "http_open", return_value=response) as send:
                    with self.assertRaises(HTTPError) as caught:
                        no_redirect_opener(use_proxy=False).open(req, timeout=2)
                    with caught.exception as error:
                        self.assertEqual(error.code, code)
                        self.assertEqual(error.fp.tell(), 0)
                    send.assert_called_once()

    def test_custom_redirect_refusal_closes_body_even_without_location(self):
        url = "http://origin.example/"
        response = self.redirect(url, 302, None)
        with patch.object(HTTPHandler, "http_open", return_value=response) as send:
            with self.assertRaisesRegex(ValueError, "^redirect refused$"):
                no_redirect_opener(
                    use_proxy=False, redirect_error=lambda: ValueError("redirect refused")
                ).open(url, timeout=2)
            self.assertTrue(response.fp.closed)
            send.assert_called_once()

    @staticmethod
    def redirect(url, code, target):
        headers = Message()
        if target is not None:
            headers["Location"] = target
        response = addinfourl(BytesIO(b"synthetic body"), headers, url, code)
        response.msg = "redirect"
        return response
