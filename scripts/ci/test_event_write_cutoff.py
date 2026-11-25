"""Pin the provider rules used by the isolated event write rehearsal."""

import importlib.util
import io
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

SPEC = importlib.util.spec_from_file_location(
    "recorder", Path(__file__).resolve().parents[2] / "tests" / "event_write_cutoff" / "recorder.py"
)
recorder = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(recorder)


class EventWriteCutoffTest(unittest.TestCase):
    def test_provider_requires_both_event_permissions_removed(self):
        self.assertTrue(recorder.permits_event("POST", recorder.CREATE_EVENTS, False))
        self.assertFalse(recorder.permits_event("POST", recorder.MANAGE_EVENTS, False))
        for method in ("PATCH", "DELETE"):
            self.assertTrue(recorder.permits_event(method, recorder.CREATE_EVENTS, True))
            self.assertFalse(recorder.permits_event(method, recorder.CREATE_EVENTS, False))
            self.assertTrue(recorder.permits_event(method, recorder.MANAGE_EVENTS, False))
        for method in ("POST", "PATCH", "DELETE"):
            self.assertTrue(recorder.permits_event(method, recorder.ADMINISTRATOR, True))
            self.assertFalse(recorder.permits_event(method, 0, True))
            self.assertFalse(recorder.permits_event(method, 1 << 28, False))

    def test_wire_routes_cover_event_writes_and_leave_controls_distinct(self):
        events = f"/api/v10/guilds/{recorder.GUILD}/scheduled-events"
        self.assertTrue(recorder.event_write("POST", events))
        self.assertTrue(recorder.event_write("PATCH", f"{events}/{recorder.EVENT}"))
        self.assertTrue(recorder.event_write("DELETE", f"{events}/{recorder.EVENT}"))
        self.assertFalse(recorder.event_write("GET", f"{events}/{recorder.EVENT}"))
        self.assertFalse(recorder.event_write("PUT", recorder.ROLE_PATH))
        self.assertFalse(recorder.event_write("POST", recorder.MESSAGE_PATH))

    def test_handler_denies_event_create_unless_policy_is_explicitly_open(self):
        cases = ((None, 403), ("invalid\n", 403), ("closed\n", 403), ("open\n", 200))
        for policy, expected in cases:
            with self.subTest(policy=policy), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "barrier").write_text("none\n")
                if policy is not None:
                    (root / "policy").write_text(policy)
                handler = recorder.Handler.__new__(recorder.Handler)
                handler.server = SimpleNamespace(state=root, token="Bot synthetic", lock=threading.Lock())
                handler.headers = {"Authorization": "Bot synthetic"}
                handler.rfile = io.BytesIO()
                handler.command = "POST"
                handler.path = f"/api/v10/guilds/{recorder.GUILD}/scheduled-events"
                handler.reply = Mock()

                handler.do_POST()

                self.assertEqual(handler.reply.call_count, 1)
                self.assertEqual(handler.reply.call_args.args[0], expected)


if __name__ == "__main__":
    unittest.main()
