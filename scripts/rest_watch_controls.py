"""Inactive control-observation guard; success is not a watch admission.

The owner supplies authenticated observations, a trusted monotonic clock and
serialized calls. This module neither collects evidence nor performs recovery.
"""

import copy

from rest_evidence_continuity import EvidenceError, MAX_COUNTER


class ControlGuard:
    """Enforce control cadence and fail permanently on unsafe observations.

    The verifier must authenticate the entire observation against the frozen
    handoff and observation tick. The clock returns (tick, clock_domain) from the
    same trusted monotonic domain as the handoff. The caller must separately
    enforce sampling, fixed windows, watch duration and restoration fencing.
    """

    def __init__(self, record, transition_tick, clock_domain, clock, verify):
        if (type(record) is not dict or type(transition_tick) is not int
                or not 0 <= transition_tick <= MAX_COUNTER
                or type(clock_domain) is not str or not clock_domain
                or not callable(clock) or not callable(verify)):
            raise EvidenceError("invalid control dependencies")
        self._record = copy.deepcopy(record)
        self._domain = clock_domain
        self._clock = clock
        self._verify = verify
        self._last_tick = transition_tick
        self._last_clock = transition_tick
        self._misses = 0
        self.failed = False

    def observe(self, controls, *, deadline):
        """Authenticate one observation before its absolute deadline.

        The deadline is the caller's earliest sample, window, collection or
        watch deadline; verifier latency counts against it and the next control
        interval. A successful return only means this observation was accepted.
        """
        if self.failed:
            raise EvidenceError("control guard is terminal")
        try:
            tick, domain = self._clock()
            if (type(tick) is not int or type(deadline) is not int
                    or not self._last_tick < tick <= MAX_COUNTER
                    or tick < self._last_clock
                    or not 0 <= deadline <= MAX_COUNTER or tick > deadline
                    or tick - self._last_tick > 10000 or domain != self._domain
                    or type(controls) is not dict
                    or set(controls) != {"readiness", "journeys", "ownership"}
                    or any(type(value) is not bool for value in controls.values())):
                raise EvidenceError("invalid control observation")
            authenticated = self._verify(copy.deepcopy(controls), copy.deepcopy(self._record), tick)
            finished, final_domain = self._clock()
            if (authenticated is not True or type(finished) is not int
                    or not tick <= finished <= min(deadline, tick + 10000, MAX_COUNTER)
                    or final_domain != self._domain):
                raise EvidenceError("unverified or late control observation")
            if not controls["ownership"] or not controls["journeys"]:
                raise EvidenceError("control failure")
            self._misses = 0 if controls["readiness"] else self._misses + 1
            if self._misses >= 18:
                raise EvidenceError("readiness failure")
            self._last_tick = tick
            self._last_clock = finished
            return controls["readiness"]
        except Exception:
            self.failed = True
            raise EvidenceError("invalid or unverified controls") from None
        except BaseException:
            self.failed = True
            raise
