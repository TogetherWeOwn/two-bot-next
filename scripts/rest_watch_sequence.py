"""Inactive authenticated sample sequencing, not a watch admission or receipt.

The caller must authenticate and atomically claim the initial handoff. The
injected verifier authenticates complete samples in their original proof context;
the injected ledger must durably compare-and-swap the complete run-bound state
across restarts. No production verifier or ledger is provided here. Calls on one
sequence must be serialized by its owner, with trusted clocks and binding.
"""

import copy

from rest_evidence_continuity import EvidenceError, validate_continuity


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

    def accept(self, sample, proof, *, now_ms, monotonic_ms):
        """Verify both bookends, then claim the endpoint before returning it.

        The ledger receives previous sample/proof, current sample/proof and
        expected binding, and must return exactly True only after an atomic
        durable compare-and-swap. Any error, including an ambiguous ledger
        outcome, permanently terminates this object; recovery is caller-owned.
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
            if self._ledger(copy.deepcopy(previous), copy.deepcopy(old_proof),
                            copy.deepcopy(current), copy.deepcopy(new_proof),
                            copy.deepcopy(self._expected)) is not True:
                raise EvidenceError("replay ledger rejected endpoint")
            self._previous, self._proof = current, new_proof
            return copy.deepcopy(current)
        except Exception:
            self._failed = True
            raise EvidenceError("invalid or unverified sample sequence") from None
