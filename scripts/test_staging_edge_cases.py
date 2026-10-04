"""Offline staging-smoke edge-case fixtures; stdlib only, no network access."""

import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_edge_cases as edge  # noqa: E402

REPO = Path(__file__).resolve().parent.parent


class EdgeCaseTests(unittest.TestCase):
    def setUp(self):
        # Any real HTTP is a test failure: the classifier never fetches.
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        self.pack = edge.load_pack()

    def test_pack_shape_is_versioned_with_unique_names(self):
        self.assertEqual(self.pack["version"], 1)
        names = [case["name"] for case in self.pack["cases"]]
        self.assertEqual(len(names), len(set(names)))
        self.assertEqual(set(names), {
            "permission_denied", "interaction_timeout", "empty_rank",
            "generic_failure_contrast",
        })
        for case in self.pack["cases"]:
            self.assertTrue(case["command"])
            self.assertTrue(case["expected_outcome"])
            self.assertTrue(case["meaning"])

    def test_every_fixture_classifies_to_its_expected_outcome(self):
        for name, outcome, expected in edge.classify_pack(self.pack):
            with self.subTest(fixture=name):
                self.assertEqual(outcome, expected)

    def test_edge_cases_never_land_on_generic_failure(self):
        for name, outcome, _ in edge.classify_pack(self.pack):
            with self.subTest(fixture=name):
                if name == "generic_failure_contrast":
                    self.assertEqual(outcome, edge.GENERIC_FAILURE)
                else:
                    self.assertNotEqual(outcome, edge.GENERIC_FAILURE)

    def test_outcomes_are_distinct_per_shape(self):
        seen = {}
        for name, outcome, _ in edge.classify_pack(self.pack):
            key = name if name != "generic_failure_contrast" else "generic_failure"
            self.assertNotIn(outcome, seen,
                             f"{name} collides with {seen.get(outcome)}")
            seen[outcome] = key
        self.assertEqual(set(seen), {
            edge.PERMISSION_DENIED, edge.INTERACTION_TIMEOUT,
            edge.EMPTY_RANK, edge.GENERIC_FAILURE,
        })

    def test_unknown_shapes_fall_closed_to_generic_failure(self):
        self.assertEqual(edge.classify_observation({}), edge.GENERIC_FAILURE)
        self.assertEqual(edge.classify_observation(None), edge.GENERIC_FAILURE)
        self.assertEqual(
            edge.classify_observation({"reply": "not registered in the staging guild"}),
            edge.GENERIC_FAILURE)

    def test_timeout_prefix_rejects_nearby_transport_shapes(self):
        # 401/403/429 are credential/access/rate outcomes with their own
        # triage; only the did-not-respond transport shape is a timeout.
        for reason in ("credential refused (401); rotation is an operator decision",
                       "staging application lacks access (403)",
                       "rate limited (429); rerun later",
                       "interactions endpoint answered 500, expected 200"):
            self.assertNotEqual(
                edge.classify_observation({"reply": None, "reason": reason}),
                edge.INTERACTION_TIMEOUT, reason)

    def test_denied_copy_matches_the_router_source(self):
        source = (REPO / "crates" / "core" / "src" / "router.rs").read_text()
        self.assertIn(edge.MANAGE_SERVER_DENIAL, source)

    def test_unranked_label_matches_the_leveling_source(self):
        source = (REPO / "crates" / "core" / "src" / "leveling.rs").read_text()
        self.assertIn(edge.UNRANKED_LABEL, source)

    def test_timeout_shape_matches_the_smoke_source(self):
        source = (REPO / "scripts" / "staging_automation_read_smoke.py").read_text()
        self.assertIn(edge.TIMEOUT_REASON_PREFIX.rstrip("(").rstrip(), source)

    def test_generic_prefix_matches_the_reply_contract_source(self):
        source = (REPO / "crates" / "core" / "src" / "router" / "replies.rs").read_text()
        self.assertIn("Something went wrong (ref {reference})", source)

    def test_pack_carries_no_secret_or_credential(self):
        raw = (REPO / "scripts" / "fixtures" / "staging_edge_cases.json").read_text()
        for token in ("Bot ", "Bearer", "BEGIN ", "DISCORD_STAGING_BOT_TOKEN"):
            self.assertNotIn(token, raw)
        # Only the contrast case may borrow failure copy, and only with the
        # obvious placeholder reference.
        self.assertEqual(raw.count("Something went wrong"), 1)
        self.assertIn("ref 1a2b3c4d", raw)


if __name__ == "__main__":
    unittest.main()
