"""Inactive authenticated sample sequencing, not a watch admission or receipt.

The caller must authenticate and atomically claim the initial handoff. The
injected verifier authenticates complete samples in their original proof context;
the injected ledger must durably compare-and-swap the complete run-bound state
across restarts. No production verifier or ledger is provided here. Calls on one
sequence must be serialized by its owner, with trusted clocks and binding.
"""

import copy

from rest_evidence_continuity import EvidenceError, MAX_COUNTER, validate_continuity


class SampleSequence:
    def __init__(self, anchor, proof, expected, ledger, verifier):
        if proof is None or not callable(verifier) or not callable(ledger):
            raise EvidenceError("missing sequence dependency")
        self._previous = copy.deepcopy(anchor)
        self._proof = copy.deepcopy(proof)
        self._expected = copy.deepcopy(expected)
        self._ledger = ledger
        self._verifier = verifier
        self._failed = False

    def accept(self, sample, proof, *, now_ms, monotonic_ms, before_claim=None):
        """Verify both bookends, then claim the endpoint before returning it.

        The required trusted before_claim callback reads the clock after
        verification and returns limits capped by the active cutoff and sample
        freshness. The ledger receives both bookends and proofs, the expected
        binding, and those limits. In the same atomic commit as sequence and
        replay consumption, it must verify the clock domain and enforce the
        inclusive wall and tick ranges, returning exactly True only after that
        commit. Failure is terminal; reconcile a possible commit by its original
        operation, without retry or ownership restoration.
        """
        if self._failed:
            raise EvidenceError("sample sequence is terminal")
        try:
            if proof is None or type(sample) is not dict or type(self._previous) is not dict:
                raise EvidenceError("missing sample or proof")
            old_tick = self._previous.get("monotonic_ms")
            new_tick = sample.get("monotonic_ms")
            if (type(old_tick) is not int or type(new_tick) is not int
                    or not 0 < new_tick - old_tick <= 30000):
                raise EvidenceError("invalid sample interval")
            previous, current = copy.deepcopy(self._previous), copy.deepcopy(sample)
            old_proof, new_proof = copy.deepcopy(self._proof), copy.deepcopy(proof)

            def verify(item, expected):
                context = old_proof if item is previous else new_proof
                return self._verifier(copy.deepcopy(item), copy.deepcopy(context),
                                      copy.deepcopy(expected))

            validate_continuity([previous, current], self._expected, now_ms=now_ms,
                                monotonic_ms=monotonic_ms, verify_sample=verify)
            if not callable(before_claim):
                raise EvidenceError("missing claim limits")
            claim_args = copy.deepcopy((previous, old_proof, current, new_proof, self._expected))
            limits = before_claim(copy.deepcopy(current))
            keys = {"clock_domain", "wall_not_before", "tick_not_before",
                    "wall_deadline", "tick_deadline"}
            if (type(limits) is not dict or set(limits) != keys
                    or type(limits["clock_domain"]) is not str or not limits["clock_domain"]
                    or any(type(limits[k]) is not int or not 0 <= limits[k] <= MAX_COUNTER
                           for k in keys - {"clock_domain"})
                    or not now_ms <= limits["wall_not_before"] <= limits["wall_deadline"]
                    or not monotonic_ms <= limits["tick_not_before"] <= limits["tick_deadline"]
                    or limits["wall_deadline"] > current["end_ms"] + 30000
                    or limits["tick_deadline"] > current["monotonic_ms"] + 30000):
                raise EvidenceError("invalid claim limits")
            if self._ledger(*claim_args, limits.copy()) is not True:
                raise EvidenceError("replay ledger rejected endpoint")
            self._previous, self._proof = current, new_proof
            return copy.deepcopy(current)
        except Exception:
            self._failed = True
            raise EvidenceError("invalid or unverified sample sequence") from None
        except BaseException:
            self._failed = True
            raise
