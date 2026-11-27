"""Exercise the dormant watch verifier with synthetic, pre-attributed receipts."""

from dataclasses import FrozenInstanceError
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from production_watch_evidence import verify_production_watch_bundle


START = "2026-11-27T12:00:00.123456789Z"
END = "2026-11-27T12:30:00.987654321Z"
MIDPOINT = "2026-11-27T12:15:00Z"
PIN = {
    "environment": "production",
    "candidate_sha": "a" * 40,
    "worker_version": "12345678-1234-4321-8765-123456789abc",
    "image_digest": "sha256:" + "b" * 64,
    "owner_epoch": 7,
    "started_at": START,
    "completed_at": END,
}


def fixture():
    release = dict(PIN, production_lane_non_overlapping=True, production_auto_promote_disabled=True)
    bundle = {
        "naming": dict(PIN, empty_room_rename_at=START, occupied_room_return_at=END),
        "help": dict(PIN, observed_at=MIDPOINT, branding_confirmed=True, cleanup_confirmed=True),
        "gateway": dict(PIN, endpoint_policy_refused_at=MIDPOINT, unexpected_allows=0),
        "event_write_fence": dict(PIN, discord_permissions_readback_at=END,
                                  current_and_retained_discord_fenced=True, cross_rollback_writes=0),
    }
    return release, bundle


class ProductionWatchEvidenceTests(unittest.TestCase):
    def assert_refused(self, release, bundle, reason):
        result = verify_production_watch_bundle(release, bundle, enabled=True)
        self.assertEqual((result.status, result.reason), ("refused", reason))

    def test_disabled_requires_exact_true_and_returns_frozen_result(self):
        disabled = verify_production_watch_bundle()
        self.assertEqual((disabled.status, disabled.reason), ("disabled", None))
        for enabled in (False, 0, 1, None, "true", [True]):
            with self.subTest(enabled=enabled):
                self.assertIs(verify_production_watch_bundle(object(), object(), enabled=enabled), disabled)
        with self.assertRaises(FrozenInstanceError):
            disabled.status = "verified"

    def test_complete_exact_bundle_verifies_at_inclusive_bounds(self):
        release, bundle = fixture()
        result = verify_production_watch_bundle(release, bundle, enabled=True)
        self.assertEqual((result.status, result.reason), ("verified", None))
        self.assertEqual((release, bundle), fixture())

    def test_equivalent_utc_spellings_preserve_sub_microsecond_order(self):
        release, bundle = fixture()
        bundle["naming"].update(
            started_at="2026-11-27T12:00:00.12345678900+00:00",
            empty_room_rename_at="2026-11-27T12:00:00.123456789+0000",
            occupied_room_return_at="2026-11-27T12:00:00.123456790Z",
        )
        self.assertEqual(verify_production_watch_bundle(release, bundle, enabled=True).status, "verified")

    def test_release_pin_must_be_complete_and_valid(self):
        for field, value in (
            ("environment", "staging"), ("candidate_sha", "a" * 39), ("worker_version", "not-a-uuid"),
            ("image_digest", "sha256:" + "b" * 63), ("owner_epoch", True),
            ("owner_epoch", -1), ("owner_epoch", 2**53),
            ("started_at", "2026-11-27T12:00:00"), ("started_at", "2026-11-27T12:00:00+01:00"),
            ("started_at", END), ("completed_at", START), ("completed_at", "2026-11-28T12:00:00Z\n"),
            ("completed_at", "2026-11-27T11:59:59Z"), ("completed_at", "2026-11-31T12:00:00Z"),
        ):
            with self.subTest(field=field, value=value):
                release, bundle = fixture()
                release[field] = value
                self.assert_refused(release, bundle, "invalid_release_pin")
        for field in PIN:
            with self.subTest(missing=field):
                release, bundle = fixture()
                del release[field]
                self.assert_refused(release, bundle, "invalid_release_pin")

    def test_production_lane_and_auto_promotion_require_explicit_confirmation(self):
        for field, reason in (
            ("production_lane_non_overlapping", "production_lane_overlap_or_unverified"),
            ("production_auto_promote_disabled", "production_auto_promote_not_disabled"),
        ):
            for value in (False, "true", 1, None):
                with self.subTest(field=field, value=value):
                    release, bundle = fixture()
                    release[field] = value
                    self.assert_refused(release, bundle, reason)
            with self.subTest(missing=field):
                release, bundle = fixture()
                del release[field]
                self.assert_refused(release, bundle, "invalid_release_pin")

    def test_missing_or_malformed_receipt_refuses_every_domain(self):
        valid_release, valid_bundle = fixture()
        for domain, original in valid_bundle.items():
            with self.subTest(missing=domain):
                self.assert_refused(valid_release, {k: v for k, v in valid_bundle.items() if k != domain},
                                    "invalid_bundle_inventory")
            with self.subTest(malformed=domain):
                self.assert_refused(valid_release, dict(valid_bundle, **{domain: []}), f"invalid_{domain}_receipt")
            for field in original:
                with self.subTest(domain=domain, missing_field=field):
                    release, bundle = fixture()
                    del bundle[domain][field]
                    self.assert_refused(release, bundle, f"invalid_{domain}_receipt")
        self.assert_refused(valid_release, dict(valid_bundle, extra={}), "invalid_bundle_inventory")

    def test_every_domain_rejects_drift_from_any_release_pin(self):
        for domain in fixture()[1]:
            for field, value in (
                ("environment", "staging"), ("candidate_sha", "c" * 40),
                ("worker_version", "12345678-1234-4321-8765-123456789abd"),
                ("image_digest", "sha256:" + "c" * 64), ("owner_epoch", 8), ("owner_epoch", 7.0),
                ("started_at", "2026-11-27T12:00:00.123456788Z"),
                ("completed_at", "2026-11-27T12:30:00.987654322Z"),
            ):
                with self.subTest(domain=domain, field=field, value=value):
                    release, bundle = fixture()
                    bundle[domain][field] = value
                    self.assert_refused(release, bundle, f"invalid_{domain}_receipt")

    def test_all_observations_refuse_out_of_window_and_malformed_timestamps(self):
        for domain, field in (
            ("naming", "empty_room_rename_at"), ("naming", "occupied_room_return_at"),
            ("help", "observed_at"), ("gateway", "endpoint_policy_refused_at"),
            ("event_write_fence", "discord_permissions_readback_at"),
        ):
            for value in ("2026-11-27T12:00:00.123456788Z", "2026-11-27T12:30:00.987654322Z", None):
                with self.subTest(domain=domain, field=field, value=value):
                    release, bundle = fixture()
                    bundle[domain][field] = value
                    self.assert_refused(release, bundle, f"invalid_{domain}_receipt")

    def test_naming_requires_distinct_transitions_in_order(self):
        for earlier, later in ((MIDPOINT, MIDPOINT), (END, START), (START, START[:-1] + "0Z")):
            with self.subTest(earlier=earlier, later=later):
                release, bundle = fixture()
                bundle["naming"].update(empty_room_rename_at=earlier, occupied_room_return_at=later)
                self.assert_refused(release, bundle, "invalid_naming_receipt")

    def test_help_branding_cleanup_and_discord_fence_require_confirmed_outcomes(self):
        for domain, field in (
            ("help", "branding_confirmed"), ("help", "cleanup_confirmed"),
            ("event_write_fence", "current_and_retained_discord_fenced"),
        ):
            for value in (False, 1, "true", None):
                with self.subTest(domain=domain, field=field, value=value):
                    release, bundle = fixture()
                    bundle[domain][field] = value
                    self.assert_refused(release, bundle, f"invalid_{domain}_receipt")

    def test_no_unexpected_gateway_allows_or_cross_rollback_writes(self):
        for domain, field in (("gateway", "unexpected_allows"), ("event_write_fence", "cross_rollback_writes")):
            for value in (1, -1, False, 0.0, "0", None):
                with self.subTest(domain=domain, field=field, value=value):
                    release, bundle = fixture()
                    bundle[domain][field] = value
                    self.assert_refused(release, bundle, f"invalid_{domain}_receipt")
