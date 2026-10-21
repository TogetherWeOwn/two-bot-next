#!/usr/bin/env python3
"""Builder-owned pins for `rollback --dry-run` review findings.

The protected acceptance suite
(`test_worker_container_rollback_dry_run_acceptance.py`) cannot be edited by
the Builder, so the review-requested pins live here instead, reusing its
offline fixtures by import (read-only; the protected file is unchanged):

1. Already-serving fence wording: the first plan line must say fencing is
   deferred to takeover (the live path only fences inside takeover after the
   image rollout in that case), while the worker-leg case keeps the
   pre-restore fence line.
2. Pre-stamp snapshot dry-run: a snapshot with no recorded SHA exits 0 with
   the exact no-sha gate line, no `None` text, and zero POSTs.
3. No evidence write: a dry-run leaves the `--evidence` path absent, and a
   pre-existing evidence file byte-identical.

Offline only: in-memory fakes from the acceptance module, temp snapshot
files, recording control stubs. No network, no credentials, no databases.

Run: PYTHONDONTWRITEBYTECODE=1 python3 ../scripts/test_worker_container_rollback_dry_run_pins.py -v
"""

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
# E402-safe by convention: every scripts/ test adds its own directory first,
# then imports the sibling modules under test (no linter runs on scripts/).
import test_worker_container_rollback_dry_run_acceptance as acc
import worker_container_rollback as rollback


class DryRunPinTests(acc.DryRunCase):
    def test_fence_line_defers_to_takeover_when_already_serving(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "evidence.json"
            client = acc.FakeClient(acc.live_routes(history=(acc.GOOD_VERSION,
                                                              acc.BAD_VERSION)))
            control, calls = acc.control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(acc.manual_argv(evidence), client)
        self.assertEqual(code, 0, out)
        plan = self.plan_lines(out)
        self.assertIn("fence deferred to takeover", plan[0],
                      f"already-serving fence line must defer: {plan}")
        self.assertNotIn("before restoring", plan[0],
                         f"already-serving fence line must not promise a pre-restore fence: {plan}")
        self.assertIn(acc.GOOD_VERSION, plan[1])
        self.assert_no_posts(client, out)
        self.assert_no_mutations(calls, out)

    def test_fence_line_is_pre_restore_when_worker_leg_runs(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "evidence.json"
            client = acc.FakeClient(acc.live_routes())
            control, calls = acc.control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(acc.manual_argv(evidence), client)
        self.assertEqual(code, 0, out)
        plan = self.plan_lines(out)
        self.assertEqual(plan[0],
                         f"dry-run plan: fence the singleton before restoring "
                         f"version {acc.GOOD_VERSION}")
        self.assert_no_posts(client, out)
        self.assert_no_mutations(calls, out)

    def test_pre_stamp_snapshot_dry_run_prints_no_sha_gate(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = acc.write_snapshot(tmp, serving_sha=None,
                                       provenance_complete=False)
            evidence = Path(tmp) / "evidence.json"
            routes = acc.live_routes()
            routes.update(acc.snapshot_reread_routes())
            client = acc.FakeClient(routes)
            control, calls = acc.control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(acc.snapshot_argv(path, evidence),
                                         client)
        self.assertEqual(code, 0, out)
        plan = self.plan_lines(out)
        self.assertEqual(plan[4],
                         f"dry-run plan: gate stable readyz for version "
                         f"{acc.GOOD_VERSION} pre-stamp no-sha")
        self.assertNotIn("None", out, f"plan must never print None: {out[:400]}")
        self.assert_no_posts(client, out)
        self.assert_no_mutations(calls, out)

    def test_dry_run_writes_no_evidence_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            manual_evidence = Path(tmp) / "manual-evidence.json"
            client = acc.FakeClient(acc.live_routes())
            control, _ = acc.control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(acc.manual_argv(manual_evidence),
                                         client)
            self.assertEqual(code, 0, out)
            self.assertFalse(manual_evidence.exists(),
                             "manual dry-run must not write the evidence file")

            snap_path = acc.write_snapshot(tmp)
            snap_evidence = Path(tmp) / "snap-evidence.json"
            routes = acc.live_routes()
            routes.update(acc.snapshot_reread_routes())
            snap_client = acc.FakeClient(routes)
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(acc.snapshot_argv(snap_path,
                                                            snap_evidence),
                                         snap_client)
            self.assertEqual(code, 0, out)
            self.assertFalse(snap_evidence.exists(),
                             "snapshot dry-run must not write the evidence file")

    def test_dry_run_preserves_pre_existing_evidence_file(self):
        sentinel = '{"sentinel": true, "note": "do-not-overwrite"}\n'
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "evidence.json"
            evidence.write_text(sentinel)
            client = acc.FakeClient(acc.live_routes())
            control, _ = acc.control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(acc.manual_argv(evidence), client)
            after = evidence.read_text()
        self.assertEqual(code, 0, out)
        self.assertEqual(after, sentinel,
                         "dry-run must leave a pre-existing evidence file byte-identical")


if __name__ == "__main__":
    unittest.main()
