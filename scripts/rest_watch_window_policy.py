"""Inactive evidence handling with a fixed refusal cutoff, not a watch policy.

The caller serializes calls and supplies the claimed sequence, trusted clock and
fixed cutoff. Authentication, atomic durable claims and bounded callbacks remain
adapter obligations. A result is not window coverage or watch completion.
"""

from rest_evidence_continuity import EvidenceError, MAX_COUNTER


class EvidenceGate:
    def __init__(self, sequence, clock, clock_domain, cutoff):
        if (not callable(clock) or type(clock_domain) is not str or not clock_domain
                or type(cutoff) is not int or not 0 <= cutoff <= MAX_COUNTER):
            raise EvidenceError("invalid evidence gate dependencies")
        self._sequence = sequence
        self._clock = clock
        self._domain = clock_domain
        self._cutoff = cutoff
        self._last_clock = None
        self.failed = False

    def _time(self):
        wall, tick, domain = self._clock()
        if (type(wall) is not int or type(tick) is not int
                or not 0 <= wall <= MAX_COUNTER or not 0 <= tick <= MAX_COUNTER
                or type(domain) is not str or domain != self._domain
                or (self._last_clock is not None
                    and (wall < self._last_clock[0] or tick < self._last_clock[1]))
                or tick > self._cutoff):
            raise EvidenceError("invalid or late evidence clock")
        self._last_clock = wall, tick
        return wall, tick

    def check_deadline(self):
        """Refuse a missed cutoff even when no endpoint arrives."""
        if self.failed:
            raise EvidenceError("evidence gate is terminal")
        try:
            self._time()
        except Exception:
            self.failed = True
            raise EvidenceError("invalid or late evidence clock") from None
        except BaseException:
            self.failed = True
            raise

    def _fresh(self, sample):
        wall, tick = self._time()
        if (wall - sample["end_ms"] > 30000
                or tick - sample["monotonic_ms"] > 30000):
            raise EvidenceError("stale evidence")

    def observe(self, sample, proof):
        """Return claimed continuity evidence, never window or watch completion.

        Post-return checks cannot interrupt a hung callback or enforce an atomic
        transaction cutoff. A late or ambiguous claim requires authoritative
        reconciliation of the same operation, not retry or ownership restoration.
        """
        if self.failed:
            raise EvidenceError("evidence gate is terminal")
        try:
            wall, tick = self._time()
            accepted = self._sequence.accept(sample, proof, now_ms=wall, monotonic_ms=tick,
                                             before_claim=self._fresh)
            self._fresh(accepted)
            return accepted
        except Exception:
            self.failed = True
            raise EvidenceError("invalid or late evidence") from None
        except BaseException:
            self.failed = True
            raise
