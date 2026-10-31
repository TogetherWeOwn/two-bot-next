"""Offline regressions for image-restore listing lag; no live traffic or credentials."""

import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import worker_container_rollback as rollback
import test_worker_container_rollback as fixtures


class ImageRestoreListingTests(unittest.TestCase):
    def routes(self, row=None):
        routes, _ = fixtures.RollbackRunTests().routes()
        # The application list never catches up to the restored digest.
        routes[("GET", "/containers/applications")] = lambda: {
            "success": True, "result": [{
                "id": fixtures.APP_ID, "name": fixtures.APPLICATION,
                "durable_objects": {"namespace_id": fixtures.NAMESPACE},
                "configuration": {"image": fixtures.bad_image()}}]}
        if row is not None:
            routes[("GET", f"/containers/applications/{fixtures.APP_ID}/rollouts/r-new")] = \
                lambda: {"success": True, "result": row}
        return routes

    def row(self):
        return fixtures.rollout_row("r-new", fixtures.good_image(), target_version=9)

    def test_stale_listing_accepts_completed_rollout_then_requires_18_ready_polls(self):
        for active, healthy in ((1, 1), (0, 1), (1, 0)):
            with self.subTest(active=active, healthy=healthy):
                row = self.row()
                row["health"]["instances"].update(active=active, healthy=healthy)
                rb, client, log = fixtures.make_rollback(self.routes(row))
                result = rb.run(fixtures.GOOD_VERSION, fixtures.good_image())
                self.assertEqual(rollback.STABLE_PASSES, 18)
                self.assertEqual(result["stable_polls"], 18)
                self.assertEqual(result["gate"], "stable-200-gateway-ready")
                self.assertEqual(result["restored_rollout_id"], "r-new")
                self.assertEqual(log.getvalue().count("gate poll"), 18)
                self.assertEqual(sum(call == ("PROBE", rb.url + "/readyz")
                                     for call in client.calls), 18)

    def test_stale_listing_does_not_bypass_gateway_failure(self):
        routes = self.routes(self.row())
        ready, health = fixtures.ready_probe(fixtures.GOOD_VERSION, gateway="down")
        routes[("PROBE", "readyz")], routes[("PROBE", "health")] = ready, health
        rb, _, _ = fixtures.make_rollback(routes)
        with self.assertRaises(rollback.GateError) as caught:
            rb.run(fixtures.GOOD_VERSION, fixtures.good_image())
        self.assertEqual(str(caught.exception), "gate_timeout")
        self.assertEqual(rb.evidence["restored_rollout_id"], "r-new")
        self.assertNotIn("gate", rb.evidence)

    def test_production_still_requires_listing_digest(self):
        rb, _, _ = fixtures.make_rollback(self.routes(self.row()), variant="production")
        with self.assertRaises(rollback.GateError) as caught:
            rb.restore_image(fixtures.good_image())
        self.assertEqual(str(caught.exception), "image_rollout_timeout")
        self.assertIn("waiting_for=application_image", caught.exception.detail)

    def test_replaced_reverted_wrong_image_and_identity_fail_closed(self):
        for field, value, expected in (
                ("status", "replaced", "rollout_replaced_or_reverted"),
                ("status", "reverted", "rollout_replaced_or_reverted"),
                ("target_configuration", {"image": fixtures.bad_image()}, "rollout_identity_drift"),
                ("id", "r-other", "rollout_identity_drift"),
                ("target_version", 10, "rollout_identity_drift"),
                ("steps", [{"status": "progressing"}], "rollout_steps_incomplete")):
            with self.subTest(field=field, value=value):
                row = self.row()
                row[field] = value
                rb, client, _ = fixtures.make_rollback(self.routes(row))
                with self.assertRaises(rollback.GateError) as caught:
                    rb.run(fixtures.GOOD_VERSION, fixtures.good_image())
                self.assertEqual(str(caught.exception), expected)
                self.assertFalse(any(call[0] == "PROBE" for call in client.calls))
                self.assertNotIn("restored_rollout_id", rb.evidence)

    def test_target_version_can_be_pinned_by_a_later_poll(self):
        routes = self.routes(self.row())
        routes[("POST", f"/containers/applications/{fixtures.APP_ID}/rollouts")] = \
            lambda _body: {"id": "r-new"}
        rb, _, _ = fixtures.make_rollback(routes)
        result = rb.run(fixtures.GOOD_VERSION, fixtures.good_image())
        self.assertEqual(result["restored_rollout_id"], "r-new")
        self.assertEqual(result["stable_polls"], 18)

    def test_incomplete_instance_progress_is_not_accepted(self):
        row = self.row()
        row["progress"]["updated_instances"] = 0
        rb, _, _ = fixtures.make_rollback(self.routes(row))
        with self.assertRaises(rollback.GateError) as caught:
            rb.restore_image(fixtures.good_image())
        self.assertEqual(str(caught.exception), "image_rollout_timeout")
        self.assertIn("waiting_for=counters_or_progress", caught.exception.detail)

    def test_unknown_status_is_not_logged(self):
        marker = "remote-status-must-not-be-logged"
        row = self.row()
        row["status"] = marker
        rb, _, log = fixtures.make_rollback(self.routes(row))
        with self.assertRaises(rollback.GateError) as caught:
            rb.restore_image(fixtures.good_image())
        self.assertEqual(str(caught.exception), "unknown_rollout_status")
        self.assertNotIn(marker, log.getvalue())
        self.assertNotIn(marker, json.dumps(rb.evidence))

    def timeout_case(self, reason):
        row = self.row()
        routes = self.routes(row)
        if reason == "target_version":
            row.pop("target_version")
            routes[("POST", f"/containers/applications/{fixtures.APP_ID}/rollouts")] = \
                lambda _body: {"id": "r-new"}
        elif reason == "rollout_completion":
            row["status"] = "progressing"
        elif reason == "counters_or_progress":
            row["health"]["instances"]["failed"] = 1
        return routes

    def test_timeout_names_each_remaining_accept_conjunct(self):
        for reason in ("target_version", "rollout_completion", "counters_or_progress"):
            with self.subTest(reason=reason):
                rb, _, log = fixtures.make_rollback(self.timeout_case(reason))
                with self.assertRaises(rollback.GateError) as caught:
                    rb.restore_image(fixtures.good_image())
                self.assertEqual(str(caught.exception), "image_rollout_timeout")
                self.assertIn(f"waiting_for={reason}", caught.exception.detail)
                self.assertIn(f"waiting_for={reason}", log.getvalue())

    def test_timeout_main_writes_failure_evidence_without_raw_configuration(self):
        routes = self.timeout_case("counters_or_progress")
        client = fixtures.FakeClient(routes)
        clock = fixtures.Clock()
        real_cls = rollback.Rollback

        def factory(*args, **kwargs):
            return real_cls(*args, **kwargs, now=clock.now, sleep=clock.sleep)

        with tempfile.TemporaryDirectory() as tmp:
            snapshot = Path(tmp) / "snapshot.json"
            # Legacy snapshot avoids unrelated version-provenance fixture calls.
            snapshot.write_text(json.dumps({
                "has_previous": True, "worker": fixtures.WORKER,
                "application": fixtures.APPLICATION, "application_id": fixtures.APP_ID,
                "serving_version": fixtures.GOOD_VERSION, "serving_rollout_id": "r-prev",
                "serving_image": fixtures.good_image()}))
            routes[("GET", f"/containers/applications/{fixtures.APP_ID}/rollouts/r-prev")] = \
                lambda: {"success": True, "result": fixtures.rollout_row(
                    "r-prev", fixtures.good_image())}
            evidence = Path(tmp) / "evidence.json"
            with (mock.patch.object(rollback, "Rollback", factory),
                  mock.patch.object(rollback, "control_client", return_value=fixtures.control_stub()[0]),
                  redirect_stdout(io.StringIO())):
                code = rollback.main([
                    "rollback", "--variant", "staging", "--worker-url",
                    "https://two-bot-next-staging.acct.workers.dev", "--snapshot", str(snapshot),
                    "--actor", "test", "--evidence", str(evidence)],
                    environ=fixtures.MainValidationTests().base_env(),
                    client_factory=lambda _a, _t: client)
            self.assertEqual(code, 1)
            record = json.loads(evidence.read_text())
        self.assertEqual(record["failure"], "image_rollout_timeout")
        self.assertEqual(record["image_restore_wait"], "counters_or_progress")
        self.assertNotIn("gate", record)
        self.assertNotIn("configuration", json.dumps(record))
        self.assertNotIn(fixtures.bad_image(), json.dumps(record))


if __name__ == "__main__":
    unittest.main()
