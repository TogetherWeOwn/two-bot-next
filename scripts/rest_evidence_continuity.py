"""Inactive structural continuity check for candidate-bound REST samples.

This is not a scrape, authentication implementation, or promotion decision.
No production verifier currently supplies the required same-sample identity proof.
A future caller must authenticate the complete sample using a reviewed verifier,
provide trusted expected bindings and a replay-resistant, run-scoped challenge,
and enforce all other staging, initial-gate and watch requirements separately.
A successful result means only that the supplied series are continuous.
"""

import json
import re

MAX_SAMPLES = 128
MAX_SERIES = 512
MAX_COUNTER = (1 << 53) - 1
HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX32 = re.compile(r"[0-9a-f]{32}\Z")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
IDENTIFIER = re.compile(r"[A-Za-z0-9_.:-]{1,128}\Z")
ROUTE = re.compile(r"[a-zA-Z0-9_./:{}@ -]{1,128}\Z")


class EvidenceError(ValueError):
    """Evidence is absent, invalid, unverified, stale, or discontinuous."""


def _integer(value, lower=0, upper=MAX_COUNTER):
    return type(value) is int and lower <= value <= upper


def _require(condition):
    if not condition:
        raise EvidenceError("invalid or unverified REST evidence")


def parse_samples(raw):
    """Decode bounded UTF-8 JSON without allowing duplicate object fields."""
    _require(type(raw) is bytes and len(raw) <= 65536)

    def unique(pairs):
        result = {}
        for key, value in pairs:
            _require(key not in result)
            result[key] = value
        return result

    try:
        return json.loads(raw.decode("utf-8"), object_pairs_hook=unique)
    except (UnicodeError, ValueError, RecursionError):
        raise EvidenceError("invalid or unverified REST evidence") from None


def _identity(binding):
    keys = {"environment", "sha", "build", "deployment", "worker", "image", "run", "attempt", "challenge"}
    _require(type(binding) is dict and set(binding) == keys)
    _require(binding["environment"] in ("staging", "production"))
    _require(type(binding["sha"]) is str and HEX40.fullmatch(binding["sha"]) is not None)
    _require(type(binding["image"]) is str and DIGEST.fullmatch(binding["image"]) is not None)
    for key in ("build", "deployment", "worker", "challenge"):
        _require(type(binding[key]) is str and IDENTIFIER.fullmatch(binding[key]) is not None)
    for key in ("run", "attempt"):
        _require(_integer(binding[key], 1))


def validate_continuity(samples, expected, *, now_ms, monotonic_ms, verify_sample=None):
    """Return final bookend for handoff, never an admission or sparse verdict.

    verify_sample(sample, expected) must establish authentication, freshness and
    same-response process/build identity over *all* fields. It is deliberately
    absent by default; a label, boolean in the receipt, or independent readiness
    response is not such proof. The caller owns challenge uniqueness, persistent
    replay prevention, trusted clocks, and atomic handoff to the watch controller.
    For handoff retain the complete authenticated final input sample and its proof,
    the expected binding and challenge, plus replay state; the returned summary
    alone cannot be used as the next bookend or prove authentication. The watch
    must reverify that same sample before comparing its first new sample. No
    caller may treat this summary as a pass, a deferred verdict, or a receipt of
    the other policy checks.
    Times are integer milliseconds; collection duration is limited to 15 seconds
    and the final observation to 30 seconds at the caller's trusted judgment time.
    """
    _identity(expected)
    _require(callable(verify_sample))
    _require(type(samples) is list and 2 <= len(samples) <= MAX_SAMPLES)
    _require(_integer(now_ms) and _integer(monotonic_ms))
    previous = None
    prior_end = -1
    prior_tick = -1
    incarnation = None
    for sample in samples:
        _require(type(sample) is dict and set(sample) == {
            "binding", "incarnation", "start_ms", "end_ms", "monotonic_ms", "series"})
        _identity(sample["binding"])
        _require(sample["binding"] == expected)
        token = sample["incarnation"]
        _require(type(token) is str and HEX32.fullmatch(token) is not None and token != "0" * 32)
        if incarnation is None:
            incarnation = token
        _require(token == incarnation)
        start, end, tick = sample["start_ms"], sample["end_ms"], sample["monotonic_ms"]
        _require(all(_integer(t) for t in (start, end, tick)))
        _require(prior_end < start <= end <= now_ms and end - start <= 15000)
        _require(prior_tick < tick <= monotonic_ms)
        _require(verify_sample(sample, expected) is True)
        rows = sample["series"]
        _require(type(rows) is list and 0 < len(rows) <= MAX_SERIES)
        current = {}
        for row in rows:
            _require(type(row) is dict and set(row) == {"route", "result", "count"})
            route, result, count = row["route"], row["result"], row["count"]
            _require(type(route) is str and ROUTE.fullmatch(route) is not None)
            _require(type(result) is str and result in ("2xx", "3xx", "4xx", "429", "5xx", "transport"))
            _require(_integer(count))
            key = (route, result)
            _require(key not in current)
            current[key] = count
        if previous is not None:
            _require(previous.keys() <= current.keys())
            _require(all(current[key] >= count for key, count in previous.items()))
        previous, prior_end, prior_tick = current, end, tick
    _require(now_ms - prior_end <= 30000 and monotonic_ms - prior_tick <= 30000)
    return {"kind": "continuity_only", "binding": expected.copy(), "incarnation": incarnation,
            "end_ms": prior_end, "monotonic_ms": prior_tick,
            "series": [{"route": route, "result": result, "count": count}
                       for (route, result), count in sorted(previous.items())]}
