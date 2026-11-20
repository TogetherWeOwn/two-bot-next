"""Inactive single-collection deadline bookkeeping, not evidence or watch success.

The owner serializes calls, supplies trusted frozen limits and enforces the
returned cutoff throughout collection, including any child process and retries.
This helper cannot interrupt a hung collector or authenticate its result.
"""

from rest_evidence_continuity import EvidenceError, MAX_COUNTER


class CollectionDeadline:
    """Freeze one collection's earliest deadline in a trusted clock domain."""

    def __init__(self, *, clock, clock_domain, last_sample_tick, window_deadline, watch_end):
        if (not callable(clock) or type(clock_domain) is not str or not clock_domain
                or any(type(value) is not int or not 0 <= value <= MAX_COUNTER
                       for value in (last_sample_tick, window_deadline, watch_end))):
            raise EvidenceError("invalid collection dependencies")
        self._clock = clock
        self._domain = clock_domain
        self._last_tick = last_sample_tick
        self._state = "pending"
        try:
            tick = self._time()
            self._deadline = min(tick + 15000, last_sample_tick + 30000,
                                 window_deadline, watch_end, MAX_COUNTER)
            if tick >= self._deadline:
                raise EvidenceError("collection already due")
        except Exception:
            self._state = "failed"
            raise EvidenceError("invalid collection deadline") from None

    @property
    def deadline(self):
        return self._deadline

    def _time(self):
        tick, domain = self._clock()
        if (type(tick) is not int or not self._last_tick <= tick <= MAX_COUNTER
                or domain != self._domain):
            raise EvidenceError("invalid collection clock")
        self._last_tick = tick
        return tick

    def _check(self, *, finishing):
        if self._state != "pending":
            raise EvidenceError("collection deadline is terminal")
        try:
            tick = self._time()
            if tick > self.deadline or (tick == self.deadline and not finishing):
                raise EvidenceError("collection deadline missed")
            if finishing:
                self._state = "finished"
        except Exception:
            self._state = "failed"
            raise EvidenceError("invalid or late collection") from None
        except BaseException:
            self._state = "failed"
            raise

    def check(self):
        """Refuse a collection still pending at its frozen deadline."""
        self._check(finishing=False)

    def finish(self):
        """Close collection on time; this does not validate its endpoint."""
        self._check(finishing=True)
