"""Gate2 docs conformance for staging idle-sleep PR1883 (offline only).

Section-bound doc guards only: they assert the Gate2 runbook text requires
fail-closed evidence rules (or explicitly says NOT VERIFIED with the gate
closed). They report doc-check gaps, not per-attempt runtime proof. No new
live evidence paths, no hold changes, no other files.

Run offline:
  PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci \\
    -p 'test_staging_idle_soak_gate_acceptance.py' -v

Baseline is RED: the current Gate2 synthetic row claims run-level listings
as proof without per-attempt/saturation/discovery-only/identity rules and
without a bound NOT VERIFIED. Binding explicit NOT VERIFIED to the Gate2
synthetic subcheck turns these GREEN.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
GATE_PATH = ROOT / "docs/soak-entry-gates.md"

CLOSED = ("closes the gate", "gate closed", "stays closed",
          "keeps t unset", "closes gate 2")

REQ_2 = ("Gate2 doc must require complete independently verified per-attempt "
         "staging-job/probe overlap incl superseded attempts, pagination/partitioning "
         "or fail-closed saturation, listings discovery ONLY (no-rerun alone "
         "insufficient), or explicitly say NOT VERIFIED with the gate closed")
REQ_3 = ("Gate2 doc must require every counted probe receipt's exact Gate1 canonical "
         "origin AND pinned SHA with unknown/missing/mismatched fail-closed, or "
         "explicitly leave synthetic receipt identity NOT VERIFIED with the gate closed")


def read_gate2():
    text = GATE_PATH.read_text(encoding="utf-8")
    match = re.search(r"^## Gate 2\b.*?(?=^## Gate 3\b)", text, re.M | re.S)
    if not match:
        raise AssertionError("Gate2 section not found")
    return match.group(0)


def _chunks(gate2):
    # Scan individual table rows plus non-table paragraphs only. A combined
    # table block must not merge a synthetic row with an unrelated NOT
    # VERIFIED row elsewhere in the table.
    rows = [ln for ln in gate2.splitlines() if ln.lstrip().startswith("|")]
    paras = [c for c in re.split(r"\n\s*\n", gate2) if c.strip()
             and not any(ln.lstrip().startswith("|") for ln in c.splitlines())]
    return paras + rows


def _closed(lowered):
    return any(p in lowered for p in CLOSED)


def overlap_guard(gate2):
    """True when Gate2 synthetic attempt/overlap is fail-closed (doc text)."""
    for chunk in _chunks(gate2):
        low = chunk.lower()
        synth = ("synthetic" in low or "staging-job" in low
                 or "no readiness failures" in low)
        if not synth:
            continue
        if ("not verified" in low and _closed(low) and any(
                k in low for k in ("attempt", "overlap", "rerun", "superseded",
                                   "saturation", "truncat", "discovery", "complet",
                                   "pagination", "partition"))):
            return True
        per_attempt = ("superseded" in low
                       and ("per-attempt" in low or "attempt" in low))
        saturation = any(k in low for k in ("saturation", "truncat",
                                            "paginate", "pagination", "partition"))
        discovery = "discovery" in low and "only" in low
        independent = "independently verified" in low
        if (per_attempt and saturation and discovery and independent
                and (_closed(low) or "fail-closed" in low or "fail closed" in low)):
            return True
    return False


def identity_guard(gate2):
    """True when Gate2 synthetic receipt identity is fail-closed (doc text)."""
    for chunk in _chunks(gate2):
        low = chunk.lower()
        scope = any(k in low for k in ("synthetic", "staging-job", "probe",
                                       "receipt", "readiness"))
        if not scope:
            continue
        keys = ("origin", "revision", "pinned", "build_revision",
                "expected-sha", "staging_worker_url", "receipt", "canonical")
        if "not verified" in low and _closed(low) and any(k in low for k in keys):
            return True
        exact = ("origin" in low and "revision" in low
                 and "pinned" in low and "canonical" in low)
        unknown = all(k in low for k in ("unknown", "missing", "mismatched"))
        if (exact and unknown and any(k in low for k in ("synthetic", "probe", "receipt"))
                and (_closed(low) or "fail-closed" in low or "fail closed" in low
                     or "closes the subcheck" in low or "closes subcheck" in low)):
            return True
    return False


class Gate2DocTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.gate2 = read_gate2()

    def test_attempt_overlap_requires_complete_evidence_or_not_verified(self):
        self.assertTrue(overlap_guard(self.gate2), REQ_2)

    def test_probe_identity_requires_origin_revision_or_not_verified(self):
        self.assertTrue(identity_guard(self.gate2), REQ_3)

    def test_baseline_row_plus_generic_hold_fails_both(self):
        # Adjacent rows (no blank line): synthetic overlap/close in one row,
        # NOT VERIFIED hold in the other. Must not merge across rows.
        doc = ("| No readiness failures | Every synthetic-user-flow staging-job run "
               "listed; missing overlap evidence closes the gate | At least one run |\n"
               "| Generic hold | These terms and sources remain NOT VERIFIED, so this "
               "gate stays closed | TBD |")
        self.assertFalse(overlap_guard(doc))
        self.assertFalse(identity_guard(doc))


class OverlapGuardMutations(unittest.TestCase):
    """In-memory controls: valid fail-closed passes, weak promises fail."""

    def test_accepts(self):
        valid_not_verified = (
            "| No readiness failures | Every synthetic-user-flow staging-job run | "
            "NOT VERIFIED: per-attempt overlap incl superseded reruns cannot be proven "
            "complete from listings alone; listings are discovery ONLY; saturation "
            "unknown; gate closed and T unset |")
        valid_complete = (
            "| No readiness failures | Every synthetic-user-flow staging-job attempt, "
            "incl superseded reruns, independently verified per-attempt overlap; "
            "paginated listing with fail-closed saturation; listings are discovery ONLY "
            "| missing/incomplete attempt evidence closes the gate |")
        for doc in (valid_not_verified, valid_complete):
            with self.subTest(doc=doc[:40]):
                self.assertTrue(overlap_guard(doc))

    def test_rejects(self):
        norerun_alone = (
            "| No readiness failures | Do not rerun synthetic-user-flow during the "
            "interval; every created run success | At least one run |")
        generic_not_verified = (
            "These terms and sources remain NOT VERIFIED, so this gate stays closed.")
        listing_promise = (
            "| No readiness failures | Every synthetic-user-flow run listed; listing "
            "is complete and proves no failures | At least one run success |")
        per_attempt_no_superseded = (
            "| No readiness failures | Every synthetic-user-flow staging-job attempt "
            "independently verified per-attempt overlap; paginated listing with "
            "fail-closed saturation; listings are discovery ONLY | closes the gate |")
        docs = (norerun_alone, generic_not_verified, listing_promise,
                per_attempt_no_superseded)
        for doc in docs:
            with self.subTest(doc=doc[:40]):
                self.assertFalse(overlap_guard(doc))


class IdentityGuardMutations(unittest.TestCase):
    def test_accepts(self):
        valid_not_verified = (
            "| No readiness failures | Every synthetic-user-flow probe receipt: origin "
            "and build_revision NOT VERIFIED against the Gate1 canonical origin and "
            "pinned SHA; unknown/missing/mismatched closes the subcheck; gate closed |")
        valid_exact = (
            "| No readiness failures | Every counted synthetic probe receipt must carry "
            "the exact canonical origin AND the Gate1 pinned SHA (build_revision); "
            "unknown/missing/mismatched origin or revision closes the subcheck |")
        for doc in (valid_not_verified, valid_exact):
            with self.subTest(doc=doc[:40]):
                self.assertTrue(identity_guard(doc))

    def test_rejects(self):
        generic_not_verified = (
            "These terms and sources remain NOT VERIFIED, so this gate stays closed.")
        listing_promise = (
            "| No readiness failures | Every synthetic-user-flow run reads /readyz and "
            "/health; trust the listing | At least one run success |")
        origin_only = (
            "| No readiness failures | Every synthetic probe receipt origin checked; "
            "revision assumed from listing success | At least one run success |")
        single_unknown = (
            "| No readiness failures | Every counted synthetic probe receipt must carry "
            "the exact canonical origin AND the Gate1 pinned SHA (build_revision); "
            "unknown origin closes the subcheck |")
        docs = (generic_not_verified, listing_promise, origin_only, single_unknown)
        for doc in docs:
            with self.subTest(doc=doc[:40]):
                self.assertFalse(identity_guard(doc))


if __name__ == "__main__":
    unittest.main()
