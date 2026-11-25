"""Synthetic qualification against the exact takeover protocol."""

import unittest
from unittest.mock import Mock, call

from staging_predecessor import GateError, exercise


class StagingPredecessorTests(unittest.TestCase):
    def test_fences_after_readiness_failure_and_lost_takeover_reply(self):
        states = [{"owner": {"phase": phase, "deploymentId": deployment, "epoch": epoch, "actor": "run"},
                   "running": False} for phase, deployment, epoch in
                  (("fenced", None, 4), ("active", "candidate", 5), ("fenced", None, 6))]
        for lost_reply in (False, True):
            with self.subTest(lost_reply=lost_reply):
                read = Mock(side_effect=states if lost_reply else [*states[:2], *states[1:]])
                change = Mock(side_effect=[GateError("reply_lost") if lost_reply else states[1], states[2]])
                with self.assertRaisesRegex(GateError, "reply_lost|ready_failed"):
                    exercise(read, change, Mock(side_effect=GateError("ready_failed")), "candidate", "run")
                self.assertEqual(change.call_args_list, [call("takeover-exact", 4), call("fence-exact", 5)])
