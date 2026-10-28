"""Catalog completeness for emitted series and alert-rule inputs.

Companion to `test_observability_conformance.py`, which pins callsite-anchor
stability and the REST route list but cannot see a new metric series, job
label, or alert rule that ships without a catalog row. This suite maps the
full emission surface to rows in `docs/observability-event-catalog.md`:

- every `two_bot_` series family rendered by `crates/core/src/metrics.rs`,
- every alert-input series in `wrangler/src/metrics-scrape.ts`
  (`ALERT_INPUT_LABELS` plus `ALERT_OPTIONAL_INPUT_LABELS`),
- every job label in `JOBS` and the scrape `job:` axes,
- every rule id in `wrangler/src/alert-rules.ts` (`RULES`) and
  `crates/core/src/evidence.rs` (`ALERT_RULE_IDS`).

Each item must either appear in the catalog (series families anywhere in the
text; job labels as first-column rows of the Scheduled-job table; rule ids as
first-column rows of the Alert-rules table) or sit in the explicit exemption
map beside it with the reason it lives outside the catalog (documented in
`docs/metrics.md`, owned by another gate, or landing with an open change).
Exemptions are load-bearing in both directions: an exempt item that gains a
catalog row fails until its exemption is removed, and a covered item that
loses its row fails until it is re-cataloged or exempted. A brand-new series,
label, or rule fails here until it is cataloged or explicitly exempted.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CATALOG = ROOT / "docs/observability-event-catalog.md"
METRICS_RS = ROOT / "crates/core/src/metrics.rs"
SCRAPE_TS = ROOT / "wrangler/src/metrics-scrape.ts"
RULES_TS = ROOT / "wrangler/src/alert-rules.ts"
EVIDENCE_RS = ROOT / "crates/core/src/evidence.rs"

SERIES = re.compile(r"two_bot_[a-z0-9_]+")
QUOTED = re.compile(r'"([^"]+)"')
RULE_ID = re.compile(r"id:\s*\"([^\"]+)\"")
JOB_AXIS = re.compile(r"\bjob\s*:\s*\[([^\]]*)\]")
INLINE_TESTS_MOD = re.compile(r"#\[cfg\(test\)\]\s*\n\s*mod tests\s*\{")
HISTOGRAM_CHILD = re.compile(r"_(bucket|sum|count)$")

_GUARD_REASON = (
    "REST admission-guard internals documented in docs/metrics.md; "
    "consumed by no alert rule"
)
_POOL_REASON = (
    "pool gauge ownership is an open design question; documented in "
    "docs/metrics.md; alert input for db_pool_saturated"
)
_VOICE_SERIES_REASON = (
    "metric row not yet in the catalog (the catalog covers voice_event log "
    "fields); documented in docs/metrics.md"
)
_ALERT_INPUT_REASON = (
    "alert input documented in the docs/metrics.md rule table; "
    "no catalog row yet"
)
# Every emitted family absent from the catalog, with why it stays out.
# A name here that gains a catalog row fails until its exemption is removed.
EXEMPT_SERIES = {
    "two_bot_rest_guard_invalid_requests_in_window": _GUARD_REASON,
    "two_bot_rest_guard_global_paused": _GUARD_REASON,
    "two_bot_rest_guard_global_pause_remaining_seconds": _GUARD_REASON,
    "two_bot_db_pool_configured": _POOL_REASON,
    "two_bot_db_pool_connections": _POOL_REASON,
    "two_bot_db_pool_idle_connections": _POOL_REASON,
    "two_bot_db_pool_max_connections": _POOL_REASON,
    "two_bot_voice_operations_total": _VOICE_SERIES_REASON + "; alert input for voice_failures",
    "two_bot_voice_dead_letters_total": _VOICE_SERIES_REASON + "; alert input for voice_failures",
    "two_bot_voice_orphans_total": _VOICE_SERIES_REASON + "; alert input for voice_failures",
    "two_bot_voice_reconcile_actions_total": _VOICE_SERIES_REASON,
    "two_bot_voice_tracked_rooms": _VOICE_SERIES_REASON,
    "two_bot_voice_compensation_pending": _VOICE_SERIES_REASON,
    "two_bot_voice_names_total": _VOICE_SERIES_REASON,
    "two_bot_voice_vote_kick_total": _VOICE_SERIES_REASON,
    "two_bot_event_voice_tick_total": _VOICE_SERIES_REASON,
    "two_bot_dispatch_drops_total": _ALERT_INPUT_REASON + " (dispatch_drops)",
    "two_bot_gateway_checkpoint_failures_total": (
        _ALERT_INPUT_REASON + " (gateway_checkpoint_failures)"
    ),
    "two_bot_internal_actions_total": _ALERT_INPUT_REASON + " (receiver_refusals)",
    "two_bot_send_admissions_total": _ALERT_INPUT_REASON + " (send_admission_blocked)",
    # (The backoff cap, in-flight REST age and join occupancy left this map
    # once the catalog named them: the first two as alert inputs, occupancy
    # in the deliberate-deferral note. joins_received, dispatch_timeouts,
    # watchdog_stucks, halt_recoveries and the creator-map generation/age
    # pair are likewise cataloged as alert inputs, and the worker-halted /
    # queue-pending / queue-oldest-age stall gauges gained catalog rows, so
    # none of those carry an exemption.)
    # Histogram `_bucket` child consumed as an alert input: the gate
    # normalizes catalog families (stripping `_bucket`) but compares raw
    # alert-input names, so only an exemption covers this spelling. The base
    # family row lives in the catalog voice_event section.
    "two_bot_voice_join_to_move_seconds_bucket": (
        "histogram _bucket child of the cataloged "
        "two_bot_voice_join_to_move_seconds family; alert input for "
        "voice_join_fast_burn and voice_join_slow_burn (cumulative le=\"5\" "
        "count); documented in docs/metrics.md and the catalog alert-rule rows"
    ),
    # Same `_bucket`-child spelling rule as the voice join family above: the
    # base two_bot_handler_duration_seconds family stays exempt while the
    # raw alert-input name needs its own exemption.
    "two_bot_handler_duration_seconds_bucket": (
        "histogram _bucket child of the exempt "
        "two_bot_handler_duration_seconds family; alert input for "
        "handler_duration_saturated (per-le deltas with the p99 bound); "
        "documented in docs/metrics.md and the catalog alert-rule rows"
    ),
    # Ticket admission saturation: backpressure signal with no alert rule.
    # Documented in docs/metrics.md and docs/tickets.md (saturation code
    # and on-call next action); the catalog covers event fields, not this
    # label-free counter.
    "two_bot_ticket_component_drops_total": (
        "ticket admission saturation signal documented in docs/metrics.md "
        "and docs/tickets.md; consumed by no alert rule"
    ),
}

_PREDATES_ROWS_REASON = (
    "predates catalog rows; documented in the docs/metrics.md job coverage section"
)

# Every job label without a Scheduled-job table row, with why it stays out.
EXEMPT_JOB_LABELS = {
    "invite_snapshot": _PREDATES_ROWS_REASON,
    "counter": _PREDATES_ROWS_REASON,
    "rank": _PREDATES_ROWS_REASON,
    "scheduled_events": _PREDATES_ROWS_REASON,
    "settings": _PREDATES_ROWS_REASON,
    "presence_probe": _PREDATES_ROWS_REASON,
    "community_scorecard": _PREDATES_ROWS_REASON,
    "inactivity": _PREDATES_ROWS_REASON,
    "audit_retry": _PREDATES_ROWS_REASON,
    "scheduled_messages": _PREDATES_ROWS_REASON,
    # Event-driven commit marker, excluded from the staleness fallback; its
    # semantics live in the catalog alert-rules row and docs/metrics.md.
    "session_checkpoint": (
        "event-driven commit marker excluded from the staleness fallback; "
        "documented in the catalog alert-rules row and docs/metrics.md"
    ),
    # Catch-all for unknown job names with no single cadence.
    "other": (
        "catch-all for unknown job names with no single cadence; "
        "documented in docs/metrics.md"
    ),
}

_NO_ROW_YET_REASON = (
    "documented in the docs/metrics.md rule table and docs/runbook.md; "
    "no catalog row yet"
)

# Every rule id without an Alert-rules table row, with why it stays out.
EXEMPT_RULES = {
    "job_stale": _NO_ROW_YET_REASON,
    "job_consecutive_failures": _NO_ROW_YET_REASON,
    "rest_429_rate": _NO_ROW_YET_REASON,
    "db_pool_saturated": _NO_ROW_YET_REASON,
    "db_errors": _NO_ROW_YET_REASON,
    "send_admission_blocked": _NO_ROW_YET_REASON,
    "voice_failures": _NO_ROW_YET_REASON,
    "gateway_missed_events": _NO_ROW_YET_REASON,
    "ticker_stale": _NO_ROW_YET_REASON,
    "receiver_refusals": _NO_ROW_YET_REASON,
    "dispatch_drops": _NO_ROW_YET_REASON,
    "gateway_checkpoint_failures": _NO_ROW_YET_REASON,
}


def production_source(source: str) -> str:
    """Source outside the inline `#[cfg(test)] mod tests` block."""
    match = INLINE_TESTS_MOD.search(source)
    return source[: match.start()] if match else source


def const_block(source: str, marker: str) -> str:
    """First `[...]`/`{...}` block after `marker` (Rust const or TS const)."""
    start = source.index(marker)
    ends = [
        end
        for closer in ("};", "];")
        if (end := source.find(closer, start)) != -1
    ]
    if not ends:
        raise AssertionError(f"unterminated block after {marker!r}")
    return source[start : min(ends) + 2]


def emitted_series_families() -> set:
    """Base `two_bot_` names rendered by `metrics.rs` (histogram-normalized)."""
    prod = production_source(METRICS_RS.read_text())
    families = set()
    for name in SERIES.findall(prod):
        families.add(HISTOGRAM_CHILD.sub("", name))
    return families


def alert_input_series() -> set:
    """Series families the Worker validator requires or accepts when present."""
    text = SCRAPE_TS.read_text()
    start = text.index("export const ALERT_INPUT_LABELS")
    end = text.index("function identity")
    return set(SERIES.findall(text[start:end]))


def job_labels() -> set:
    """Union of the Rust `JOBS` allowlist and the scrape `job:` axes."""
    labels = set(QUOTED.findall(const_block(METRICS_RS.read_text(), "pub const JOBS")))
    for line in SCRAPE_TS.read_text().splitlines():
        axis = JOB_AXIS.search(line)
        if axis:
            labels.update(QUOTED.findall(axis.group(1)))
    return labels


def rule_ids() -> set:
    """Shared alert-rule spellings: the Worker table must equal the Rust list."""
    worker = set(RULE_ID.findall(const_block(RULES_TS.read_text(), "export const RULES")))
    rust = set(
        QUOTED.findall(const_block(EVIDENCE_RS.read_text(), "pub const ALERT_RULE_IDS"))
    )
    assert worker == rust, (
        "Worker RULES drifted from evidence.rs ALERT_RULE_IDS; "
        f"worker-only={sorted(worker - rust)} rust-only={sorted(rust - worker)}"
    )
    return worker


def section_rows(text: str, start_heading: str, end_heading: str) -> set:
    """First-column backticked cells of every table row in one section."""
    start = text.find(start_heading)
    end = text.find(end_heading, start + 1) if start != -1 else -1
    if start == -1 or end == -1:
        return set()
    section = text[start:end]
    rows = set()
    for line in section.splitlines():
        if not line.startswith("|"):
            continue
        cells = line.split("|")
        if len(cells) < 4:
            continue
        first = cells[1].strip()
        if re.fullmatch(r"`[^`]+`", first):
            rows.add(first.strip("`"))
    return rows


def catalog_series_families(catalog: str) -> set:
    """Tokenized `two_bot_` series families named anywhere in the catalog."""
    return {HISTOGRAM_CHILD.sub("", n) for n in SERIES.findall(catalog)}


class CatalogCompletenessTests(unittest.TestCase):
    def test_exemptions_carry_reasons(self):
        for name, table in [
            ("EXEMPT_SERIES", EXEMPT_SERIES),
            ("EXEMPT_JOB_LABELS", EXEMPT_JOB_LABELS),
            ("EXEMPT_RULES", EXEMPT_RULES),
        ]:
            with self.subTest(table=name):
                self.assertGreater(len(table), 0, f"{name} must not be empty")
                for key, reason in table.items():
                    self.assertTrue(key.strip(), f"{name} has a blank key")
                    self.assertTrue(
                        reason.strip(), f"{name}[{key}] needs a reason it stays out"
                    )

    def test_exempt_keys_are_still_emitted(self):
        emitted_series = emitted_series_families() | alert_input_series()
        for name, stale in [
            ("EXEMPT_SERIES", set(EXEMPT_SERIES) - emitted_series),
            ("EXEMPT_JOB_LABELS", set(EXEMPT_JOB_LABELS) - job_labels()),
            ("EXEMPT_RULES", set(EXEMPT_RULES) - rule_ids()),
        ]:
            with self.subTest(table=name):
                self.assertEqual(
                    stale,
                    set(),
                    f"{name} holds stale keys no longer emitted: {sorted(stale)}; "
                    "remove them or fix the spelling",
                )

    def test_emitted_series_families_are_cataloged_or_exempt(self):
        cataloged = catalog_series_families(CATALOG.read_text())
        for family in sorted(emitted_series_families()):
            with self.subTest(series=family):
                covered = family in cataloged
                exempt = family in EXEMPT_SERIES
                if exempt:
                    self.assertFalse(
                        covered,
                        f"{family} gained a catalog row; remove it from EXEMPT_SERIES",
                    )
                else:
                    self.assertTrue(
                        covered,
                        f"{family} is emitted but has no catalog row; "
                        "add a row to docs/observability-event-catalog.md "
                        "or an entry to EXEMPT_SERIES with its reason",
                    )

    def test_alert_input_series_are_cataloged_or_exempt(self):
        cataloged = catalog_series_families(CATALOG.read_text())
        for family in sorted(alert_input_series()):
            with self.subTest(series=family):
                covered = family in cataloged
                exempt = family in EXEMPT_SERIES
                if exempt:
                    self.assertFalse(
                        covered,
                        f"{family} gained a catalog row; remove it from EXEMPT_SERIES",
                    )
                else:
                    self.assertTrue(
                        covered,
                        f"{family} is consumed by alert rules but has no catalog row; "
                        "add a row to docs/observability-event-catalog.md "
                        "or an entry to EXEMPT_SERIES with its reason",
                    )

    def test_job_labels_are_cataloged_or_exempt(self):
        rows = section_rows(
            CATALOG.read_text(), "## Scheduled job labels", "## Alert rules"
        )
        self.assertGreater(len(rows), 0, "expected scheduled-job rows in the catalog")
        for label in sorted(job_labels()):
            with self.subTest(label=label):
                covered = label in rows
                exempt = label in EXEMPT_JOB_LABELS
                if exempt:
                    self.assertNotIn(
                        label,
                        rows,
                        f"{label} gained a scheduled-job row; "
                        "remove it from EXEMPT_JOB_LABELS",
                    )
                else:
                    self.assertIn(
                        label,
                        rows,
                        f"job label {label} is emitted but has no scheduled-job row; "
                        "add a row to docs/observability-event-catalog.md "
                        "or an entry to EXEMPT_JOB_LABELS with its reason",
                    )

    def test_alert_rule_ids_are_cataloged_or_exempt(self):
        rows = section_rows(
            CATALOG.read_text(), "## Alert rules", "## Gateway failure classes"
        )
        self.assertGreater(len(rows), 0, "expected alert-rule rows in the catalog")
        for rule in sorted(rule_ids()):
            with self.subTest(rule=rule):
                covered = rule in rows
                exempt = rule in EXEMPT_RULES
                if exempt:
                    self.assertNotIn(
                        rule,
                        rows,
                        f"{rule} gained an alert-rule row; remove it from EXEMPT_RULES",
                    )
                else:
                    self.assertIn(
                        rule,
                        rows,
                        f"rule {rule} fires from alert-rules.ts but has no alert-rule row; "
                        "add a row to docs/observability-event-catalog.md "
                        "or an entry to EXEMPT_RULES with its reason",
                    )


if __name__ == "__main__":
    unittest.main()
