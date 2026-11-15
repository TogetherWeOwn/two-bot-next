"""Inactive arithmetic for one REST watch window, not an admission decision.

The caller must authenticate and validate both complete samples, their ordering,
replay state and fixed deadlines before using this calculation. No runtime caller
is wired to this helper. A qualified window is not a completed watch.
"""

import copy

from rest_evidence_continuity import EvidenceError, MAX_COUNTER, MAX_SERIES, ROUTE

RESULTS = frozenset(("2xx", "3xx", "4xx", "429", "5xx", "transport"))


class WindowFailure(EvidenceError):
    def __init__(self, reason, assessment=None):
        super().__init__(reason)
        self.reason = reason
        self.assessment = copy.deepcopy(assessment)


def _rows(rows):
    if type(rows) is not list or not 0 < len(rows) <= MAX_SERIES:
        raise EvidenceError("invalid window series")
    result = {}
    for row in rows:
        if type(row) is not dict or set(row) != {"route", "result", "count"}:
            raise EvidenceError("invalid window series")
        route, status, count = row["route"], row["result"], row["count"]
        if (type(route) is not str or ROUTE.fullmatch(route) is None
                or type(status) is not str or status not in RESULTS
                or type(count) is not int or not 0 <= count <= MAX_COUNTER
                or (route, status) in result):
            raise EvidenceError("invalid window series")
        result[route, status] = count
    return result


def _ratio(value):
    if (type(value) is not tuple or len(value) != 2
            or any(type(x) is not int or not 0 <= x <= MAX_COUNTER for x in value)
            or value[1] < 10 or value[0] > value[1]):
        raise EvidenceError("invalid frozen baseline")
    return value


def _regression(errors, total, baseline):
    # Integer cross-products avoid rounding at the strict 5% and 1.5x bounds.
    prior_errors, prior_total = baseline
    absolute = errors * 20 > total
    relative = errors * 2 * prior_total > 3 * prior_errors * total
    return (total >= 10 and absolute) or (errors >= 3 and absolute and relative)


def assess_window(opening, closing, required_routes, baseline):
    """Return counts and a provisional window assessment, never watch PASS.

    Baseline maps ``global`` and each required route to (5xx, total) integer
    pairs from the frozen prior-production window. All endpoints must already be
    authenticated; this function cannot establish trust or sample freshness.
    """
    if (type(required_routes) is not list or not required_routes
            or len(required_routes) > MAX_SERIES
            or any(type(route) is not str or ROUTE.fullmatch(route) is None
                   or route == "global" for route in required_routes)
            or len(set(required_routes)) != len(required_routes)):
        raise EvidenceError("invalid route manifest")
    if type(baseline) is not dict or set(baseline) != {"global", *required_routes}:
        raise EvidenceError("invalid frozen baseline")
    baseline = {route: _ratio(value) for route, value in baseline.items()}
    start, end = _rows(opening), _rows(closing)
    if (not start.keys() <= end.keys()
            or any(end[key] < value for key, value in start.items())
            or any(end[key] != 0 for key in end.keys() - start.keys())):
        raise EvidenceError("discontinuous or unanchored window series")
    totals = {route: [0, 0] for route in ("global", *required_routes)}
    for (route, status), count in end.items():
        delta = count - start.get((route, status), 0)
        if status == "transport":
            continue
        targets = ("global", route) if route in baseline and route != "global" else ("global",)
        for target in targets:
            totals[target][1] += delta
            if status == "5xx":
                totals[target][0] += delta
    regression = [route for route, (errors, total) in totals.items()
                  if _regression(errors, total, baseline[route])]
    thin = [route for route, (_, total) in totals.items() if total < 10]
    return {"assessment": "regression" if regression else "thin" if thin else "qualified",
            "counts": {key: {"errors": value[0], "total": value[1]}
                       for key, value in totals.items()},
            "regressions": regression, "thin": thin}


class WindowSchedule:
    """Judge fixed windows over already authenticated, continuous endpoints.

    This is only arithmetic and deadline bookkeeping. The caller must enforce
    the handoff, replay, identity, readiness, journeys and ownership separately.
    Completing this schedule is not a watch or deployment pass.
    """

    def __init__(self, anchor, anchor_tick, transition_tick, required_routes, baseline):
        if (type(anchor_tick) is not int or type(transition_tick) is not int
                or not 0 <= anchor_tick <= transition_tick <= MAX_COUNTER - 1800000
                or transition_tick - anchor_tick > 30000):
            raise EvidenceError("invalid watch transition")
        assess_window(anchor, anchor, required_routes, baseline)
        self.routes = copy.deepcopy(required_routes)
        self.baseline = copy.deepcopy(baseline)
        self.opening = copy.deepcopy(anchor)
        self.previous = copy.deepcopy(anchor)
        self.last_tick = anchor_tick
        self.end = transition_tick + 1800000
        self.deadline = min(anchor_tick + 240000, self.end)
        self.windows = []
        self.failed = False
        self.complete = False

    def observe(self, rows, tick):
        """Record one fresh endpoint, or fail without extending a deadline."""
        if self.failed or self.complete:
            raise EvidenceError("window schedule is terminal")
        try:
            if (type(tick) is not int or not self.last_tick < tick <= self.deadline
                    or tick - self.last_tick > 30000):
                raise WindowFailure("deadline_or_coverage")
            # Intermediate endpoints cannot hide a per-series reset.
            assess_window(self.previous, rows, self.routes, self.baseline)
            opening = copy.deepcopy(self.opening)
            opening_keys = _rows(opening)
            opening.extend(copy.deepcopy(row) for row in rows
                           if (row["route"], row["result"]) not in opening_keys)
            result = assess_window(opening, rows, self.routes, self.baseline)
            if result["assessment"] == "regression":
                raise WindowFailure("regression", result)
            self.opening = opening
            if tick == self.deadline:
                if result["assessment"] != "qualified":
                    raise WindowFailure("deadline_or_coverage", result)
                self.windows.append({"end_tick": tick, "counts": result["counts"]})
                self.opening = copy.deepcopy(rows)
                self.complete = tick == self.end
                self.deadline = min(tick + 240000, self.end)
            self.previous = copy.deepcopy(rows)
            self.last_tick = tick
            return "complete" if self.complete else "pending"
        except (EvidenceError, TypeError, KeyError):
            self.failed = True
            raise

    def check_deadline(self, tick):
        """Fail once an endpoint or sampling deadline has been missed."""
        if self.failed or self.complete:
            raise EvidenceError("window schedule is terminal")
        if (type(tick) is not int or tick < self.last_tick
                or tick > self.deadline or tick > self.last_tick + 30000):
            self.failed = True
            raise WindowFailure("deadline_or_coverage")
