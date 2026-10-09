"""Offline conformance for the observability catalog and REST route list.

- `docs/observability-event-catalog.md` identifies callsites by stable
  anchors (file plus the enclosing function or method), never by `file:line`
  numbers: line numbers drift as the code moves, symbols do not.
- The `two_bot_rest_requests_total{route,result}` list in `docs/metrics.md`
  equals `metrics::REST_ROUTES` in `crates/core/src/metrics.rs` exactly
  (order included, trailing `other` included), so scrapers and the executor
  template table cannot drift apart silently.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CATALOG = ROOT / "docs/observability-event-catalog.md"
METRICS_DOC = ROOT / "docs/metrics.md"
METRICS_RS = ROOT / "crates/core/src/metrics.rs"

LINE_NUMBER = re.compile(r"\.rs:\d")
QUOTED = re.compile(r'"([^"]+)"')
BACKTICKED = re.compile(r"`([^`]+)`")
ROUTE = re.compile(r"^(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS) /\S*$|^other$")


def rest_routes_from_source() -> list:
    source = METRICS_RS.read_text()
    marker = "pub const REST_ROUTES: &[&str] = &["
    start = source.index(marker) + len(marker)
    end = source.index("];", start)
    return QUOTED.findall(source[start:end])


def rest_routes_from_doc() -> list:
    text = METRICS_DOC.read_text()
    marker = "- `two_bot_rest_requests_total{route,result}`"
    start = text.index(marker)
    # The route list ends at the closing "`, `other`)." of this bullet; the
    # next bullet documents the job series.
    end = text.index("`other`)", start) + len("`other`)")
    return [token for token in BACKTICKED.findall(text[start:end]) if ROUTE.match(token)]


class ObservabilityConformanceTests(unittest.TestCase):
    def test_catalog_has_no_line_number_callsites(self):
        text = CATALOG.read_text()
        stale = LINE_NUMBER.findall(text)
        self.assertEqual(
            stale,
            [],
            "catalog uses file:line callsites, which drift; "
            "use file plus the enclosing function/method instead",
        )

    def test_catalog_callsite_rows_name_a_stable_anchor(self):
        rows = [
            line
            for line in CATALOG.read_text().splitlines()
            if line.startswith("|") and "crates/" in line
        ]
        self.assertGreater(len(rows), 0, "expected callsite rows in the catalog")
        for row in rows:
            with self.subTest(row=row[:80]):
                # The file cell names the file plus its enclosing item, e.g.
                # `crates/bot/src/gateway.rs` (`run_shard`).
                self.assertIn("(", row)
                self.assertNotRegex(row, r"\.rs[\"`]*\s*:?\s*\d")

    def test_doc_route_list_equals_rest_routes(self):
        self.assertEqual(
            rest_routes_from_doc(),
            rest_routes_from_source(),
            "docs/metrics.md route list drifted from metrics::REST_ROUTES; "
            "update both together",
        )

    def test_voice_lifecycle_routes_are_documented(self):
        # Regression pin: these two executor templates once lived only in
        # REST_ROUTES while the doc list ended at the scheduled-events batch.
        documented = rest_routes_from_doc()
        self.assertIn("POST /guilds/:guild/channels", documented)
        self.assertIn("DELETE /channels/:channel", documented)


if __name__ == "__main__":
    unittest.main()
