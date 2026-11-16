"""Inactive handoff validation; no production verifier or claim store is supplied.

A successful claim only creates authenticated sequencing and window bookkeeping;
readiness, journeys, ownership, scheduling and failure receipts remain the caller's
responsibility. It is never a watch or deployment pass.
"""

import copy
import math

from rest_evidence_continuity import DIGEST, EvidenceError, IDENTIFIER, MAX_COUNTER, MAX_SERIES, _identity, parse_samples, validate_continuity
from rest_watch_window import RESULTS, WindowSchedule, assess_window
from rest_watch_sequence import SampleSequence


def _finite(value):
    if type(value) is float:
        return math.isfinite(value)
    if type(value) is dict:
        return all(_finite(item) for item in value.values())
    if type(value) is list:
        return all(_finite(item) for item in value)
    return True


def _id(value):
    return type(value) is str and IDENTIFIER.fullmatch(value) is not None


def claim_recovery(raw, *, expected, clock, verify_recovery, claim):
    """Validate an inactive recovery responsibility claim, not a recovery action.

    The trusted expected binding is supplied independently of the envelope. The
    verifier authenticates the complete envelope, its frozen timing provenance,
    authoritative readback, effective controller and external fences, and the
    reconciliation checkpoint. Evidence identifiers are references, not proof.
    The claim adapter must revalidate all of those at one atomic CAS commit,
    including authorization, generation, replay consumption and both inclusive
    deadlines, then persist the new responsibility and immutable result together.
    It returns exactly True only for a known committed claim. No adapter is supplied.

    A failure after invoking the adapter may hide a committed claim. Reconcile by
    operation and generation; never infer retained ownership or retry blindly. This
    API does not reopen a watch or establish recovery, external fencing or health.
    """
    try:
        if any(not callable(item) for item in (clock, verify_recovery, claim)):
            raise EvidenceError("missing adapter")
        binding_keys = {"contract", "handoff_id", "handoff_digest", "environment",
                        "candidate", "prior", "run", "attempt", "challenge",
                        "snapshot_digest", "operation_id", "claimant", "incarnation",
                        "owner", "generation", "clock_domain"}
        if (type(expected) is not dict or set(expected) != binding_keys
                or any(not _id(expected[key]) for key in binding_keys - {
                    "run", "attempt", "generation", "candidate", "prior", "handoff_digest", "snapshot_digest"})
                or any(type(expected[key]) is not int or not 1 <= expected[key] < MAX_COUNTER
                       for key in ("run", "attempt", "generation"))
                or any(type(expected[key]) is not str or DIGEST.fullmatch(expected[key]) is None
                       for key in ("handoff_digest", "snapshot_digest"))):
            raise EvidenceError("invalid expected binding")
        _identity(expected["candidate"])
        _identity(expected["prior"])
        if (expected["candidate"] == expected["prior"]
                or any(expected["candidate"][key] != expected[key]
                       for key in ("environment", "run", "attempt", "challenge"))):
            raise EvidenceError("inconsistent identity")
        expected = copy.deepcopy(expected)
        previous = None

        def read_clock():
            nonlocal previous
            wall, tick, domain = clock()
            if (type(wall) is not int or type(tick) is not int
                    or not 0 <= wall <= MAX_COUNTER or not 0 <= tick <= MAX_COUNTER
                    or domain != expected["clock_domain"]
                    or (previous is not None and (wall < previous[0] or tick < previous[1]))):
                raise EvidenceError("invalid clock")
            previous = wall, tick
            return previous

        wall, tick = read_clock()
        record = parse_samples(raw)
        if (type(record) is not dict or set(record) != {"kind", "version", "binding", "timing", "evidence"}
                or record["kind"] != "sparse_rest_watch_recovery_claim"
                or type(record["version"]) is not int or record["version"] != 1
                or type(record["binding"]) is not dict or record["binding"] != expected
                or any(type(record["binding"].get(key)) is not int for key in ("run", "attempt", "generation"))):
            raise EvidenceError("invalid recovery envelope")
        _identity(record["binding"]["candidate"])
        _identity(record["binding"]["prior"])
        timing = record["timing"]
        if (type(timing) is not dict or set(timing) != {"policy", "origin", "cutoffs"}
                or not _id(timing["policy"]) or not _id(timing["origin"])
                or type(timing["cutoffs"]) is not list or not 1 <= len(timing["cutoffs"]) <= 16):
            raise EvidenceError("missing frozen timing")
        for cutoff in timing["cutoffs"]:
            if (type(cutoff) is not dict or set(cutoff) != {"wall_ms", "tick_ms"}
                    or any(type(cutoff[key]) is not int or not 0 <= cutoff[key] <= MAX_COUNTER
                           for key in ("wall_ms", "tick_ms"))):
                raise EvidenceError("invalid cutoff")
        wall_deadline = min(item["wall_ms"] for item in timing["cutoffs"])
        tick_deadline = min(item["tick_ms"] for item in timing["cutoffs"])
        evidence = record["evidence"]
        if (type(evidence) is not dict
                or set(evidence) != {"readback", "controller_fence", "external_fence", "checkpoint"}
                or any(not _id(value) for value in evidence.values())
                or wall > wall_deadline or tick > tick_deadline):
            raise EvidenceError("missing or expired prerequisites")
        if verify_recovery(copy.deepcopy(record), copy.deepcopy(expected)) is not True:
            raise EvidenceError("recovery authentication failed")
        wall, tick = read_clock()
        if wall > wall_deadline or tick > tick_deadline:
            raise EvidenceError("recovery claim expired")
        limits = {"clock_domain": expected["clock_domain"], "wall_not_before": wall,
                  "tick_not_before": tick, "wall_deadline": wall_deadline,
                  "tick_deadline": tick_deadline}
        claimed = claim(copy.deepcopy(record), copy.deepcopy(limits))
        wall, tick = read_clock()
        if claimed is not True or wall > wall_deadline or tick > tick_deadline:
            raise EvidenceError("recovery claim failed or expired")
        return True
    except Exception:
        raise EvidenceError("invalid or unverified recovery claim") from None


def claim_handoff(raw, *, clock_domain, clock, verify_handoff, claim, ledger, verify_sample):
    """Verify a bounded record and atomically claim it once, or fail closed.

    ``verify_handoff(record)`` must authenticate the entire record and establish
    provenance, initial journeys, baseline admission, snapshot revalidation,
    ownership fencing, inventory completeness and trusted clock/timing. The
    ``clock()`` supplies trusted (wall_ms, monotonic_ms, domain) readings.
    ``claim(record, limits)`` must atomically check that same clock domain and
    both inclusive deadlines, and persist exclusive transfer and replay state
    together. It must reject clock regression from limits' lower bounds and
    return exactly True only on a known committed claim. These transaction-boundary
    requirements are adapter obligations: local readings cannot prove atomicity.
    A late return, exception or ambiguous claim requires integrator-owned recovery;
    failure does not prove that ownership stayed with the caller. Sample proofs
    are passed to ``verify_sample(sample, proof, expected)`` in their original
    contexts. None of these callbacks is a production implementation.
    """
    try:
        if not callable(clock) or not _id(clock_domain):
            raise EvidenceError("invalid clock")
        previous = None

        def read_clock():
            nonlocal previous
            wall, tick, domain = clock()
            if (type(wall) is not int or type(tick) is not int
                    or not 0 <= wall <= MAX_COUNTER or not 0 <= tick <= MAX_COUNTER
                    or domain != clock_domain
                    or (previous is not None and (wall < previous[0] or tick < previous[1]))):
                raise EvidenceError("invalid clock reading")
            previous = (wall, tick)
            return wall, tick

        now_ms, monotonic_ms = read_clock()
        record = parse_samples(raw)
        keys = {"kind", "version", "contract", "handoff_id", "clock_domain",
                "mutation_tick", "transition_tick", "snapshot", "candidate_expected",
                "initial", "prior", "routes", "baseline", "evidence"}
        if (type(record) is not dict or not _finite(record) or set(record) != keys
                or record["kind"] != "sparse_rest_watch_handoff" or type(record["version"]) is not int
                or record["version"] != 1 or not all(_id(record[k]) for k in
                    ("contract", "handoff_id", "clock_domain", "snapshot"))
                or not _id(clock_domain) or record["clock_domain"] != clock_domain
                or any(not callable(x) for x in (verify_handoff, claim, ledger, verify_sample))
                or type(now_ms) is not int or not 0 <= now_ms <= MAX_COUNTER
                or type(monotonic_ms) is not int or not 0 <= monotonic_ms <= MAX_COUNTER):
            raise EvidenceError("invalid handoff envelope")
        mutation, transition = record["mutation_tick"], record["transition_tick"]
        if (type(mutation) is not int or type(transition) is not int
                or not 0 <= mutation <= transition <= monotonic_ms
                or transition - mutation > 240000):
            raise EvidenceError("invalid handoff timing")
        expected = record["candidate_expected"]
        _identity(expected)
        if expected["environment"] != "production":
            raise EvidenceError("invalid candidate binding")
        prior, initial = record["prior"], record["initial"]
        for block in (prior, initial):
            if (type(block) is not dict or set(block) != {"expected", "samples", "proofs"}
                    or type(block["samples"]) is not list or len(block["samples"]) != 2
                    or type(block["proofs"]) is not list or len(block["proofs"]) != 2
                    or any(proof is None for proof in block["proofs"])):
                raise EvidenceError("missing bookend or proof")
        if initial["expected"] != expected or prior["expected"] == expected:
            raise EvidenceError("invalid prior or candidate binding")
        for block in (prior, initial):
            first, last = block["samples"]
            def verify(item, binding):
                proof = block["proofs"][0] if item is first else block["proofs"][1]
                return verify_sample(copy.deepcopy(item), copy.deepcopy(proof), copy.deepcopy(binding))
            validate_continuity(block["samples"], block["expected"], now_ms=last["end_ms"],
                                monotonic_ms=last["monotonic_ms"], verify_sample=verify)
        p0, p1 = prior["samples"]
        i0, anchor = initial["samples"]
        if (p0["monotonic_ms"] > p1["monotonic_ms"]
                or p1["monotonic_ms"] - p0["monotonic_ms"] > 240000
                or not 0 <= mutation - p1["monotonic_ms"] <= 30000
                or not 0 <= i0["monotonic_ms"] - mutation <= 30000
                or not 0 <= transition - anchor["monotonic_ms"] <= 30000
                or monotonic_ms - anchor["monotonic_ms"] > 30000
                or now_ms - anchor["end_ms"] < 0 or now_ms - anchor["end_ms"] > 30000):
            raise EvidenceError("invalid bookend timing")
        routes = record["routes"]
        if (type(routes) is not list or len(routes) > MAX_SERIES // len(RESULTS)
                or not {row["route"] for sample in (p0, p1) for row in sample["series"]} <= set(routes)):
            raise EvidenceError("prior route omitted from manifest")
        raw_baseline = record["baseline"]
        if type(raw_baseline) is not dict or any(type(v) is not list or len(v) != 2
                                                 for v in raw_baseline.values()):
            raise EvidenceError("invalid baseline")
        baseline = {key: tuple(value) for key, value in raw_baseline.items()}
        prior_result = assess_window(p0["series"], p1["series"], routes, baseline)
        if prior_result["thin"]:
            raise EvidenceError("insufficient prior volume")
        actual_baseline = {route: (counts["errors"], counts["total"])
                           for route, counts in prior_result["counts"].items()}
        if actual_baseline != baseline:
            raise EvidenceError("baseline does not match bookends")
        if assess_window(i0["series"], anchor["series"], routes, baseline)["assessment"] != "qualified":
            raise EvidenceError("initial evidence did not qualify")
        if type(record["evidence"]) is not dict or not record["evidence"]:
            raise EvidenceError("missing handoff evidence")
        schedule = WindowSchedule(anchor["series"], anchor["monotonic_ms"], transition, routes, baseline)
        sequence = SampleSequence(anchor, initial["proofs"][1], expected, ledger, verify_sample)
        if verify_handoff(copy.deepcopy(record)) is not True:
            raise EvidenceError("handoff authentication failed")
        wall_deadline = min(anchor["end_ms"] + 30000, MAX_COUNTER)
        tick_deadline = min(anchor["monotonic_ms"] + 30000, MAX_COUNTER)
        wall, tick = read_clock()
        if wall > wall_deadline or tick > tick_deadline:
            raise EvidenceError("handoff expired")
        limits = {"clock_domain": clock_domain, "wall_not_before": wall,
                  "tick_not_before": tick, "wall_deadline": wall_deadline,
                  "tick_deadline": tick_deadline}
        claimed = claim(copy.deepcopy(record), copy.deepcopy(limits))
        wall, tick = read_clock()
        if claimed is not True or wall > wall_deadline or tick > tick_deadline:
            raise EvidenceError("handoff claim failed or expired")
        return record, sequence, schedule
    except Exception:
        raise EvidenceError("invalid or unverified watch handoff") from None
