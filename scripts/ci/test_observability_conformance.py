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
CALLSITE = re.compile(r"`(crates/[^`]*\.rs)`\s*\(([^)]+)\)")
IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*$")
FN_DEF = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+"
    r"([A-Za-z_][A-Za-z0-9_]*)\b",
    re.MULTILINE,
)
INLINE_TESTS_MOD = re.compile(r"#\[cfg\(test\)\]\s*\n\s*mod tests\s*\{")


def production_source(source: str) -> str:
    """Source outside the inline `#[cfg(test)] mod tests` block.

    The inline test module (when present) runs to the end of the file, so
    truncating there keeps emit-site occurrences while excluding test-only
    helpers with colliding names.
    """
    match = INLINE_TESTS_MOD.search(source)
    return source[: match.start()] if match else source


def enclosing_fn(prod: str, index: int) -> str | None:
    """Name of the nearest `fn` definition above `index`, if any."""
    last = None
    for match in FN_DEF.finditer(prod, 0, index):
        last = match.group(1)
    return last


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
        sources: dict = {}
        for row in rows:
            with self.subTest(row=row[:80]):
                # No file:line drift: line numbers move, symbols do not.
                self.assertNotRegex(row, r"\.rs[\"`]*\s*:?\s*\d")
                # The callsite cell names the file plus its enclosing item,
                # e.g. `crates/bot/src/gateway.rs` (`run_shard`).
                cells = row.split("|")
                self.assertGreaterEqual(
                    len(cells),
                    4,
                    f"callsite row is not a 3-column table row: {row[:80]}",
                )
                callsite = cells[2]
                match = CALLSITE.search(callsite)
                self.assertIsNotNone(
                    match,
                    "callsite cell must name the file plus the enclosing "
                    "function/method, e.g. `crates/bot/src/gateway.rs` "
                    f"(`run_shard`): {row[:80]}",
                )
                assert match is not None
                rel, inside = match.group(1), match.group(2)
                # Only identifier-like backticked tokens are anchors; message
                # strings such as `voice_operation succeeded` and prose such
                # as "three sites" are not.
                anchors = [
                    token for token in BACKTICKED.findall(inside) if IDENT.match(token)
                ]
                self.assertGreater(
                    len(anchors),
                    0,
                    f"callsite cell names no enclosing function/method: {row[:80]}",
                )
                if rel not in sources:
                    sources[rel] = production_source((ROOT / rel).read_text())
                prod = sources[rel]
                anchor_finals = {qualified.split("::")[-1] for qualified in anchors}
                for qualified in anchors:
                    # `HttpInvites::current` pins the method; the file must
                    # still define a function with the final segment so a
                    # rename turns this suite red. Moves are caught below by
                    # the containment check.
                    name = qualified.split("::")[-1]
                    self.assertRegex(
                        prod,
                        rf"\bfn {re.escape(name)}\b",
                        f"stable anchor `{qualified}` names no function "
                        f"in {rel}; update the catalog with the rename/move",
                    )
                # Each row's message literal must be emitted inside one of
                # its anchor functions: the nearest enclosing `fn` above
                # every occurrence has to be an anchor, so pointing a row at
                # the wrong (but existing) function turns this suite red.
                # Event names live in the first column; voice rows name the
                # field there and carry the message literals in the callsite
                # cell instead.
                messages: list = []
                for token in BACKTICKED.findall(cells[1]):
                    if '="' not in token and "crates/" not in token and ".rs" not in token:
                        messages.append(token)
                for token in BACKTICKED.findall(callsite):
                    if (
                        not IDENT.match(token)
                        and "crates/" not in token
                        and ".rs" not in token
                        and '="' not in token
                    ):
                        messages.append(token)
                messages = list(dict.fromkeys(messages))
                self.assertGreater(
                    len(messages),
                    0,
                    f"callsite row names no emitted message: {row[:80]}",
                )
                for message in messages:
                    starts = [
                        match.start()
                        for match in re.finditer(re.escape(message), prod)
                    ]
                    self.assertGreater(
                        len(starts),
                        0,
                        f"message `{message}` is not emitted in {rel}; "
                        "update the catalog with the rename/move",
                    )
                    for start in starts:
                        owner = enclosing_fn(prod, start)
                        self.assertIn(
                            owner,
                            anchor_finals,
                            f"message `{message}` in {rel} is emitted in "
                            f"`{owner}`, not in {sorted(anchor_finals)}; "
                            "update the catalog with the move",
                        )

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
