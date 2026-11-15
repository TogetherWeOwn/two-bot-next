import copy
import json
import unittest

from rest_evidence_continuity import EvidenceError
from rest_watch_handoff import claim_handoff


class HandoffTest(unittest.TestCase):
    def setUp(self):
        self.binding = {"environment": "production", "sha": "a" * 40,
                        "build": "build", "deployment": "deploy", "worker": "worker",
                        "image": "sha256:" + "b" * 64, "run": 1, "attempt": 1,
                        "challenge": "challenge"}
        previous = dict(self.binding, deployment="old", challenge="old")
        def sample(binding, tick, count):
            return {"binding": copy.deepcopy(binding), "incarnation": "1" * 32,
                    "start_ms": tick, "end_ms": tick + 1, "monotonic_ms": tick,
                    "series": [{"route": "GET /a", "result": "2xx", "count": count}]}
        self.record = {"kind": "sparse_rest_watch_handoff", "version": 1,
                       "contract": "v1", "handoff_id": "handoff", "clock_domain": "clock",
                       "mutation_tick": 50000, "transition_tick": 80000, "snapshot": "snapshot",
                       "candidate_expected": self.binding,
                       "prior": {"expected": previous, "samples": [sample(previous, 20000, 0),
                                                                       sample(previous, 40000, 10)],
                                 "proofs": ["prior-start", "prior-end"]},
                       "initial": {"expected": self.binding, "samples": [sample(self.binding, 51000, 0),
                                                                            sample(self.binding, 79000, 10)],
                                   "proofs": ["initial-start", "initial-end"]},
                       "routes": ["GET /a"], "baseline": {"global": [0, 10], "GET /a": [0, 10]},
                       "evidence": {"reference": "verified-by-handoff-adapter"}}
        self.claimed = set()

    def claim(self, record):
        if record["handoff_id"] in self.claimed:
            return False
        self.claimed.add(record["handoff_id"])
        return True

    def start(self, record=None, **overrides):
        args = {"now_ms": 80001, "monotonic_ms": 80000, "clock_domain": "clock",
                "verify_handoff": lambda item: True, "claim": self.claim,
                "ledger": lambda *args: True, "verify_sample": lambda sample, proof, expected: True}
        args.update(overrides)
        return claim_handoff(json.dumps(record or self.record).encode(), **args)

    def test_claim_initializes_inactive_components_and_rejects_second_consumer(self):
        transitions = []
        _, sequence, schedule = self.start(ledger=lambda *args: transitions.append(args) or True)
        endpoint = copy.deepcopy(self.record["initial"]["samples"][1])
        endpoint.update(start_ms=90000, end_ms=90001, monotonic_ms=90000)
        sequence.accept(endpoint, "next-proof", now_ms=90001, monotonic_ms=90000)
        self.assertEqual(transitions[0][0], self.record["initial"]["samples"][1])
        self.assertEqual(transitions[0][1], "initial-end")
        self.assertEqual(schedule.end, 1880000)
        self.assertFalse(schedule.complete)
        with self.assertRaises(EvidenceError):
            self.start()

    def test_invalid_inputs_fail_before_claim(self):
        for case in ("clock", "stale", "baseline", "sparse", "wrong_binding", "proof", "missing_verifier"):
            with self.subTest(case=case):
                record = copy.deepcopy(self.record)
                overrides = {}
                if case == "clock":
                    overrides["clock_domain"] = "different"
                elif case == "stale":
                    overrides["monotonic_ms"] = 109001
                elif case == "baseline":
                    record["baseline"]["global"] = [0, 11]
                elif case == "sparse":
                    record["initial"]["samples"][1]["series"][0]["count"] = 9
                elif case == "wrong_binding":
                    record["initial"]["samples"][1]["binding"]["attempt"] = 2
                elif case == "proof":
                    record["initial"]["proofs"][0] = None
                else:
                    overrides["verify_handoff"] = None
                with self.assertRaises(EvidenceError):
                    self.start(record, **overrides)
                self.assertFalse(self.claimed)

    def test_prior_observed_route_cannot_be_omitted(self):
        record = copy.deepcopy(self.record)
        for sample in record["prior"]["samples"]:
            sample["series"].append({"route": "GET /omitted", "result": "transport", "count": 0})
        with self.assertRaises(EvidenceError):
            self.start(record)
        self.assertFalse(self.claimed)

    def test_authentication_and_claim_exceptions_fail_closed(self):
        with self.assertRaises(EvidenceError):
            self.start(verify_handoff=lambda item: False)
        self.assertFalse(self.claimed)
        with self.assertRaises(EvidenceError):
            self.start(claim=lambda item: False)

    def test_original_proof_contexts_and_binding_are_verified(self):
        seen = []
        def verify(sample, proof, expected):
            seen.append((sample, proof, expected))
            return True
        self.start(verify_sample=verify)
        self.assertEqual([item[1] for item in seen],
                         ["prior-start", "prior-end", "initial-start", "initial-end"])
        for item, block, index in zip(seen, ("prior", "prior", "initial", "initial"), (0, 1, 0, 1)):
            self.assertEqual(item[0], self.record[block]["samples"][index])
            self.assertEqual(item[2], self.record[block]["expected"])

    def test_rejected_or_raising_dependencies_never_claim(self):
        def fail(*args):
            raise ValueError("untrusted detail")
        for overrides in ({"verify_sample": lambda *args: False}, {"verify_sample": fail},
                          {"verify_handoff": fail}, {"ledger": None}, {"claim": None}):
            with self.subTest(overrides=overrides):
                with self.assertRaises(EvidenceError) as caught:
                    self.start(**overrides)
                self.assertNotIn("untrusted detail", str(caught.exception))
                self.assertFalse(self.claimed)

    def test_bookends_and_deadlines_are_required(self):
        for block, index, field, value in (
                ("prior", 0, "monotonic_ms", 40000),
                ("prior", 1, "monotonic_ms", 19999),
                ("initial", 0, "monotonic_ms", 81000),
                ("initial", 1, "monotonic_ms", 81000),
                ("initial", 1, "incarnation", "2" * 32)):
            with self.subTest(block=block, index=index, field=field):
                record = copy.deepcopy(self.record)
                record[block]["samples"][index][field] = value
                with self.assertRaises(EvidenceError):
                    self.start(record)
                self.assertFalse(self.claimed)

    def test_manifest_duplicates_extra_routes_and_baseline_fail_closed(self):
        for change in ("duplicate", "missing", "baseline", "overflow"):
            with self.subTest(change=change):
                record = copy.deepcopy(self.record)
                if change == "duplicate":
                    record["routes"].append("GET /a")
                elif change == "missing":
                    record["routes"] = []
                elif change == "baseline":
                    record["baseline"]["GET /a"] = [1, 10]
                else:
                    record["routes"] = [f"GET /{n}" for n in range(86)]
                with self.assertRaises(EvidenceError):
                    self.start(record)
                self.assertFalse(self.claimed)

    def test_raw_input_is_bounded_and_duplicate_keys_are_refused(self):
        options = dict(now_ms=80001, monotonic_ms=80000, clock_domain="clock",
                       verify_handoff=lambda item: True, claim=self.claim,
                       ledger=lambda *args: True, verify_sample=lambda *args: True)
        nonfinite = copy.deepcopy(self.record)
        nonfinite["evidence"]["value"] = float("nan")
        for raw in (b" " * 65537, b'{"kind": 1, "kind": 2}', b"\xff",
                    json.dumps(nonfinite).encode()):
            with self.subTest(size=len(raw)):
                with self.assertRaises(EvidenceError):
                    claim_handoff(raw, **options)
                self.assertFalse(self.claimed)
