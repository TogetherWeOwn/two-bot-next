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


if __name__ == "__main__":
    unittest.main()
