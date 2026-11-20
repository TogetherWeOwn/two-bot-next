import copy
import unittest

from rest_evidence_continuity import EvidenceError, MAX_COUNTER
from rest_watch_collection import CollectionDeadline
from rest_watch_sequence import SampleSequence
from rest_watch_window_policy import EvidenceGate


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

    def ledger(self, previous, old_proof, sample, proof, expected, limits):
        self.claims += 1
        if (previous, old_proof) != self.state or expected != self.binding:
            return False
        self.state = (copy.deepcopy(sample), copy.deepcopy(proof))
        return True

    def bounded_claim(self, reading, *claim):
        wall, tick, domain = reading
        limits = claim[-1]
        if (domain != limits["clock_domain"]
                or not limits["wall_not_before"] <= wall <= limits["wall_deadline"]
                or not limits["tick_not_before"] <= tick <= limits["tick_deadline"]):
            return False
        return self.ledger(*claim)

    def sequence(self, ledger=None, verifier=None):
        return SampleSequence(self.anchor, {"tick": 10}, self.binding,
                              ledger if ledger is not None else self.ledger,
                              verifier if verifier is not None else self.verify)

    def accept(self, sequence, sample=None, proof=None, tick=20):
        return sequence.accept(self.current if sample is None else sample,
                               {"tick": 20} if proof is None else proof,
                               now_ms=4000, monotonic_ms=tick,
                               before_claim=lambda current: {
                                   "clock_domain": "clock", "wall_not_before": 4000,
                                   "tick_not_before": tick,
                                   "wall_deadline": current["end_ms"] + 30000,
                                   "tick_deadline": current["monotonic_ms"] + 30000})

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

    def gate(self, clock, ledger=None, cutoff=30020):
        return EvidenceGate(self.sequence(ledger=ledger), clock, "clock", cutoff)

    def test_gate_passes_frozen_claim_and_inclusive_limits(self):
        for cutoff, deadline in ((25000, 25000), (40000, 30020)):
            with self.subTest(cutoff=cutoff):
                self.setUp()
                seen = []
                reading = [4000, 20, "clock"]
                def verify(*args):
                    reading[:] = (4001, 21, "clock")
                    return self.verify(*args)
                def ledger(*args):
                    seen.append(copy.deepcopy(args))
                    reading[:] = (32010, deadline, "clock")
                    return self.bounded_claim(reading, *args)
                sequence = self.sequence(ledger=ledger, verifier=verify)
                gate = EvidenceGate(sequence, lambda: tuple(reading), "clock", cutoff)
                self.assertEqual(gate.observe(self.current, {"tick": 20}), self.current)
                self.assertEqual(seen, [(self.anchor, {"tick": 10}, self.current, {"tick": 20},
                                         self.binding, {"clock_domain": "clock",
                                                        "wall_not_before": 4001, "tick_not_before": 21,
                                                        "wall_deadline": 32010, "tick_deadline": deadline})])

    def test_missing_or_invalid_claim_limits_are_terminal(self):
        invalid = (None, lambda _: None, lambda _: {"clock_domain": "clock"},
                   lambda _: {"clock_domain": "clock", "wall_not_before": 4000,
                              "tick_not_before": 20, "wall_deadline": 32011,
                              "tick_deadline": 30020})
        for callback in invalid:
            with self.subTest(callback=callback):
                self.setUp()
                sequence = self.sequence()
                with self.assertRaises(EvidenceError):
                    sequence.accept(self.current, {"tick": 20}, now_ms=4000,
                                    monotonic_ms=20, before_claim=callback)
                self.assertEqual(self.claims, 0)
                with self.assertRaisesRegex(EvidenceError, "terminal"):
                    self.accept(sequence)

    def test_late_ledger_refusal_or_success_is_terminal_without_retry(self):
        for commit_tick in (100, 101):
            with self.subTest(commit_tick=commit_tick):
                self.setUp()
                reading = [4000, 20, "clock"]
                attempts = []
                def ledger(*args):
                    attempts.append(args)
                    reading[1] = commit_tick
                    committed = self.bounded_claim(reading, *args)
                    reading[1] = 101
                    return committed
                gate = self.gate(lambda: tuple(reading), ledger=ledger, cutoff=100)
                with self.assertRaises(EvidenceError):
                    gate.observe(self.current, {"tick": 20})
                with self.assertRaisesRegex(EvidenceError, "terminal"):
                    gate.observe(self.current, {"tick": 20})
                self.assertEqual(len(attempts), 1)
                self.assertEqual(self.state, (self.current, {"tick": 20}) if commit_tick == 100
                                 else (self.anchor, {"tick": 10}))

    def test_gate_refusal_cutoff_boundaries_and_no_completion(self):
        for finish in (30019, 30020, 30021):
            with self.subTest(finish=finish):
                self.setUp()
                readings = iter(((4000, 20, "clock"), (4000, 20, "clock"), (4001, finish, "clock")))
                gate = self.gate(lambda: next(readings))
                if finish <= 30020:
                    self.assertEqual(gate.observe(self.current, {"tick": 20}), self.current)
                    self.assertFalse(hasattr(gate, "complete"))
                else:
                    with self.assertRaises(EvidenceError):
                        gate.observe(self.current, {"tick": 20})
                    with self.assertRaisesRegex(EvidenceError, "terminal"):
                        gate.observe(self.current, {"tick": 20})
                self.assertEqual(self.claims, 1)

    def test_closing_endpoint_with_positive_latency_cannot_complete(self):
        readings = iter(((4000, 20, "clock"), (4000, 20, "clock"), (4001, 21, "clock")))
        gate = self.gate(lambda: next(readings), cutoff=20)
        with self.assertRaises(EvidenceError):
            gate.observe(self.current, {"tick": 20})
        self.assertEqual(self.claims, 1)
        self.assertFalse(hasattr(gate, "complete"))
        with self.assertRaisesRegex(EvidenceError, "terminal"):
            gate.observe(self.current, {"tick": 20})

    def test_gate_refuses_expired_verification_before_claim(self):
        for wall, tick, cutoff in ((4001, 101, 100), (4001, 30021, 40000),
                                   (32011, 21, 40000)):
            with self.subTest(wall=wall, tick=tick, cutoff=cutoff):
                self.setUp()
                readings = iter(((4000, 20, "clock"), (wall, tick, "clock")))
                gate = self.gate(lambda: next(readings), cutoff=cutoff)
                with self.assertRaises(EvidenceError):
                    gate.observe(self.current, {"tick": 20})
                self.assertEqual(self.claims, 0)
                with self.assertRaisesRegex(EvidenceError, "terminal"):
                    gate.observe(self.current, {"tick": 20})

    def test_gate_rechecks_both_freshness_clocks(self):
        for kind in ("wall", "monotonic"):
            for age in (30000, 30001):
                with self.subTest(kind=kind, age=age):
                    self.setUp()
                    wall = self.current["end_ms"] + age if kind == "wall" else 4001
                    tick = 20 + age if kind == "monotonic" else 21
                    readings = iter(((4000, 20, "clock"), (4000, 20, "clock"), (wall, tick, "clock")))
                    gate = self.gate(lambda: next(readings), cutoff=40000)
                    if age == 30000:
                        self.assertEqual(gate.observe(self.current, {"tick": 20}), self.current)
                    else:
                        with self.assertRaises(EvidenceError):
                            gate.observe(self.current, {"tick": 20})
                        with self.assertRaisesRegex(EvidenceError, "terminal"):
                            gate.observe(self.current, {"tick": 20})
                    self.assertEqual(self.claims, 1)

    def test_gate_rejects_regression_and_wrong_domain_after_claim(self):
        for final in ((3999, 20, "clock"), (4001, 19, "clock"),
                      (4001, 20, "other")):
            with self.subTest(final=final):
                self.setUp()
                readings = iter(((4000, 20, "clock"), (4000, 20, "clock"), final))
                gate = self.gate(lambda: next(readings))
                with self.assertRaises(EvidenceError):
                    gate.observe(self.current, {"tick": 20})
                self.assertEqual(self.claims, 1)
                with self.assertRaisesRegex(EvidenceError, "terminal"):
                    gate.observe(self.current, {"tick": 20})

    def test_gate_missing_endpoint_and_bad_clock_fail_closed(self):
        reading = [4000, 30019, "clock"]
        gate = self.gate(lambda: tuple(reading))
        gate.check_deadline()
        reading[1] = 30020
        gate.check_deadline()
        reading[1] = 30021
        with self.assertRaises(EvidenceError):
            gate.check_deadline()
        with self.assertRaisesRegex(EvidenceError, "terminal"):
            gate.check_deadline()
        for clock in (None, lambda: (True, 20, "clock")):
            with self.subTest(clock=clock):
                if clock is None:
                    with self.assertRaises(EvidenceError):
                        self.gate(clock)
                else:
                    gate = self.gate(clock)
                    with self.assertRaises(EvidenceError):
                        gate.check_deadline()
                    self.assertTrue(gate.failed)

    def test_gate_ambiguous_or_invalid_evidence_is_terminal(self):
        for case in ("proof", "ledger", "interrupt"):
            with self.subTest(case=case):
                self.setUp()
                def ledger(*args):
                    self.ledger(*args)
                    if case == "interrupt":
                        raise KeyboardInterrupt()
                    raise ValueError("private")
                gate = self.gate(lambda: (4000, 20, "clock"), ledger=ledger)
                error = KeyboardInterrupt if case == "interrupt" else EvidenceError
                with self.assertRaises(error):
                    gate.observe(self.current, {"tick": 99 if case == "proof" else 20})
                self.assertEqual(self.claims, int(case != "proof"))
                with self.assertRaisesRegex(EvidenceError, "terminal"):
                    gate.observe(self.current, {"tick": 20})

    def test_missing_dependencies_and_non_boolean_ledger(self):
        for proof, ledger, verifier in ((None, self.ledger, self.verify),
                                        ({"tick": 10}, None, self.verify),
                                        ({"tick": 10}, self.ledger, None)):
            with self.subTest(proof=proof, ledger=ledger, verifier=verifier):
                with self.assertRaises(EvidenceError):
                    SampleSequence(self.anchor, proof, self.binding, ledger, verifier)
        with self.assertRaises(EvidenceError):
            self.accept(self.sequence(ledger=lambda *args: 1))


class CollectionDeadlineTest(unittest.TestCase):
    def gate(self, reading, *, sample=100, window=50000, end=60000):
        return CollectionDeadline(clock=lambda: tuple(reading), clock_domain="clock",
                                  last_sample_tick=sample, window_deadline=window,
                                  watch_end=end)

    def test_earliest_frozen_limit_and_inclusive_finish(self):
        for sample, window, end, expected in ((100, 50000, 60000, 15100),
                                               (0, 50000, 60000, 15100),
                                               (100, 10000, 60000, 10000),
                                               (100, 50000, 9000, 9000)):
            with self.subTest(expected=expected):
                reading = [100, "clock"]
                gate = self.gate(reading, sample=sample, window=window, end=end)
                self.assertEqual(gate.deadline, expected)
                reading[0] = expected - 1
                gate.check()
                reading[0] = expected
                gate.finish()
                with self.assertRaisesRegex(EvidenceError, "terminal"):
                    gate.finish()

    def test_sample_limit_and_no_deadline_extension(self):
        reading = [20000, "clock"]
        gate = self.gate(reading, sample=0)
        self.assertEqual(gate.deadline, 30000)
        reading[0] = 29999
        gate.check()
        self.assertEqual(gate.deadline, 30000)
        reading[0] = 30000
        with self.assertRaises(EvidenceError):
            gate.check()
        with self.assertRaisesRegex(EvidenceError, "terminal"):
            gate.finish()

    def test_bad_or_late_clock_is_terminal(self):
        for value in ((99, "clock"), (101, "other"), (True, "clock"),
                      (15101, "clock")):
            with self.subTest(value=value):
                reading = [100, "clock"]
                gate = self.gate(reading)
                reading[:] = value
                with self.assertRaises(EvidenceError):
                    gate.finish()
                reading[:] = (100, "clock")
                with self.assertRaisesRegex(EvidenceError, "terminal"):
                    gate.check()

    def test_invalid_initial_limits_and_due_time(self):
        for sample, window, end in ((True, 50000, 60000), (101, 50000, 60000),
                                    (100, 100, 60000), (100, 50000, MAX_COUNTER + 1)):
            with self.subTest(sample=sample, window=window, end=end):
                with self.assertRaises(EvidenceError):
                    self.gate([100, "clock"], sample=sample, window=window, end=end)

    def test_interrupted_clock_is_terminal(self):
        reading = [100, "clock"]
        gate = self.gate(reading)
        def interrupted():
            raise KeyboardInterrupt()
        gate._clock = interrupted
        with self.assertRaises(KeyboardInterrupt):
            gate.check()
        with self.assertRaisesRegex(EvidenceError, "terminal"):
            gate.finish()
