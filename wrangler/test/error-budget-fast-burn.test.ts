/**
 * Acceptance tests: fast-burn page carries build SHA and log link.
 *
 * Offline pin of docs/error-budget-alerts.md fast burn (12x over the 15 min
 * long window confirmed by the 2 min short window) with synthetic REST
 * window data. The rule, its window history, and its runbook section do not
 * exist yet, so these tests are red until the fast-burn page lands. The
 * builder must not edit this file: add the `error_budget_fast_burn` rule
 * (page, not ticket), the minimal Worker window history, the runbook anchor,
 * and the evidence-catalog entry, and keep these fixtures green unmodified.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE,
  LOG_QUERY_URL,
  RULES,
  evaluateMetrics,
  packetFilename,
  parseExposition,
  ruleFor,
  runbookUrl,
  transitionMessages,
} from "../src/alert-rules.ts";

const NOW = 2_000_000;
const BUILD_SHA = "abc123def456abc123def456abc123def456abcd";

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

function headingAnchors(text: string): Set<string> {
  return new Set(
    [...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
      m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-"),
    ),
  );
}

test("error-budget fast burn is a paged rule with a runbook anchor", () => {
  const rule = ruleFor("error_budget_fast_burn");
  assert.ok(rule, "missing error_budget_fast_burn rule");
  assert.equal(rule!.severity, undefined, "fast burn pages, never tickets");
  const slugs = headingAnchors(
    readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8"),
  );
  const anchor = rule!.runbook.split("#")[1]!;
  assert.ok(slugs.has(anchor), `missing runbook heading for #${anchor}`);
  assert.ok(
    RULES.some((candidate) => candidate.id === "error_budget_fast_burn"),
    "rule must be registered in RULES",
  );
});

test("error-budget fast-burn packet carries a stable evidence filename", () => {
  const window = "2026-10-17T00-00-00Z";
  assert.equal(
    packetFilename("error_budget_fast_burn", window),
    `evidence-error_budget_fast_burn-${window}.json`,
  );
});

test("forced fast burn pages with synthetic window data", () => {
  // 10,000 eligible events/hour: ~2,500 per 15 min, ~333 per 2 min. A forced
  // 12x burn holds 30 errors per 15 min and 4 per 2 min across the windows.
  // Feed 60 s scrapes for 20 min; the last 15 min carry the burn.
  let state = EMPTY_STATE;
  let total = 100_000;
  let errors = 1_000;
  let t = NOW - 20 * 60;
  state = evaluateMetrics(parseExposition(body(restLines(total, errors))), state, t).state;
  for (let minute = 1; minute <= 20; minute += 1) {
    // ~167 eligible events/min (~10k/hour); the last 15 min add 2 errors/min
    // (30 per 15 min = 12x) while the first 5 min stay near 1x.
    const burning = minute > 5;
    total += 167;
    errors += burning ? 2 : 0;
    t += 60;
    state = evaluateMetrics(parseExposition(body(restLines(total, errors))), state, t).state;
  }
  const firing = evaluateMetrics(parseExposition(body(restLines(total, errors))), state, t + 60);
  assert.ok(
    firing.firing.includes("error_budget_fast_burn"),
    `forced 12x burn over 15 min + 2 min must page, got: ${firing.firing.join(", ")}`,
  );
  const packet = transitionMessages([], firing.firing, {}, undefined, { buildRevision: BUILD_SHA })[0]!;
  assert.ok(packet.includes(BUILD_SHA), `page must carry build SHA, got: ${packet}`);
  assert.ok(packet.includes(LOG_QUERY_URL), `page must carry log-query URL, got: ${packet}`);
  assert.ok(packet.includes(runbookUrl(ruleFor("error_budget_fast_burn")!)), "page must link its runbook");
});

test("healthy burn stays silent", () => {
  // A fifth of budget (~0.2x): 1,000 eligible events with a single error.
  let state = EMPTY_STATE;
  let total = 50_000;
  let errors = 500;
  let t = NOW - 20 * 60;
  state = evaluateMetrics(parseExposition(body(restLines(total, errors))), state, t).state;
  for (let minute = 1; minute <= 20; minute += 1) {
    total += 50;
    t += 60;
    state = evaluateMetrics(parseExposition(body(restLines(total, errors))), state, t).state;
  }
  const healthy = evaluateMetrics(parseExposition(body(restLines(total, errors))), state, t + 60);
  assert.ok(
    !healthy.firing.includes("error_budget_fast_burn"),
    "healthy burn must not page",
  );
});
