import copy
import unittest

from rest_evidence_continuity import (
    MAX_COUNTER,
    EvidenceError,
    parse_samples,
    validate_continuity,
)


class ContinuityTest(unittest.TestCase):
    def setUp(self):
        self.binding = {"environment": "production", "sha": "a" * 40, "build": "12-1",
                        "deployment": "deploy-1", "worker": "worker-1",
                        "image": "sha256:" + "b" * 64, "run": 12, "attempt": 1,
                        "challenge": "fresh-run-challenge"}
        self.samples = [self.sample(1000, 1, 3), self.sample(2000, 2, 4)]

    def sample(self, time, tick, count):
        return {"binding": copy.deepcopy(self.binding), "incarnation": "1" * 32,
                "start_ms": time, "end_ms": time + 10, "monotonic_ms": tick,
                "series": [{"route": "GET /users/@me", "result": "2xx", "count": count},
                           {"route": "GET /users/@me", "result": "5xx", "count": 0}]}

    def validate(self, verifier=lambda sample, expected: True, now=2020, tick=3):
        return validate_continuity(self.samples, self.binding, now_ms=now,
                                   monotonic_ms=tick, verify_sample=verifier)

    def test_valid_handoff_is_not_an_admission(self):
        handoff = self.validate()
        self.assertEqual(handoff["kind"], "continuity_only")
        self.assertEqual(handoff["binding"], self.binding)
        self.assertEqual(handoff["series"][0]["count"], 4)
        self.assertNotIn("verdict", handoff)

    def test_requires_authenticated_same_sample_verifier(self):
        for verifier in (None, lambda sample, expected: False, lambda sample, expected: 1):
            with self.subTest(verifier=verifier), self.assertRaises(EvidenceError):
                self.validate(verifier)

    def test_every_binding_must_match(self):
        for key, replacement in (("environment", "staging"), ("sha", "b" * 40),
                                 ("build", "12-2"), ("deployment", "other"),
                                 ("worker", "other"), ("image", "sha256:" + "c" * 64),
                                 ("run", 13), ("attempt", 2), ("challenge", "replay")):
            with self.subTest(key=key):
                self.samples[1]["binding"][key] = replacement
                with self.assertRaises(EvidenceError):
                    self.validate()
                self.samples[1]["binding"][key] = self.binding[key]

    def test_missing_and_malformed_identity(self):
        for token in (None, "0" * 32, "1" * 31, "2" * 32):
            with self.subTest(token=token):
                self.samples[1]["incarnation"] = token
                with self.assertRaises(EvidenceError):
                    self.validate()

    def test_series_reset_missing_duplicate_and_invalid(self):
        bad_rows = [[], [self.samples[0]["series"][0]],
                    self.samples[0]["series"] * 2,
                    [{"route": "GET /users/@me", "result": "2xx", "count": 2},
                     self.samples[0]["series"][1]]]
        for value in (-1, True, 1.5, float("nan"), MAX_COUNTER + 1):
            bad_rows.append([{"route": "GET /users/@me", "result": "2xx", "count": value}])
        for rows in bad_rows:
            with self.subTest(rows=rows):
                self.samples[1]["series"] = rows
                with self.assertRaises(EvidenceError):
                    self.validate()

    def test_collection_times_and_replay(self):
        for field, value in (("start_ms", 500), ("end_ms", 999),
                             ("end_ms", 20000), ("monotonic_ms", 1),
                             ("monotonic_ms", 4)):
            with self.subTest(field=field, value=value):
                original = self.samples[1][field]
                self.samples[1][field] = value
                with self.assertRaises(EvidenceError):
                    self.validate()
                self.samples[1][field] = original
        with self.assertRaises(EvidenceError):
            self.validate(now=32011)
        with self.assertRaises(EvidenceError):
            self.validate(tick=30003)

    def test_parser_rejects_duplicates_and_unbounded_input(self):
        self.assertEqual(parse_samples(b'[{"a": 1}]'), [{"a": 1}])
        for raw in (b'{"a": 1, "a": 2}', b'\xff', b' ' * 65537):
            with self.subTest(raw=raw[:20]), self.assertRaises(EvidenceError):
                parse_samples(raw)

    def test_missing_fields_and_bounds(self):
        self.samples[1].pop("series")
        with self.assertRaises(EvidenceError):
            self.validate()
        self.samples = []
        with self.assertRaises(EvidenceError):
            self.validate()


class WatchWindowTest(unittest.TestCase):
    def setUp(self):
        from rest_watch_window import assess_window
        self.assess = assess_window
        self.routes = ["GET /a", "GET /b"]
        self.baseline = {"global": (0, 20), "GET /a": (0, 10), "GET /b": (0, 10)}
        self.start = [{"route": route, "result": result, "count": count}
                      for route in self.routes
                      for result, count in (("2xx", 100), ("5xx", 5), ("transport", 50))]

    def end(self, a_ok, a_errors, b_ok, b_errors):
        rows = copy.deepcopy(self.start)
        for row in rows:
            values = (a_ok, a_errors) if row["route"] == "GET /a" else (b_ok, b_errors)
            row["count"] += {"2xx": values[0], "5xx": values[1], "transport": 100}.get(row["result"], 0)
        return rows

    def judge(self, end):
        return self.assess(self.start, end, self.routes, self.baseline)

    def test_new_volume_only_and_transport_excluded(self):
        result = self.judge(self.end(10, 0, 10, 0))
        self.assertEqual(result["assessment"], "qualified")
        self.assertEqual(result["counts"]["global"], {"errors": 0, "total": 20})
        self.assertEqual(self.judge(self.end(9, 0, 10, 0))["thin"], ["GET /a"])
        self.assertEqual(self.judge(self.end(0, 0, 0, 0))["assessment"], "thin")

    def test_route_regression_is_not_diluted_and_beats_other_thin_route(self):
        result = self.judge(self.end(9, 1, 0, 0))
        self.assertEqual(result["assessment"], "regression")
        self.assertIn("GET /a", result["regressions"])
        self.assertIn("GET /b", result["thin"])
        result = self.judge(self.end(9, 1, 1000, 0))
        self.assertEqual(result["assessment"], "regression")

    def test_five_percent_equality_and_one_error_over(self):
        self.assertEqual(self.judge(self.end(19, 1, 20, 0))["assessment"], "qualified")
        self.assertEqual(self.judge(self.end(18, 2, 20, 0))["assessment"], "regression")

    def test_rejects_reset_unanchored_and_invalid_baseline(self):
        end = self.end(10, 0, 10, 0)
        end[0]["count"] -= 11
        with self.assertRaises(EvidenceError):
            self.judge(end)
        end = self.end(10, 0, 10, 0)
        end.append({"route": "GET /new", "result": "2xx", "count": 1})
        with self.assertRaises(EvidenceError):
            self.judge(end)
        self.baseline["GET /a"] = (0, 9)
        with self.assertRaises(EvidenceError):
            self.judge(self.end(10, 0, 10, 0))


class WindowScheduleTest(unittest.TestCase):
    def setUp(self):
        from rest_watch_window import WindowSchedule
        self.schedule = WindowSchedule(
            self.rows(1000), 0, 10000, ["GET /a"],
            {"global": (0, 10), "GET /a": (0, 10)})

    @staticmethod
    def rows(count):
        return [{"route": "GET /a", "result": "2xx", "count": count},
                {"route": "GET /a", "result": "5xx", "count": 0}]

    def test_full_watch_judges_final_partial_window(self):
        tick = 0
        while tick < self.schedule.end:
            tick = min(tick + 30000, self.schedule.deadline)
            outcome = self.schedule.observe(self.rows(1000 + tick // 1000), tick)
        self.assertEqual(outcome, "complete")
        self.assertEqual(len(self.schedule.windows), 8)
        self.assertEqual(self.schedule.windows[-1]["end_tick"], 1810000)
        self.assertEqual(self.schedule.windows[-1]["counts"]["global"]["total"], 130)

    def test_delayed_handoff_and_thin_traffic_do_not_move_deadlines(self):
        self.assertEqual(self.schedule.deadline, 240000)
        self.schedule.observe(self.rows(1000), 30000)
        self.assertEqual(self.schedule.deadline, 240000)
        with self.assertRaises(EvidenceError):
            self.schedule.check_deadline(60000)
        self.assertTrue(self.schedule.failed)

    def test_thin_window_and_intermediate_reset_are_terminal(self):
        for tick in range(30000, 240000, 30000):
            self.schedule.observe(self.rows(1000), tick)
        with self.assertRaises(EvidenceError):
            self.schedule.observe(self.rows(1009), 240000)
        with self.assertRaises(EvidenceError):
            self.schedule.observe(self.rows(1010), 240001)

    def test_reset_cannot_be_concealed_by_later_growth(self):
        self.schedule.observe(self.rows(1030), 30000)
        with self.assertRaises(EvidenceError):
            self.schedule.observe(self.rows(1020), 60000)


if __name__ == "__main__":
    unittest.main()
