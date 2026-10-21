"""Protected acceptance tests: voice-synthetic gateway intake caps and FAIL verdicts.

Offline only. A fake socket feeds crafted gateway frame bytes to the real
``WebSocket.recv`` reader, and a scripted fake gateway drives the real
``Session`` hello parse, ``pump`` dispatch handling and ``main`` verdict
rendering. No network, no tokens (the token below is a fake).

Contract pinned for the Builder (D1 of the roadmap):

- Gateway frame payloads and reassembled messages are each capped at
  ``synth.MAX_BODY_BYTES`` (64 KiB), extending the REST ``BodyTooLarge``
  precedent to the gateway path. An exact-cap frame/message passes; cap+1
  refuses. The length declared on the wire is validated before the payload
  is read, so a huge declared length refuses without an unbounded read.
- Over-cap input, malformed hellos and malformed dispatches raise with a
  fixed no-echo message (never a traceback-flavored ``KeyError``/``TypeError``
  detail, never echoing wire bytes):

  - ``FRAME_MSG``: a single frame payload over the cap.
  - ``MESSAGE_MSG``: continuation accumulation over the cap.
  - ``HELLO_MSG``: missing/non-object ``d``, missing/non-numeric
    ``heartbeat_interval``, or close-before-hello.
  - ``DISPATCH_MSG``: dispatch missing ``t``/``d``, or READY without
    ``user.id``.

- Only ``str(exc)`` is pinned: the exception type name is the Builder's
  choice. Run-level FAIL details must contain the fixed message.
- Valid hellos, valid dispatches and exact-cap inputs behave byte-identically
  (controls); close frames still return None.

Protected file: the Builder builds on this branch head and never edits this
file. Test corrections go back to the author.

Builder notes (not test assertions):

- ``scripts/test_staging_voice_synthetic.py::WebSocketFrameTest`` feeds a
  ~70 KiB fragmented message, which is over the new message cap. Shrink that
  fixture under ``MAX_BODY_BYTES`` in the sibling file (fragmentation and
  ping coverage stay); this file is not the place.
- Wire this file into CI with one voice-gated line mirroring the existing
  ``test_staging_voice_synthetic.py`` invocation in ``check.yml``.
"""

import contextlib
import io
import json
import os
import struct
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_voice_synthetic as synth

FRAME_MSG = "gateway frame answered over the byte cap"
MESSAGE_MSG = "gateway message answered over the byte cap"
HELLO_MSG = "gateway hello was malformed"
DISPATCH_MSG = "gateway dispatch was malformed"

SENTINEL = "SENTINEL_WIRE_BYTES_must_not_echo"
# Fake token, never sent (the gateway is stubbed). Lowercase fixed messages
# stay intact under main()'s token scrub because this has no lowercase overlap.
FAKE_TOKEN = "TOK"


def frame(opcode, payload, fin=True):
    """One server-to-client (unmasked) WebSocket frame."""
    head = bytes([(0x80 if fin else 0) | opcode])
    n = len(payload)
    if n < 126:
        head += bytes([n])
    elif n < 65536:
        head += bytes([126]) + struct.pack(">H", n)
    else:
        head += bytes([127]) + struct.pack(">Q", n)
    return head + payload


def padded_json(obj, size):
    """The object as JSON bytes padded with trailing whitespace to ``size``."""
    raw = json.dumps(obj).encode()
    assert len(raw) <= size, "fixture must fit the target size"
    return raw + b" " * (size - len(raw))


class FakeSocket:
    """Deterministic socket: scripted bytes, EOF when exhausted, reads logged."""

    def __init__(self, data):
        self._buf = bytearray(data)
        self.requested = []
        self.timeouts = []
        self.sent = []
        self.closed = False

    def recv(self, n):
        self.requested.append(n)
        chunk = bytes(self._buf[:n])
        del self._buf[:n]
        return chunk

    def settimeout(self, timeout):
        self.timeouts.append(timeout)

    def sendall(self, data):
        self.sent.append(bytes(data))

    def close(self):
        self.closed = True


class FakeGateway:
    """Scripted ``WebSocket`` stand-in returning parsed messages."""

    def __init__(self, script):
        self._script = list(script)
        self.sent = []
        self.sock = FakeSocket(b"")
        self.closed = False

    def recv(self):
        assert self._script, "test bug: gateway script exhausted"
        return self._script.pop(0)

    def send(self, payload):
        self.sent.append(payload)

    def close(self):
        self.closed = True


def reader(data):
    """The real ``WebSocket.recv`` reader over fake socket bytes."""
    ws = synth.WebSocket.__new__(synth.WebSocket)
    sock = FakeSocket(data)
    ws.sock, ws.lock = sock, threading.Lock()
    return ws, sock


def pump_session(script):
    """The real ``Session.pump`` over a scripted fake gateway."""
    session = synth.Session.__new__(synth.Session)
    session.ws = FakeGateway(script)
    session.seq, session.events, session.me = None, [], None
    return session


class GatewayCapTest(unittest.TestCase):
    CAP = synth.MAX_BODY_BYTES

    def test_single_frame_over_cap_refuses_with_fixed_message(self):
        payload = padded_json({"op": 10, "d": {"heartbeat_interval": 41250},
                               "note": SENTINEL}, self.CAP + 1)
        self.assertEqual(len(payload), self.CAP + 1)
        self.assertEqual(json.loads(payload)["note"], SENTINEL)
        ws, _ = reader(frame(0x1, payload))
        try:
            ws.recv()
        except Exception as exc:
            self.assertEqual(str(exc), FRAME_MSG)
            self.assertNotIn(SENTINEL, str(exc))
        else:
            self.fail("an over-cap gateway frame was read instead of refused")

    def test_single_frame_exact_cap_passes(self):
        payload = padded_json({"op": 10, "d": {"heartbeat_interval": 41250}},
                              self.CAP)
        ws, _ = reader(frame(0x1, payload))
        self.assertEqual(ws.recv(), json.loads(payload))

    def test_continuation_accumulation_over_cap_refuses_with_fixed_message(self):
        payload = padded_json({"op": 0, "t": "READY",
                               "d": {"user": {"id": "900"}}, "note": SENTINEL},
                              self.CAP + 1)
        first, rest = payload[:-100], payload[-100:]
        self.assertLess(len(first), self.CAP + 1)
        ws, _ = reader(frame(0x1, first, fin=False) + frame(0x0, rest))
        try:
            ws.recv()
        except Exception as exc:
            self.assertEqual(str(exc), MESSAGE_MSG)
            self.assertNotIn(SENTINEL, str(exc))
        else:
            self.fail("an over-cap reassembled message was read instead of refused")

    def test_continuation_accumulation_exact_cap_passes(self):
        payload = padded_json({"op": 0, "t": "READY",
                               "d": {"user": {"id": "900"}}}, self.CAP)
        first, rest = payload[:-100], payload[-100:]
        ws, _ = reader(frame(0x1, first, fin=False) + frame(0x0, rest))
        self.assertEqual(ws.recv(), json.loads(payload))

    def test_huge_declared_length_refuses_before_an_unbounded_read(self):
        header = b"\x81\x7f" + struct.pack(">Q", 2**40)
        ws, sock = reader(header + b"0123456789abcdef")
        try:
            ws.recv()
        except Exception as exc:
            self.assertLessEqual(sum(sock.requested), 1024,
                                 "the declared length must be validated before the payload is read")
            self.assertEqual(str(exc), FRAME_MSG)
            self.assertNotIn(SENTINEL, str(exc))
        else:
            self.fail("a frame declaring 2**40 bytes was read instead of refused")

    def test_close_frame_still_returns_none(self):
        ws, _ = reader(frame(0x8, b""))
        self.assertIsNone(ws.recv())


class GatewayHelloTest(unittest.TestCase):
    def hello_fails(self, hello):
        """Drive the real Session hello parse; return the raised message."""
        gateway = FakeGateway([hello])
        with mock.patch.object(synth, "WebSocket", return_value=gateway):
            try:
                synth.Session(FAKE_TOKEN)
            except Exception as exc:
                self.assertEqual(str(exc), HELLO_MSG)
                self.assertNotIn(SENTINEL, str(exc))
            else:
                self.fail(f"malformed hello was accepted: {hello!r}")

    def test_hello_missing_d_refuses(self):
        self.hello_fails({"op": 10, "note": SENTINEL})

    def test_hello_non_object_d_refuses(self):
        for bad in (None, "not-an-object", ["heartbeat_interval"]):
            with self.subTest(d=bad):
                self.hello_fails({"op": 10, "d": bad, "note": SENTINEL})

    def test_hello_missing_or_non_numeric_interval_refuses(self):
        for bad in ({}, {"heartbeat_interval": None},
                    {"heartbeat_interval": "41250"},
                    {"heartbeat_interval": [41250]}):
            with self.subTest(d=bad):
                self.hello_fails({"op": 10, "d": bad, "note": SENTINEL})

    def test_close_before_hello_refuses(self):
        self.hello_fails(None)

    def test_valid_hello_is_byte_identical(self):
        gateway = FakeGateway([{"op": 10, "d": {"heartbeat_interval": 41250}}])
        with mock.patch.object(synth, "WebSocket", return_value=gateway) as ws_cls:
            session = synth.Session(FAKE_TOKEN)
        try:
            ws_cls.assert_called_once_with(synth.GATEWAY, "/?v=10&encoding=json")
            self.assertEqual(session.interval, 41.25)
            self.assertEqual(gateway.sent, [{"op": 2, "d": {
                "token": FAKE_TOKEN, "intents": synth.INTENTS,
                "properties": {"os": "linux", "browser": "voice-synthetic",
                               "device": "voice-synthetic"}}}])
        finally:
            session.close()
        self.assertFalse(session.alive)


class GatewayDispatchTest(unittest.TestCase):
    def dispatch_fails(self, message):
        session = pump_session([message])
        try:
            session.pump(lambda event: False, time.monotonic() + 5)
        except Exception as exc:
            self.assertEqual(str(exc), DISPATCH_MSG)
            self.assertNotIn(SENTINEL, str(exc))
        else:
            self.fail(f"malformed dispatch was accepted: {message!r}")

    def test_dispatch_missing_t_or_d_refuses(self):
        for bad in ({"op": 0, "s": 5, "note": SENTINEL},
                    {"op": 0, "s": 5, "t": "READY", "note": SENTINEL},
                    {"op": 0, "s": 5, "d": {}, "note": SENTINEL}):
            with self.subTest(message=bad):
                self.dispatch_fails(bad)

    def test_ready_without_user_id_refuses(self):
        for bad in ({"op": 0, "s": 1, "t": "READY", "d": {"note": SENTINEL}},
                    {"op": 0, "s": 1, "t": "READY",
                     "d": {"user": {}, "note": SENTINEL}}):
            with self.subTest(message=bad):
                self.dispatch_fails(bad)

    def test_valid_dispatches_are_byte_identical(self):
        ready = {"op": 0, "s": 1, "t": "READY",
                 "d": {"user": {"id": "900"}, "session_id": "sess"}}
        voice = {"op": 0, "s": 2, "t": "VOICE_STATE_UPDATE",
                 "d": {"user_id": "900", "channel_id": "5001"}}
        session = pump_session([{"op": 11, "d": None}, ready, voice])
        event = session.pump(lambda e: e["t"] == "VOICE_STATE_UPDATE",
                             time.monotonic() + 5)
        self.assertEqual(event, {"t": "VOICE_STATE_UPDATE", "d": voice["d"]})
        self.assertEqual(session.events, [{"t": "READY", "d": ready["d"]},
                                          {"t": "VOICE_STATE_UPDATE", "d": voice["d"]}])
        self.assertEqual(session.me, "900")
        self.assertEqual(session.seq, 2)


class GatewayRunVerdictTest(unittest.TestCase):
    def run_with_script(self, script):
        gateway = FakeGateway(script)
        with tempfile.TemporaryDirectory() as tmp:
            out = str(Path(tmp) / "out.json")
            with mock.patch.object(synth, "WebSocket", return_value=gateway), \
                    mock.patch.object(synth, "rest",
                                      side_effect=AssertionError("must not reach REST")), \
                    mock.patch.object(synth, "rest_patch",
                                      side_effect=AssertionError("must not reach REST")), \
                    mock.patch.dict(os.environ,
                                    {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": FAKE_TOKEN},
                                    clear=False):
                os.environ.pop("TWO_STAGING_METRICS_TOKEN", None)
                os.environ.pop("STAGING_WORKER_URL", None)
                stdout, stderr = io.StringIO(), io.StringIO()
                with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                    code = synth.main(["--evidence", out])
            evidence = json.loads(Path(out).read_text())
        return code, stdout.getvalue(), stderr.getvalue(), evidence

    def assert_run_fail(self, result, message):
        code, stdout, stderr, evidence = result
        self.assertEqual(code, 1)
        self.assertEqual(len(evidence["checks"]), 1)
        failed = evidence["checks"][0]
        self.assertEqual((failed["check"], failed["verdict"]), ("run", "FAIL"))
        self.assertIn(message, failed["detail"])
        self.assertIn(message, stdout)
        self.assertNotIn(SENTINEL, failed["detail"])
        self.assertNotIn(SENTINEL, stdout)
        self.assertNotIn("Traceback", stderr)

    def test_malformed_hello_reports_fail_with_fixed_message(self):
        result = self.run_with_script([{"op": 10, "note": SENTINEL}])
        self.assert_run_fail(result, HELLO_MSG)

    def test_malformed_dispatch_reports_fail_with_fixed_message(self):
        result = self.run_with_script([
            {"op": 10, "d": {"heartbeat_interval": 41250}},
            {"op": 0, "s": 1, "note": SENTINEL},
        ])
        self.assert_run_fail(result, DISPATCH_MSG)


if __name__ == "__main__":
    unittest.main()
