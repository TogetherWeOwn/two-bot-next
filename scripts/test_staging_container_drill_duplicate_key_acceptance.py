"""Protected acceptance: container drill attestation refuses duplicate keys.

Offline only (stdlib, offline World double, no network). No implementation
source or diff is used to derive expectations. The combined fix must retain
this file byte-identically; the builder must make these tests pass without
editing it.

Contract (baseline 8978f1249): validate_attestation uses bare json.loads,
so a repeated key last-wins before window/count grading. Fixed code uses
object_pairs_hook=_reject_duplicate_keys (precedent:
rollback_readiness_probe.decode_strict) and refuses with the fixed
shape token "invalid_session_attestation", echoing no raw key, value,
body or header. A refused attestation grades nothing and proves no
session; valid attestations keep byte-identical verdicts.
"""

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_staging_container_drill as fixtures

container_drill = fixtures.container_drill
GateError = fixtures.GateError

SENTINEL = "SENTINEL_DUP_ATTEST_5e7a3c1d"

WINDOW_A = {"distinct_sessions": 1,
            "window_start": "1970-01-01T00:00:00Z",
            "window_end": "2286-11-20T17:46:39Z"}
WINDOW_B = {"distinct_sessions": 1,
            "window_start": "1970-01-02T00:00:00Z",
            "window_end": "2286-11-20T17:46:39Z"}


def attestation_with(drill_fragment: str, restore_fragment: str | None = None) -> str:
    restore = restore_fragment if restore_fragment is not None else json.dumps(WINDOW_A)
    return '{"drill": %s, "restore": %s}' % (drill_fragment, restore)


class AttestationDuplicateKeyAcceptanceTests(unittest.TestCase):
    def assert_refused(self, text: str):
        self.assertIn(SENTINEL, text,
                      "each duplicate fixture must carry the sentinel for the no-echo pin")
        with self.assertRaises(GateError) as ctx:
            container_drill.validate_attestation(text)
        self.assertEqual(str(ctx.exception), "invalid_session_attestation")
        detail = getattr(ctx.exception, "detail", None)
        blob = str(ctx.exception) + (str(detail) if detail else "")
        self.assertNotIn(SENTINEL, blob)
        self.assertNotIn("SENTINEL_DUP", blob)

    def assert_refused_without_world_change(self, text: str):
        world = fixtures.World()
        with self.assertRaises(GateError) as ctx:
            fixtures.run_drill(world, attestation=text)
        self.assertEqual(str(ctx.exception), "invalid_session_attestation")
        self.assertEqual(world.deploys, [])
        self.assertNotIn("fence", [call[0] for call in world.calls])

    # -- controls: valid bodies keep byte-identical verdicts -----------------

    def test_valid_covering_attestation_passes(self):
        parsed = container_drill.validate_attestation(fixtures.COVERING_ATTESTATION)
        self.assertEqual(parsed["drill"]["distinct_sessions"], 1)
        world = fixtures.World()
        _, failures, _ = fixtures.run_drill(world)
        self.assertEqual(failures, [])

    def test_valid_covering_attestation_is_byte_identical(self):
        first = container_drill.validate_attestation(fixtures.COVERING_ATTESTATION)
        second = container_drill.validate_attestation(fixtures.COVERING_ATTESTATION)
        self.assertEqual(first, second)
        self.assertEqual(json.dumps(first, sort_keys=True),
                         json.dumps(second, sort_keys=True))

    def test_absent_attestation_still_not_proven(self):
        self.assertIsNone(container_drill.validate_attestation(""))
        world = fixtures.World()
        _, failures, _ = fixtures.run_drill(world, attestation="")
        self.assertEqual(failures, ["drill:session_not_proven",
                                   "restore:session_not_proven"])

    # -- top-level duplicates: conflicting both orders, identical ------------

    def test_top_level_drill_conflicting_order_a_then_b_refused(self):
        # Last-wins today: WINDOW_B (valid) survives and the drill passes.
        dup = '{"drill": %s, "drill": %s, "restore": %s, "note": "%s"}' % (
            json.dumps(WINDOW_A), json.dumps(WINDOW_B),
            json.dumps(WINDOW_A), SENTINEL)
        # Sanity: bare loads grades the survivor as valid.
        self.assertEqual(json.loads(dup)["drill"], WINDOW_B)
        # The trailing note is inside the raw body only for the echo pin;
        # the fixed decoder must refuse before it is ever graded. Strip it
        # for the validation call: duplicates alone must refuse.
        core = '{"drill": %s, "drill": %s, "restore": %s}' % (
            json.dumps(WINDOW_A), json.dumps(WINDOW_B), json.dumps(WINDOW_A))
        core_with_sentinel = core[:-1] + ', "sentinel": "%s"}' % SENTINEL
        # The sentinel field would already be invalid; instead hide the
        # sentinel in the overwritten first value so last-wins stays valid.
        overwritten = '{"drill": {"distinct_sessions": 1, "window_start": "%s", "window_end": "%s"}, "drill": %s, "restore": %s}' % (
            SENTINEL, WINDOW_A["window_end"], json.dumps(WINDOW_B), json.dumps(WINDOW_A))
        self.assertEqual(json.loads(overwritten)["drill"], WINDOW_B)
        self.assert_refused(overwritten)

    def test_top_level_drill_conflicting_order_b_then_a_refused(self):
        overwritten = '{"drill": {"distinct_sessions": 1, "window_start": "%s", "window_end": "%s"}, "drill": %s, "restore": %s}' % (
            SENTINEL, WINDOW_B["window_end"], json.dumps(WINDOW_A), json.dumps(WINDOW_A))
        self.assertEqual(json.loads(overwritten)["drill"], WINDOW_A)
        self.assert_refused(overwritten)

    def test_top_level_drill_identical_values_refused(self):
        # Same valid window twice: last-wins passes today.
        dup = '{"drill": %s, "drill": %s, "restore": %s}' % (
            json.dumps(WINDOW_A), json.dumps(WINDOW_A), json.dumps(WINDOW_A))
        self.assertEqual(json.loads(dup)["drill"], WINDOW_A)
        with_sentinel = dup[:-1] + ', "drill": {"distinct_sessions": 1, "window_start": "%s", "window_end": "%s"}}' % (
            WINDOW_A["window_start"], WINDOW_A["window_end"])
        # Three drills now; simplify: identical pair plus sentinel overwrite.
        identical = ('{"drill": %s, "drill": %s, "restore": %s, '
                     '"_sentinel": "%s"}') % (
                         json.dumps(WINDOW_A), json.dumps(WINDOW_A),
                         json.dumps(WINDOW_A), SENTINEL)
        # The extra key would already be invalid; use the overwritten-value
        # shape instead so the only defect is the duplicate.
        _ = with_sentinel
        overwritten_identical = ('{"drill": {"distinct_sessions": 1, '
                                 '"window_start": "%s", "window_end": "%s", "pad": 0}, '
                                 '"drill": %s, "restore": %s}') % (
                                     SENTINEL, WINDOW_A["window_end"],
                                     json.dumps(WINDOW_A), json.dumps(WINDOW_A))
        # First drill carries the sentinel but is overwritten by the valid
        # identical survivor; bare loads still validates.
        survivor = json.loads(overwritten_identical)["drill"]
        self.assertEqual(survivor, WINDOW_A)
        _ = identical
        self.assert_refused(overwritten_identical)

    def test_top_level_restore_duplicate_refused(self):
        overwritten = '{"drill": %s, "restore": {"distinct_sessions": 1, "window_start": "%s", "window_end": "%s"}, "restore": %s}' % (
            json.dumps(WINDOW_A), SENTINEL,
            WINDOW_A["window_end"], json.dumps(WINDOW_A))
        self.assertEqual(json.loads(overwritten)["restore"], WINDOW_A)
        self.assert_refused(overwritten)

    # -- nested duplicates ----------------------------------------------------

    def test_nested_window_start_conflicting_both_orders_refused(self):
        for first, second in (("1970-01-01T00:00:00Z", "1970-01-02T00:00:00Z"),
                              ("1970-01-02T00:00:00Z", "1970-01-01T00:00:00Z")):
            with self.subTest(first=first, second=second):
                drill = ('{"distinct_sessions": 1, "window_start": "%s", '
                         '"window_start": "%s", "window_end": "%s", '
                         '"pad": "%s"}') % (first, second,
                                            WINDOW_A["window_end"], SENTINEL)
                # The pad carries the sentinel but is overwritten-away only
                # in shape; bare loads still sees it, so drop the pad and
                # hide the sentinel in the overwritten start instead.
                _ = drill
                drill_core = ('{"distinct_sessions": 1, '
                              '"window_start": "%s", "window_start": "%s", '
                              '"window_end": "%s"}') % (SENTINEL, second,
                                                       WINDOW_A["window_end"])
                # Overwritten sentinel, valid survivor.
                text = attestation_with(drill_core)
                self.assertEqual(json.loads(text)["drill"]["window_start"], second)
                self.assert_refused(text)

    def test_nested_distinct_sessions_identical_refused(self):
        drill = ('{"distinct_sessions": 1, "distinct_sessions": 1, '
                 '"window_start": "%s", "window_end": "%s", '
                 '"window_start": "%s"}') % (
                     WINDOW_A["window_start"], WINDOW_A["window_end"],
                     WINDOW_A["window_start"])
        # Two duplicates (sessions identical, start identical); bare loads
        # still validates.
        parsed = json.loads('{"drill": %s, "restore": %s}' % (
            drill, json.dumps(WINDOW_A)))
        self.assertEqual(parsed["drill"]["distinct_sessions"], 1)
        # Add the sentinel as an overwritten nested value.
        drill_sentinel = ('{"distinct_sessions": 1, "distinct_sessions": 1, '
                          '"window_start": "%s", "window_start": "%s", '
                          '"window_end": "%s"}') % (
                              SENTINEL, WINDOW_A["window_start"],
                              WINDOW_A["window_end"])
        text = attestation_with(drill_sentinel)
        self.assertEqual(json.loads(text)["drill"]["distinct_sessions"], 1)
        self.assert_refused(text)

    def test_nested_restore_window_duplicate_refused(self):
        drill = json.dumps(WINDOW_A)
        restore = ('{"distinct_sessions": 1, "window_start": "%s", '
                   '"window_start": "%s", "window_end": "%s"}') % (
                       SENTINEL, WINDOW_A["window_start"], WINDOW_A["window_end"])
        text = '{"drill": %s, "restore": %s}' % (drill, restore)
        self.assert_refused(text)

    # -- end-to-end: refused bodies dispatch nothing --------------------------

    def test_end_to_end_duplicate_dispatches_nothing(self):
        overwritten = '{"drill": {"distinct_sessions": 1, "window_start": "%s", "window_end": "%s"}, "drill": %s, "restore": %s}' % (
            SENTINEL, WINDOW_A["window_end"], json.dumps(WINDOW_B), json.dumps(WINDOW_A))
        self.assert_refused_without_world_change(overwritten)


if __name__ == "__main__":
    unittest.main()
