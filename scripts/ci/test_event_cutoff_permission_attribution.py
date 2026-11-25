"""Replay the operator's evidence comparison with synthetic records only."""

import unittest

from test_event_cutoff_release_positions import PINS, capture_runner, load_fixture


def compare_attribution(position, pin, receipt, requests):
    """Compare projections of already checked evidence in this example."""
    nonce = receipt["scope_nonce"]
    matches = [request for request in requests if request["nonce"].lower() == nonce.lower()]
    if len(matches) != 1 or matches[0]["nonce"] != nonce:
        return "attribution gap: signed request"

    expected = {
        "position": position,
        "image_digest": pin["image"].split("@", 1)[1],
        "source_revision": pin["source_revision"],
        "worker_version": pin["worker_version"],
        "guild_id": pin["guild_id"],
        "bot_id": pin["bot_id"],
    }
    for field, value in expected.items():
        actual = matches[0].get(field)
        if not actual:
            return f"attribution gap: {field}"
        if actual != value:
            return f"attribution mismatch: {field}"
    return "matches release position"


class EventCutoffPermissionAttributionTest(unittest.TestCase):
    def setUp(self):
        # Reviewed pins, private request attribution (including the separate
        # Worker evidence), and redacted HTTP responses each start independently.
        self.releases = {
            "current": {
                **PINS["current"], "worker_version": "00000000-0000-4000-8000-000000000001",
                "guild_id": "1", "bot_id": "2",
            },
            "retained": {
                **PINS["retained"], "worker_version": "00000000-0000-4000-8000-000000000002",
                "guild_id": "1", "bot_id": "3",
            },
        }
        self.requests = [
            {
                "nonce": "a" * 32, "position": "current", "image_digest": "sha256:" + "a" * 64,
                "source_revision": "1" * 40, "worker_version": "00000000-0000-4000-8000-000000000001",
                "guild_id": "1", "bot_id": "2",
            },
            {
                "nonce": "b" * 32, "position": "retained", "image_digest": "sha256:" + "b" * 64,
                "source_revision": "2" * 40, "worker_version": "00000000-0000-4000-8000-000000000002",
                "guild_id": "1", "bot_id": "3",
            },
        ]
        counts = {"channels": 1, "create_events": 0, "manage_events": 0, "either_event_permission": 0}
        permissions = {
            "owner_or_admin": False, "guild_create_events": False, "guild_manage_events": False,
            "voice": counts, "stage": counts,
        }
        self.receipts = {
            "current": {
                "v": 1, "scope_nonce": "a" * 32,
                "observed_at_unix_seconds": 100, "permissions": permissions,
            },
            "retained": {
                "v": 1, "scope_nonce": "b" * 32,
                "observed_at_unix_seconds": 100, "permissions": permissions,
            },
        }

    def compare(self, position):
        return compare_attribution(
            position, self.releases[position], self.receipts[position], self.requests,
        )

    def test_synthetic_pair_matches_reviewed_positions(self):
        load_fixture("verify_release_positions").verify(PINS, capture_runner(PINS), 0)
        for position in self.releases:
            with self.subTest(position=position):
                self.assertEqual(self.compare(position), "matches release position")

    def test_retained_response_cannot_prove_current(self):
        self.receipts["current"] = self.receipts["retained"]
        self.assertEqual(self.compare("current"), "attribution mismatch: position")

    def test_same_revision_wrong_digest_cannot_prove_current(self):
        self.requests[0]["image_digest"] = self.requests[1]["image_digest"]
        self.assertEqual(self.compare("current"), "attribution mismatch: image_digest")

    def test_conflicting_worker_revision_or_scope_cannot_prove_retained(self):
        for field, wrong_value in (
            ("worker_version", self.requests[0]["worker_version"]),
            ("source_revision", self.requests[0]["source_revision"]),
            ("guild_id", "4"),
            ("bot_id", self.requests[0]["bot_id"]),
        ):
            with self.subTest(field=field):
                original = self.requests[1][field]
                self.requests[1][field] = wrong_value
                self.assertEqual(self.compare("retained"), f"attribution mismatch: {field}")
                self.requests[1][field] = original

    def test_receipt_alone_leaves_attribution_gap(self):
        self.requests.pop(0)
        self.assertEqual(self.compare("current"), "attribution gap: signed request")

    def test_reused_nonce_leaves_attribution_gap(self):
        self.requests[1]["nonce"] = self.requests[0]["nonce"].upper()
        self.assertEqual(self.compare("current"), "attribution gap: signed request")

    def test_missing_identity_field_leaves_attribution_gap(self):
        for field in ("image_digest", "source_revision", "worker_version", "guild_id", "bot_id"):
            with self.subTest(field=field):
                original = self.requests[1].pop(field)
                self.assertEqual(self.compare("retained"), f"attribution gap: {field}")
                self.requests[1][field] = original


if __name__ == "__main__":
    unittest.main()
