/**
 * Acceptance tests: error-budget slow burn files a ticket (never a page).
 *
 * Offline pin of docs/error-budget-alerts.md slow burn (3x over the 2 h
 * long window confirmed by the 15 min short window; worked example at
 * 10,000 eligible events/hour is 60 errors in 2 h confirmed by 8 in 15 min)
 * with synthetic REST window data. The rule, its 2 h window history, and its
 * runbook section do not exist yet, so these tests are red until the
 * slow-burn ticket lands. The builder must not edit this file: add the
 * `error_budget_slow_burn` rule with severity "ticket" immediately after
 * `error_budget_fast_burn`, the `ERROR_BUDGET_SLOW_BURN` /
 * `ERROR_BUDGET_SLOW_LONG_SECONDS` / `ERROR_BUDGET_SLOW_SHORT_SECONDS`
 * constants (3 / 7200 / 900, mirroring `SLOW_BURN_*` in
 * crates/bot/src/burn_rate.rs), 2 h ring retention on the existing
 * `errorBudgetBurn` snapshot ring, hold-on-unknown semantics matching the
 * fast rule, and the `runbook.md#alert-error-budget-slow-burn` section, and
 * keep these fixtures green unmodified. Fast-burn paging must not change.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as alertRules from "../src/alert-rules.ts";
import {
  EMPTY_STATE,
  RULES,
  evaluateMetrics,
  packetFilename,
  parseExposition,
  ruleFor,
  type MetricsAlertState,
} from "../src/alert-rules.ts";

const NOW = 4_000_000;

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

interface Feed {
  state: MetricsAlertState;
  total: number;
  errors: number;
  t: number;
  firing: string[];
}

/**
 * Feed 60 s scrapes for `minutes` minutes at ~10,000 eligible events/hour
 * (167/min) with `errorsPerMinute` budget errors each minute. At this rate
 * 1 error/min burns ~6x: above the 3x slow threshold, below the 12x page.
 */
function feed(feedState: Feed, minutes: number, errorsPerMinute: number): Feed {
  let { state, total, errors, t } = feedState;
  let firing: string[] = [];
  for (let minute = 0; minute < minutes; minute += 1) {
    total += 167;
    errors += errorsPerMinute;
    t += 60;
    const next = scrape(total, errors, state, t);
    state = next.state;
    firing = next.firing;
  }
  return { state, total, errors, t, firing };
}

function start(): Feed {
  const total = 100_000;
  const errors = 1_000;
  const t = NOW - 200 * 60;
  const first = scrape(total, errors, EMPTY_STATE, t);
  return { state: first.state, total, errors, t, firing: first.firing };
}

/** Two-plus hours of sustained ~6x burn: a firing slow-burn ticket. */
function burningFeed(): Feed {
  const burning = feed(start(), 125, 1);
  assert.ok(
    burning.firing.includes("error_budget_slow_burn"),
    `sustained ~6x burn over 2 h + 15 min must ticket, got: ${burning.firing.join(", ")}`,
  );
  return burning;
}

function headingAnchors(text: string): Set<string> {
  return new Set(
    [...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
      m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-"),
    ),
  );
}

test("error-budget slow burn is a ticket rule with a runbook anchor", () => {
  const rule = ruleFor("error_budget_slow_burn");
  assert.ok(rule, "missing error_budget_slow_burn rule");
  assert.equal(rule!.severity, "ticket", "slow burn tickets, never pages");
  assert.equal(rule!.runbook, "runbook.md#alert-error-budget-slow-burn");
  const slugs = headingAnchors(
    readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8"),
  );
  assert.ok(
    slugs.has("alert-error-budget-slow-burn"),
    "missing runbook heading for #alert-error-budget-slow-burn",
  );
  const ids = RULES.map((candidate) => candidate.id);
  assert.equal(
    ids.indexOf("error_budget_slow_burn"),
    ids.indexOf("error_budget_fast_burn") + 1,
    "slow-burn rule must sit immediately after the fast-burn rule",
  );
  assert.equal(ruleFor("error_budget_fast_burn")!.severity, undefined, "fast burn still pages");
});

test("slow-burn constants mirror the Rust policy (3x over 2 h / 15 min)", () => {
  const exported = alertRules as unknown as Record<string, unknown>;
  assert.equal(exported["ERROR_BUDGET_SLOW_BURN"], 3);
  assert.equal(exported["ERROR_BUDGET_SLOW_LONG_SECONDS"], 7200);
  assert.equal(exported["ERROR_BUDGET_SLOW_SHORT_SECONDS"], 900);
});

test("error-budget slow-burn packet carries a stable evidence filename", () => {
  const window = "2026-10-17T00-00-00Z";
  assert.equal(
    packetFilename("error_budget_slow_burn", window),
    `evidence-error_budget_slow_burn-${window}.json`,
  );
});

test("sustained burn in both windows tickets, and only with full 2 h history", () => {
  // One hour of ~6x burn is a burning short window with no 2 h coverage:
  // unknown history must never open a ticket early.
  const partial = feed(start(), 60, 1);
  assert.ok(
    !partial.firing.includes("error_budget_slow_burn"),
    "no ticket before the ring covers the full 2 h long window",
  );
  const burning = feed(partial, 65, 1);
  assert.ok(
    burning.firing.includes("error_budget_slow_burn"),
    `sustained ~6x burn over 2 h + 15 min must ticket, got: ${burning.firing.join(", ")}`,
  );
  assert.ok(
    !burning.firing.includes("error_budget_fast_burn"),
    "~6x sustained burn is a ticket, not a 12x fast-burn page",
  );
  // The ring now retains a full 2 h of history (plus its anchor) and stays
  // bounded: about two hours of 60 s scrapes, never an unbounded log.
  const ring = burning.state.errorBudgetBurn ?? [];
  assert.ok(
    ring.some((s) => s.t <= burning.t - 7200),
    "ring must retain a baseline at or before the 2 h window start",
  );
  assert.ok(ring.length >= 110, `ring must cover ~2 h of scrapes, got ${ring.length}`);
  assert.ok(ring.length <= 145, `ring must stay bounded near 2 h, got ${ring.length} snapshots`);
});

test("short-window spike alone never tickets", () => {
  // Two healthy hours, then a 15 min spike at ~6x: the short window burns
  // but the 2 h window sits under 1x, so there is no slow burn.
  const healthy = feed(start(), 120, 0);
  const spiked = feed(healthy, 15, 1);
  assert.ok(
    !spiked.firing.includes("error_budget_slow_burn"),
    "short-window spike without long-window burn must not ticket",
  );
});

test("long-window burn resolves once the short window cools", () => {
  // After the ticket fires, 16 quiet minutes leave the 2 h window still
  // burning (~5x of aging errors) but the covered 15 min window is clean:
  // the ticket resolves instead of being held on history alone.
  const cooled = feed(burningFeed(), 16, 0);
  assert.ok(
    !cooled.firing.includes("error_budget_slow_burn"),
    "covered quiet short window must resolve the ticket",
  );
});

test("quiet traffic never tickets even when every request fails", () => {
  // One eligible request every ten minutes, all of them errors: the ratio
  // is enormous but each 15 min window holds at most two requests, below
  // the volume floors, for a full 2 h of scrapes.
  let current = start();
  for (let minute = 1; minute <= 125; minute += 1) {
    if (minute % 10 === 0) {
      current = { ...current, total: current.total + 1, errors: current.errors + 1 };
    }
    const next = scrape(current.total, current.errors, current.state, current.t + 60);
    assert.ok(
      !next.firing.includes("error_budget_slow_burn"),
      `trickle traffic must stay silent (minute ${minute})`,
    );
    current = { state: next.state, total: current.total, errors: current.errors, t: current.t + 60, firing: next.firing };
  }
});

test("missing REST series holds a firing ticket instead of resolving it", () => {
  const burning = burningFeed();
  const held = evaluateMetrics(parseExposition(body([])), burning.state, burning.t + 60);
  assert.ok(
    held.firing.includes("error_budget_slow_burn"),
    "unknown inputs must hold the previous firing state, not resolve it",
  );
});

test("scrape gap during a firing burn holds the ticket without flapping", () => {
  const burning = burningFeed();
  // Failed scrapes never evaluate, but the process keeps serving at the
  // same burn, so the cumulative counters advance across the ~186 s gap.
  let current: Feed = {
    ...burning,
    total: burning.total + 501,
    errors: burning.errors + 3,
    t: burning.t + 186,
  };
  for (let minute = 1; minute <= 20; minute += 1) {
    current = feed(current, 1, 1);
    assert.ok(
      current.firing.includes("error_budget_slow_burn"),
      `ticket must hold through the gap (minute ${minute} after resume), got: ${current.firing.join(", ")}`,
    );
  }
});

test("fast-burn page is unchanged and does not open a slow ticket early", () => {
  // The fast-burn fixture: 20 min feed, last 15 min at 2 errors/min (~12x).
  let current = start();
  current = feed(current, 5, 0);
  current = feed(current, 15, 2);
  assert.ok(
    current.firing.includes("error_budget_fast_burn"),
    `forced 12x burn over 15 min + 2 min must still page, got: ${current.firing.join(", ")}`,
  );
  assert.ok(
    !current.firing.includes("error_budget_slow_burn"),
    "20 min of history cannot ticket a 2 h slow burn",
  );
});
