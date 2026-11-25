"""Pin the provider rules used by the isolated event write rehearsal."""

import importlib.util
import io
import json
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
    def test_channel_event_approval_changes_without_opening_the_guild(self):
        earlier = recorder.EVENT_CHANNEL
        replacement = recorder.REPLACEMENT_EVENT_CHANNEL
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "barrier").write_text("none\n")
            (root / "policy").write_text("closed\n")
            approval = root / "channel-event-approval"
            handler = recorder.Handler.__new__(recorder.Handler)
            handler.server = SimpleNamespace(state=root, token="Bot synthetic", lock=threading.Lock())
            handler.command = "PATCH"
            handler.path = f"/api/v10/guilds/{recorder.GUILD}/scheduled-events/{recorder.EVENT}"
            handler.reply = Mock()

            for approved_scope, requested, status in (
                (None, earlier, 403),
                (f"{recorder.GUILD}:{earlier}", earlier, 200),
                (f"{recorder.GUILD}:{replacement}", earlier, 403),
                (f"{recorder.GUILD}:{replacement}", replacement, 200),
                (f"{recorder.GUILD}:{replacement}", None, 403),
                (f"{recorder.GUILD}0:{earlier}", earlier, 403),
            ):
                with self.subTest(approved_scope=approved_scope, requested=requested):
                    if approved_scope is not None:
                        approval.write_text(f"{approved_scope}\n")
                    body = json.dumps({"entity_type": 2 if requested else 3, "channel_id": requested}).encode()
                    handler.headers = {"Authorization": "Bot synthetic", "Content-Length": str(len(body))}
                    handler.rfile = io.BytesIO(body)
                    handler.reply.reset_mock()
                    handler.do_PATCH()
                    self.assertEqual(handler.reply.call_count, 1)
                    self.assertEqual(handler.reply.call_args.args[0], status)
                    if status == 200:
                        self.assertEqual(handler.reply.call_args.args[1]["channel_id"], requested)

            entries = [json.loads(line) for line in (root / "journal.jsonl").read_text().splitlines()]
            self.assertEqual(entries, [
                ["PATCH", handler.path, "channel-edit", "denied"],
                ["PATCH", handler.path, "channel-edit", "applied"],
                ["PATCH", handler.path, "channel-edit", "denied"],
                ["PATCH", handler.path, "channel-edit", "applied"],
                ["PATCH", handler.path, "edit", "denied"],
                ["PATCH", handler.path, "channel-edit", "denied"],
            ])

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
