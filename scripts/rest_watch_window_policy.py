"""Inactive composition of claimed sample continuity and fixed windows.

The caller supplies the matching objects returned by claim_handoff and serializes
calls. Authentication, durable ledger atomicity, clock trust and bounded callbacks
remain adapter obligations. Nothing here starts a watch or authorizes recovery.
"""

from rest_evidence_continuity import EvidenceError, MAX_COUNTER


class WindowPolicy:
    """Feed authenticated endpoints into a fixed schedule, failing permanently.

    Completion means only that the window schedule reached its original end;
    controls and all other watch and release gates remain independent.
    """

    def __init__(self, sequence, schedule, clock, clock_domain):
        if not callable(clock) or type(clock_domain) is not str or not clock_domain:
            raise EvidenceError("invalid window policy dependencies")
        self._sequence = sequence
        self._schedule = schedule
        self._clock = clock
        self._domain = clock_domain
        self._last_clock = None
        self.failed = False
        self.complete = False

    def _time(self):
        wall, tick, domain = self._clock()
        if (type(wall) is not int or type(tick) is not int
                or not 0 <= wall <= MAX_COUNTER or not 0 <= tick <= MAX_COUNTER
                or domain != self._domain
                or (self._last_clock is not None
                    and (wall < self._last_clock[0] or tick < self._last_clock[1]))):
            raise EvidenceError("invalid window clock")
        self._last_clock = wall, tick
        return wall, tick

    def check_deadline(self):
        """Fail on a missed deadline when the caller supplies no endpoint."""
        if self.failed or self.complete:
            raise EvidenceError("window policy is terminal")
        try:
            _, tick = self._time()
            self._schedule.check_deadline(tick)
        except Exception:
            self.failed = True
            raise EvidenceError("invalid or late window clock") from None
        except BaseException:
            self.failed = True
            raise

    def observe(self, sample, proof):
        """Accept one endpoint without extending its original deadline.

        Clock checks include verifier and ledger latency after they return; they
        cannot interrupt a hung callback. A late or ambiguous ledger result may
        already be committed and requires authoritative reconciliation, not retry.
        """
        if self.failed or self.complete:
            raise EvidenceError("window policy is terminal")
        try:
            wall, tick = self._time()
            self._schedule.check_deadline(tick)
            accepted = self._sequence.accept(sample, proof, now_ms=wall, monotonic_ms=tick)
            _, finished = self._time()
            self._schedule.check_deadline(finished)
            result = self._schedule.observe(accepted["series"], accepted["monotonic_ms"])
            self.complete = result == "complete"
            return result
        except Exception:
            self.failed = True
            raise EvidenceError("invalid or late window endpoint") from None
        except BaseException:
            self.failed = True
            raise
