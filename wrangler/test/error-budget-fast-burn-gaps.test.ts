/**
 * Scrape-gap fixtures for the error-budget fast-burn page.
 *
 * A failed /metrics scrape never evaluates, so after a gap the ring's window
 * boundary baselines are stale or missing. These pin that a firing page
 * survives the gap (no resolve/re-page flap) and that an uncovered short
 * window never opens a new page on unattributable errors. Kept in a separate
 * file from the acceptance fixtures, which must stay unmodified.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  EMPTY_STATE,
  evaluateMetrics,
  parseExposition,
  type MetricsAlertState,
} from "../src/alert-rules.ts";

const NOW = 3_000_000;

function body(extra: string[]): string {
  return ["# HELP x y", "# TYPE x gauge", ...extra, ""].join("\n");
}

/** Cumulative REST counters: total eligible plus 5xx errors. */
function restLines(total: number, errors5xx: number): string[] {
  return [
    `two_bot_rest_requests_total{route="other",result="2xx"} ${total - errors5xx}`,
    `two_bot_rest_requests_total{route="other",result="5xx"} ${errors5xx}`,
  ];
}

function scrape(total: number, errors: number, state: MetricsAlertState, t: number) {
  return evaluateMetrics(parseExposition(body(restLines(total, errors))), state, t);
}

test("scrape gap during a firing burn holds the page without flapping", () => {
  // Forced 12x burn: ~167 eligible events/min with 2 errors/min over the
  // last 15 min of a 20 min feed.
  let state: MetricsAlertState = EMPTY_STATE;
  let total = 100_000;
  let errors = 1_000;
  let t = NOW - 20 * 60;
  state = scrape(total, errors, state, t).state;
  let last = scrape(total, errors, state, t);
  for (let minute = 1; minute <= 20; minute += 1) {
    const burning = minute > 5;
    total += 167;
    errors += burning ? 2 : 0;
    t += 60;
    last = scrape(total, errors, state, t);
    state = last.state;
  }
  assert.ok(
    last.firing.includes("error_budget_fast_burn"),
    `burn must page before the gap, got: ${last.firing.join(", ")}`,
  );

  // Two consecutive scrapes fail: no evaluation runs, but the process keeps
  // serving at the same burn, so the cumulative counters advance across the
  // ~186 s gap.
  total += 517;
  errors += 7;
  t += 186;

  // Resume 60 s scrapes for 17 more minutes. About 15 min after the gap the
  // pre-gap baselines age past the prune cutoff; without the long-window
  // anchor that reads as uncovered and the page flaps resolve/re-page. Every
  // single evaluation must keep the page up.
  for (let minute = 1; minute <= 17; minute += 1) {
    total += 167;
    errors += 2;
    const next = scrape(total, errors, state, t);
    assert.ok(
      next.firing.includes("error_budget_fast_burn"),
      `page must hold through the gap (minute ${minute} after resume), got: ${next.firing.join(", ")}`,
    );
    state = next.state;
    t += 60;
  }

  // Bounded history: the shared ring now retains about 2 h of 60 s scrapes
  // for the slow-burn ticket plus its anchor, never an unbounded log. This
  // 37 min feed keeps every snapshot.
  const ring = state.errorBudgetBurn ?? [];
  assert.ok(ring.length >= 30 && ring.length <= 50, `ring must stay bounded near the feed length, got ${ring.length} snapshots`);
});

test("uncovered short window after a gap never opens a new page", () => {
  // Arm the long window with 15 min of ~18x burn, then cool the short window
  // with quiet minutes so the rule resolves and is not firing.
  let state: MetricsAlertState = EMPTY_STATE;
  let total = 50_000;
  let errors = 500;
  let t = NOW - 18 * 60;
  state = scrape(total, errors, state, t).state;
  for (let minute = 1; minute <= 15; minute += 1) {
    total += 170;
    errors += 3;
    t += 60;
    state = scrape(total, errors, state, t).state;
  }
  for (let quiet = 0; quiet < 3; quiet += 1) {
    total += 170;
    t += 60;
    state = scrape(total, errors, state, t).state;
  }

  // A longer scrape gap carries 7 fresh errors, then successes. The gap
  // errors cannot be attributed to the last 2 min without a boundary sample:
  // sweeping them into the confirmation window reads ~18x and would page.
  total += 390;
  errors += 7;
  t += 260;
  const next = scrape(total, errors, state, t);
  assert.ok(
    !next.firing.includes("error_budget_fast_burn"),
    "uncovered short window must wait for history, not page on unattributable errors",
  );
});
