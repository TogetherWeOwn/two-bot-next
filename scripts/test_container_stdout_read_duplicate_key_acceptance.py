"""Duplicate-key refusal acceptance for scripts/container_stdout_read.py (offline only).

The transport parses every API answer with a hookless ``json.loads``
(``HttpTransport.__call__``), so a repeated object key currently keeps the
last value and the reader grades it: a repeated ``success`` can flip a
failed query into a printed packet, and a repeated event field can smuggle
a second message past redaction into evidence.

Contract pinned here: any duplicate object key in an answer — conflicting
in both orders, identical values, nested, or hidden behind an escaped
spelling — refuses with the existing fixed vocabulary ``bad_response``,
with exit 1, an empty stdout, the exact one-line stderr diagnostic, and no
echo of the hostile keys, values or bodies. Duplicate-free answers parse
byte-identically to ``json.loads``.

Stdlib only: the real transport runs against a stubbed opener, so no socket
is ever opened and no credential is needed.

This file is the contract: satisfy it by changing the reader, not by
editing these tests.
"""

from datetime import datetime, timezone
import io
import json
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import container_stdout_read as reader

SERVICE = "11111111-2222-4333-8444-555555555555"
ACCOUNT = "0123456789abcdef0123456789abcdef"
END = "2026-10-11T12:20:00Z"
NOW = datetime(2026, 10, 11, 12, 30, 0, tzinfo=timezone.utc)
QUERY_PATH = f"/accounts/{ACCOUNT}/workers/observability/telemetry/query"
TOKEN = "tok-" + "A1b2C3d4" * 4  # fake; assembled so no scanner mistakes it for a credential
TS_MS = int(datetime(2026, 10, 11, 12, 18, tzinfo=timezone.utc).timestamp() * 1000)

# Hostile markers are assembled at run time so the file stays scanner-clean,
# and stay short enough to survive redaction had they ever reached an event.
HOSTILE_KEY = "hos" + "tile" + "-dup-key"
HOSTILE_VALUE = "hos" + "tile" + "-dup-value-9"

EMPTY_RESULT = '{"events": {"events": []}}'
# Duplicate-key answers. Pre-fix each parses last-wins; post-fix the
# transport itself refuses every one with bad_response.
CONFLICT_TRUE_WINS = (
    '{"success": false, "result": %s, "success": true}' % EMPTY_RESULT
).encode()
CONFLICT_FALSE_WINS = (
    '{"success": true, "result": %s, "success": false}' % EMPTY_RESULT
).encode()
IDENTICAL_REPEAT = (
    '{"success": true, "result": %s, "success": true}' % EMPTY_RESULT
).encode()
NESTED_EVENTS = (
    '{"success": true, "result": {"events": {"events": [], "events": []}}}'
).encode()
ESCAPED_SUCCESS = (
    '{"success": false, "result": %s, "\\u0073uccess": true}' % EMPTY_RESULT
).encode()
EVENT_NESTED = (
    '{"success": true, "result": {"events": {"events": '
    '[{"timestamp": %d, "source": {"message": "line one", "message": "%s"}, '
    '"$metadata": {"service": "%s", "level": "warn", "message": "line one"}}]}}}'
    % (TS_MS, HOSTILE_VALUE, SERVICE)
).encode()
DUPLICATE_BODIES = {
    "conflict_true_wins": CONFLICT_TRUE_WINS,
    "conflict_false_wins": CONFLICT_FALSE_WINS,
    "identical_repeat": IDENTICAL_REPEAT,
    "nested_events": NESTED_EVENTS,
    "escaped_success": ESCAPED_SUCCESS,
    "event_nested": EVENT_NESTED,
}
UNIQUE_BODY = ('{"success": true, "result": %s}' % EMPTY_RESULT).encode()
DUPLICATE_ACCOUNTS = (
    '{"success": true, "result": [{"id": "%s"}], "success": true}' % ACCOUNT
).encode()
UNIQUE_ACCOUNTS = ('{"success": true, "result": [{"id": "%s"}]}' % ACCOUNT).encode()


class FakeResponse:
    def __init__(self, payload, status=200):
        self.status = status
        self._payload = payload

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False

    def read(self, limit=-1):
        return self._payload


class FakeOpener:
    """Stubbed urlopen: answers every request with the canned payload."""

    def __init__(self, payload, status=200):
        self.payload = payload
        self.status = status
        self.calls = []

    def open(self, request, timeout=None):
        self.calls.append(request)
        return FakeResponse(self.payload, self.status)


def offline_transport(payload, status=200):
    """The real transport with a stubbed opener: the allowlist, the cap and
    the parse under test all run; no socket is ever opened."""
    opener = FakeOpener(payload, status)
    with mock.patch.object(reader.urllib.request, "build_opener", return_value=opener):
        return reader.HttpTransport(TOKEN)


def run_main(argv, transport, env=None):
    out, err = io.StringIO(), io.StringIO()
    code = reader.main(argv, env={} if env is None else env, transport=transport,
                       now=lambda: NOW, stdout=out, stderr=err)
    return code, out.getvalue(), err.getvalue()


QUERY_ARGV = ["--service", SERVICE, "--end", END, "--account-id", ACCOUNT]


class TransportParseTests(unittest.TestCase):
    def assert_refuses(self, payload, method="POST", path=QUERY_PATH):
        transport = offline_transport(payload)
        with self.assertRaises(reader.ReadFailed) as caught:
            transport(method, path, {"ping": 1} if method == "POST" else None)
        self.assertEqual(str(caught.exception), "bad_response")

    def test_conflicting_success_orders_refuse(self):
        for name in ("conflict_true_wins", "conflict_false_wins"):
            with self.subTest(name):
                self.assert_refuses(DUPLICATE_BODIES[name])

    def test_identical_values_refuse(self):
        self.assert_refuses(DUPLICATE_BODIES["identical_repeat"])

    def test_nested_duplicate_refuses(self):
        self.assert_refuses(DUPLICATE_BODIES["nested_events"])

    def test_escaped_spelling_refuses(self):
        # The decoder resolves escapes before the hook runs, so this is a
        # repeat of "success", not a new key. A text scan would miss it.
        self.assert_refuses(DUPLICATE_BODIES["escaped_success"])

    def test_event_source_duplicate_refuses(self):
        self.assert_refuses(DUPLICATE_BODIES["event_nested"])

    def test_accounts_list_duplicate_refuses(self):
        # The same shared parse feeds account resolution: it refuses there too.
        self.assert_refuses(DUPLICATE_ACCOUNTS, method="GET", path="/accounts")

    def test_unique_body_parses_byte_identically(self):
        transport = offline_transport(UNIQUE_BODY)
        self.assertEqual(transport("POST", QUERY_PATH, {"ping": 1}),
                         json.loads(UNIQUE_BODY))


class MainTests(unittest.TestCase):
    def test_duplicate_answer_fails_closed_with_a_fixed_line(self):
        for name, payload in DUPLICATE_BODIES.items():
            with self.subTest(name):
                code, out, err = run_main(list(QUERY_ARGV), offline_transport(payload),
                                          env={reader.ACCOUNT_ENV: ACCOUNT})
                self.assertEqual(code, 1, out)
                self.assertEqual(out, "")
                self.assertEqual(err.strip(),
                                 "container_stdout_read: read failed (bad_response)")

    def test_duplicate_accounts_answer_fails_closed(self):
        routes = {"query": UNIQUE_BODY, "accounts": DUPLICATE_ACCOUNTS}

        class Router(FakeOpener):
            def open(self, request, timeout=None):
                self.calls.append(request)
                if request.full_url.endswith("/accounts"):
                    return FakeResponse(routes["accounts"])
                return FakeResponse(routes["query"])

        with mock.patch.object(reader.urllib.request, "build_opener",
                               return_value=Router(b"")):
            transport = reader.HttpTransport(TOKEN)
        code, out, err = run_main(["--service", SERVICE, "--end", END], transport, env={})
        self.assertEqual(code, 1, out)
        self.assertEqual(out, "")
        self.assertEqual(err.strip(), "container_stdout_read: read failed (bad_response)")

    def test_hostile_values_never_echo(self):
        for name in ("event_nested", "conflict_true_wins"):
            with self.subTest(name):
                code, out, err = run_main(list(QUERY_ARGV),
                                          offline_transport(DUPLICATE_BODIES[name]),
                                          env={reader.ACCOUNT_ENV: ACCOUNT})
                self.assertEqual(code, 1, out)
                self.assertNotIn(HOSTILE_VALUE, out)
                self.assertNotIn(HOSTILE_VALUE, err)
                self.assertNotIn(HOSTILE_KEY, out)
                self.assertNotIn(HOSTILE_KEY, err)

    def test_unique_answer_reads_unchanged(self):
        # Control: a duplicate-free answer prints the packet, pre- and post-fix.
        code, out, err = run_main(list(QUERY_ARGV), offline_transport(UNIQUE_BODY),
                                  env={reader.ACCOUNT_ENV: ACCOUNT})
        self.assertEqual((code, err), (0, ""))
        packet = json.loads(out)
        self.assertEqual(packet["event_count"], 0)
        self.assertEqual(packet["events"], [])


if __name__ == "__main__":
    unittest.main()
