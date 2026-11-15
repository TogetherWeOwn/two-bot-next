import copy
import unittest

from rest_evidence_continuity import EvidenceError
from rest_watch_sequence import SampleSequence


class SampleSequenceTest(unittest.TestCase):
    def setUp(self):
        self.binding = {"environment": "production", "sha": "a" * 40,
                        "build": "build", "deployment": "deploy", "worker": "worker",
                        "image": "sha256:" + "b" * 64, "run": 12, "attempt": 1,
                        "challenge": "run-challenge"}
        self.anchor = self.sample(1000, 10, 10)
        self.current = self.sample(2000, 20, 20)
        self.state = (copy.deepcopy(self.anchor), {"tick": 10})
        self.verified = []
        self.claims = 0

    def sample(self, wall, tick, count):
        return {"binding": copy.deepcopy(self.binding), "incarnation": "1" * 32,
                "start_ms": wall, "end_ms": wall + 10, "monotonic_ms": tick,
                "series": [{"route": "GET /a", "result": "2xx", "count": count}]}

    def verify(self, sample, proof, expected):
        self.verified.append(copy.deepcopy(proof))
        return proof == {"tick": sample["monotonic_ms"]} and expected == self.binding

    def ledger(self, previous, old_proof, sample, proof, expected):
        self.claims += 1
        if (previous, old_proof) != self.state or expected != self.binding:
            return False
        self.state = (copy.deepcopy(sample), copy.deepcopy(proof))
        return True

    def sequence(self, ledger=None, verifier=None):
        return SampleSequence(self.anchor, {"tick": 10}, self.binding,
                              ledger if ledger is not None else self.ledger,
                              verifier if verifier is not None else self.verify)

    def accept(self, sequence, sample=None, proof=None, tick=20):
        return sequence.accept(self.current if sample is None else sample,
                               {"tick": 20} if proof is None else proof,
                               now_ms=4000, monotonic_ms=tick)

    def test_original_proofs_and_atomic_compare_and_swap(self):
        first, competing = self.sequence(), self.sequence()
        result = self.accept(first)
        self.assertEqual(self.verified, [{"tick": 10}, {"tick": 20}])
        result["series"][0]["count"] = 0
        self.current["series"][0]["count"] = 99
        next_sample = self.sample(3000, 30, 30)
        self.accept(first, next_sample, {"tick": 30}, tick=30)
        self.assertEqual(self.verified[-2:], [{"tick": 20}, {"tick": 30}])
        with self.assertRaises(EvidenceError):
            self.accept(competing, self.sample(2000, 20, 20))
        self.assertEqual(self.state[0], next_sample)
        self.assertEqual(self.claims, 3)

    def test_interval_boundaries(self):
        for delta, allowed in ((0, False), (1, True), (30000, True), (30001, False)):
            with self.subTest(delta=delta):
                self.setUp()
                endpoint = self.sample(2000, 10 + delta, 20)
                sequence = self.sequence()
                if allowed:
                    self.accept(sequence, endpoint, {"tick": 10 + delta}, tick=10 + delta)
                    self.assertEqual(self.claims, 1)
                else:
                    with self.assertRaises(EvidenceError):
                        self.accept(sequence, endpoint, {"tick": 10 + delta}, tick=10 + delta)
                    self.assertEqual(self.claims, 0)

    def test_failures_are_terminal_and_do_not_claim_invalid_samples(self):
        for case in ("proof", "counter", "identity", "verifier_exception", "ledger_false", "ledger_exception"):
            with self.subTest(case=case):
                self.setUp()
                endpoint = copy.deepcopy(self.current)
                if case == "counter":
                    endpoint["series"][0]["count"] = 9
                if case == "identity":
                    endpoint["incarnation"] = "2" * 32
                def fail(*args):
                    raise RuntimeError("private dependency detail")
                ledger = (lambda *args: False) if case == "ledger_false" else (fail if case == "ledger_exception" else None)
                sequence = self.sequence(ledger=ledger, verifier=fail if case == "verifier_exception" else None)
                with self.assertRaises(EvidenceError) as error:
                    self.accept(sequence, endpoint, {"tick": 99} if case == "proof" else {"tick": 20})
                self.assertNotIn("private dependency detail", str(error.exception))
                self.assertEqual(self.claims, 0)
                with self.assertRaisesRegex(EvidenceError, "terminal"):
                    self.accept(sequence)

    def test_interrupted_ledger_is_terminal_before_or_after_commit(self):
        for commit in (False, True):
            with self.subTest(commit=commit):
                self.setUp()
                def interrupted(*args):
                    if commit:
                        self.ledger(*args)
                    raise KeyboardInterrupt()
                sequence = self.sequence(ledger=interrupted)
                with self.assertRaises(KeyboardInterrupt):
                    self.accept(sequence)
                verified = copy.deepcopy(self.verified)
                with self.assertRaisesRegex(EvidenceError, "terminal"):
                    self.accept(sequence)
                self.assertEqual(self.verified, verified)
                self.assertEqual(self.claims, int(commit))

    def test_missing_dependencies_and_non_boolean_ledger(self):
        for proof, ledger, verifier in ((None, self.ledger, self.verify),
                                        ({"tick": 10}, None, self.verify),
                                        ({"tick": 10}, self.ledger, None)):
            with self.subTest(proof=proof, ledger=ledger, verifier=verifier):
                with self.assertRaises(EvidenceError):
                    SampleSequence(self.anchor, proof, self.binding, ledger, verifier)
        with self.assertRaises(EvidenceError):
            self.accept(self.sequence(ledger=lambda *args: 1))
