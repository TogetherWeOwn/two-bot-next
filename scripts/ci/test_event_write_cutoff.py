"""Pin the provider rules used by the isolated event write rehearsal."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tests" / "event_write_cutoff"))
from recorder import (  # noqa: E402
    ADMINISTRATOR, CREATE_EVENTS, EVENT, GUILD, MANAGE_EVENTS, MESSAGE_PATH, ROLE_PATH,
    event_write, permits_event,
)


class EventWriteCutoffTest(unittest.TestCase):
    def test_provider_requires_both_event_permissions_removed(self):
        self.assertTrue(permits_event("POST", CREATE_EVENTS, False))
        self.assertFalse(permits_event("POST", MANAGE_EVENTS, False))
        for method in ("PATCH", "DELETE"):
            self.assertTrue(permits_event(method, CREATE_EVENTS, True))
            self.assertFalse(permits_event(method, CREATE_EVENTS, False))
            self.assertTrue(permits_event(method, MANAGE_EVENTS, False))
        for method in ("POST", "PATCH", "DELETE"):
            self.assertTrue(permits_event(method, ADMINISTRATOR, True))
            self.assertFalse(permits_event(method, 0, True))
            self.assertFalse(permits_event(method, 1 << 28, False))

    def test_wire_routes_cover_event_writes_and_leave_controls_distinct(self):
        events = f"/api/v10/guilds/{GUILD}/scheduled-events"
        self.assertTrue(event_write("POST", events))
        self.assertTrue(event_write("PATCH", f"{events}/{EVENT}"))
        self.assertTrue(event_write("DELETE", f"{events}/{EVENT}"))
        self.assertFalse(event_write("GET", f"{events}/{EVENT}"))
        self.assertFalse(event_write("PUT", ROLE_PATH))
        self.assertFalse(event_write("POST", MESSAGE_PATH))


if __name__ == "__main__":
    unittest.main()
