import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE, RULES, evaluateMetrics, evaluatePromotionGate, packetFilename,
  parseExposition, ruleFor, transitionMessages,
  type MetricsAlertState,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody } from "./fixtures/metrics.ts";

/**
 * Acceptance contract: report sustained REST admission breaker openings.
 *
 * The Rust process guard already exports the label-free 0/1 gauge
 * `two_bot_rest_guard_breaker_open`. Local admission refusals happen before
 * any REST wire attempt, so the REST request counter (and the 429/5xx rules
 * over it) never sees them. This file pins the missing notification consumer:
 * a ticket after three consecutive valid open samples, recovery on one valid
 * closed sample, and no manufactured evidence from missing/invalid samples.
 * It changes no guard threshold, token, retry or backoff behaviour.
 */

const RULE_ID = "rest_admission_breaker_open";
const GAUGE = "two_bot_rest_guard_breaker_open";
const NOW = 1_000_000;

const open = `${GAUGE} 1`;
const closed = `${GAUGE} 0`;
const ev = (lines: string[], state: MetricsAlertState = EMPTY_STATE) =>
  evaluateMetrics(parseExposition(lines.join("\n")), state, NOW);

/** Feed one sample per entry, threading evaluator state like the DO does. */
function run(samples: string[][], state: MetricsAlertState = EMPTY_STATE) {
  let result = ev(samples[0] ?? [], state);
  for (const lines of samples.slice(1)) result = ev(lines, result.state);
  return result;
}

function firingState(): MetricsAlertState {
  const state = run([[open], [open], [open]]).state;
  assert.deepEqual(state.firing, [RULE_ID], "three consecutive open samples must be firing");
  return state;
}

test("breaker rule has one fixed ticket identity in every catalog", () => {
  const matches = RULES.filter((r) => r.id === RULE_ID);
  assert.equal(matches.length, 1, "exactly one rule with the fixed id");
  const rule = matches[0]!;
  assert.equal(rule.severity, "ticket", "sustained breaker opening is a ticket, never a page");
  assert.equal(rule.runbook, "runbook.md#alert-rest-admission-breaker-open");
  assert.match(rule.summary, /breaker/i);
  assert.match(rule.summary, /3 consecutive samples/);
  assert.equal(ruleFor(RULE_ID), rule);
  assert.equal(packetFilename(RULE_ID, "2026-10-09T20-11-06Z"),
    `evidence-${RULE_ID}-2026-10-09T20-11-06Z.json`);
  // The Rust evidence catalog is the single shared spelling for rule ids.
  const evidence = readFileSync(new URL("../../crates/core/src/evidence.rs", import.meta.url), "utf8");
  const catalog = /pub const ALERT_RULE_IDS: \[&str; \d+\] = \[([\s\S]*?)\];/.exec(evidence);
  assert.ok(catalog, "Rust ALERT_RULE_IDS catalog must be readable");
  assert.deepEqual(RULES.map((r) => r.id),
    [...catalog[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!),
    "Worker RULES and Rust ALERT_RULE_IDS must stay one shared list");
  // A ticket about local admission must never fail a promotion stage.
  assert.deepEqual(evaluatePromotionGate([RULE_ID]), { fail: false, reasons: [] });
});

test("sustained open tickets on the third consecutive valid sample; brief openings stay quiet", () => {
  assert.deepEqual(run([[open]]).firing, []);
  assert.deepEqual(run([[open], [open]]).firing, [], "two open samples are a brief opening");
  const fired = run([[open], [open], [open]]);
  assert.deepEqual(fired.firing, [RULE_ID]);
  // One ticket transition, carrying the shared runbook/log/build context.
  const before = run([[open], [open]]);
  const messages = transitionMessages(before.firing, fired.firing, {}, {}, { buildRevision: "abc123" });
  assert.equal(messages.length, 1);
  assert.match(messages[0]!, new RegExp(`ALERT ${RULE_ID} \\(ticket\\):`));
  assert.match(messages[0]!, /runbook\.md#alert-rest-admission-breaker-open/);
  assert.match(messages[0]!, /Build: abc123\. Logs: /);
  // A valid closed sample resets the streak: brief, closed, brief stays quiet.
  assert.deepEqual(run([[open], [open], [closed], [open], [open]]).firing, []);
  assert.deepEqual(run([[open], [open], [closed], [open], [open], [open]]).firing, [RULE_ID]);
  // Closed from the start never fires, however often it is sampled.
  assert.deepEqual(run([[closed], [closed], [closed], [closed]]).firing, []);
});

test("firing breaker emits one transition and one valid closed sample recovers once", () => {
  const firing = firingState();
  const stillOpen = ev([open], firing);
  assert.deepEqual(stillOpen.firing, [RULE_ID]);
  assert.deepEqual(transitionMessages(firing.firing, stillOpen.firing), [],
    "further open samples must not re-notify");
  const recovered = ev([closed], stillOpen.state);
  assert.deepEqual(recovered.firing, []);
  const resolutions = transitionMessages(stillOpen.firing, recovered.firing);
  assert.equal(resolutions.length, 1);
  assert.match(resolutions[0]!, new RegExp(`RESOLVED ${RULE_ID}\\. Build: unknown\\. Logs: `));
  const quiet = ev([closed], recovered.state);
  assert.deepEqual(transitionMessages(recovered.firing, quiet.firing), [],
    "recovery is reported once, not per closed sample");
  // After recovery a new opening needs a fresh sustained run.
  assert.deepEqual(run([[open], [open]], quiet.state).firing, []);
  assert.deepEqual(run([[open], [open], [open]], quiet.state).firing, [RULE_ID]);
});

const invalidSamples: Record<string, string[]> = {
  "missing family": [],
  "NaN": [`${GAUGE} NaN`],
  "infinity": [`${GAUGE} +Inf`],
  "negative": [`${GAUGE} -1`],
  "non-boolean 2": [`${GAUGE} 2`],
  "fractional": [`${GAUGE} 0.5`],
  "duplicate open rows": [open, open],
  "duplicate conflicting rows": [open, closed],
  "labeled row": [`${GAUGE}{route="other"} 1`],
};
for (const [name, lines] of Object.entries(invalidSamples)) {
  test(`breaker ${name} sample manufactures neither sustained evidence nor recovery`, () => {
    // Before firing: the invalid sample breaks the streak, so sustained
    // evidence must be three fresh consecutive valid open samples.
    const partial = run([[open], [open], lines]);
    assert.deepEqual(partial.firing, []);
    assert.deepEqual(run([[open]], partial.state).firing, [],
      "one open sample after an invalid gap is not a third consecutive sample");
    assert.deepEqual(run([[open], [open]], partial.state).firing, []);
    assert.deepEqual(run([[open], [open], [open]], partial.state).firing, [RULE_ID]);
    // While firing: the invalid sample holds the ticket without a transition;
    // only a valid closed sample recovers it.
    const firing = firingState();
    const held = ev(lines, firing);
    assert.deepEqual(held.firing, [RULE_ID]);
    assert.deepEqual(transitionMessages(firing.firing, held.firing), []);
    assert.deepEqual(ev([open], held.state).firing, [RULE_ID],
      "a valid open sample after the gap keeps the ticket without re-firing evidence");
    assert.deepEqual(ev([closed], held.state).firing, [],
      "a valid closed sample after the gap recovers once");
  });
}

test("absent older-container family retains all existing alert evaluation", () => {
  // No breaker series at all: other rules evaluate exactly as today.
  assert.deepEqual(ev([`two_bot_job_consecutive_failures{job="counter"} 3`]).firing,
    ["job_consecutive_failures:counter"]);
  assert.deepEqual(run([[], [], [], [], []]).firing, [],
    "absence alone never creates breaker evidence");
  // Closed breaker alongside another rule: only the other rule fires.
  assert.deepEqual(ev([closed, `two_bot_job_consecutive_failures{job="counter"} 3`]).firing,
    ["job_consecutive_failures:counter"]);
  // Sustained breaker alongside another rule: both fire, neither hides the other.
  const both = run([
    [open, `two_bot_job_consecutive_failures{job="counter"} 3`],
    [open, `two_bot_job_consecutive_failures{job="counter"} 3`],
    [open, `two_bot_job_consecutive_failures{job="counter"} 3`],
  ]);
  assert.deepEqual([...both.firing].sort(), [RULE_ID, "job_consecutive_failures:counter"].sort());
  // Normal global pauses are healthy backpressure, not a breaker opening.
  assert.deepEqual(run([
    ["two_bot_rest_guard_global_paused 1"],
    ["two_bot_rest_guard_global_paused 1"],
    ["two_bot_rest_guard_global_paused 1"],
  ]).firing, []);
});

test("scrape validates breaker_open as optional-when-present label-free 0/1", () => {
  assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[GAUGE], {},
    "the gauge is an optional input with no labels");
  assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, GAUGE), false,
    "the gauge must never become a required input (rollback containers lack it)");
  const valid = metricsBody(3);
  assert.ok(valid.includes(`${GAUGE} 0`), "fixture carries the closed gauge");
  assert.ok(parseAlertScrape(valid));
  assert.ok(parseAlertScrape(valid.replace(`${GAUGE} 0`, `${GAUGE} 1`)));
  // Older exposition without the family still scrapes and still evaluates
  // every pre-existing rule.
  const older = valid.split("\n").filter((line) => !line.startsWith(GAUGE)).join("\n");
  const olderSamples = parseAlertScrape(older);
  assert.ok(olderSamples, "rollback container without the gauge must still scrape");
  assert.ok(evaluateMetrics(olderSamples, EMPTY_STATE, NOW).firing
    .includes("job_consecutive_failures:rank"));
  // Present but malformed fails the whole scrape, like every validated input:
  // a bad gauge must never silently hold the rule off or on.
  for (const value of ["2", "-1", "0.5", "NaN", "+Inf"]) {
    assert.equal(parseAlertScrape(valid.replace(`${GAUGE} 0`, `${GAUGE} ${value}`)), null, value);
  }
  assert.equal(parseAlertScrape(valid.replace(`${GAUGE} 0`, `${GAUGE}{route="other"} 0`)), null,
    "a labeled row is not the label-free gauge");
  assert.equal(parseAlertScrape(`${valid}${GAUGE} 1\n`), null, "duplicate rows are rejected");
});

test("breaker state survives DO reconstruction, reads legacy storage and stays bounded", () => {
  // Legacy DO storage predates any breaker field: it must read as no streak,
  // never as a firing or pre-sustained state.
  const legacy = JSON.parse(JSON.stringify(EMPTY_STATE)) as Record<string, unknown>;
  for (const key of Object.keys(legacy)) if (/breaker/i.test(key)) delete legacy[key];
  let state = legacy as unknown as MetricsAlertState;
  for (const [index, expected] of [[1, []], [2, []], [3, [RULE_ID]]] as const) {
    const result = ev([open], JSON.parse(JSON.stringify(state)) as MetricsAlertState);
    assert.deepEqual(result.firing, expected, `open sample ${index} after reconstruction`);
    state = result.state;
  }
  // Reconstruction while firing keeps the ticket; a reconstructed closed
  // sample still recovers it.
  const rebuilt = JSON.parse(JSON.stringify(state)) as MetricsAlertState;
  assert.deepEqual(ev([open], rebuilt).firing, [RULE_ID]);
  assert.deepEqual(ev([closed], rebuilt).firing, []);
  // Bounded persisted state: once firing, repeated open samples change
  // nothing (the streak is capped, no history accumulates).
  const atFour = run([[open]], state).state;
  const atTwelve = run(Array.from({ length: 8 }, () => [open]), atFour).state;
  assert.deepEqual(JSON.parse(JSON.stringify(atTwelve)), JSON.parse(JSON.stringify(atFour)));
});

test("breaker gauge semantics ignore counter resets in other families", () => {
  const rest = (total: number) =>
    `two_bot_rest_requests_total{route="other",result="2xx"} ${total}`;
  // The REST counters go backwards (process restart) between samples. That
  // skips counter-window rules but must not touch gauge evidence: the same
  // gauge value 1 sampled three times is sustained, not a flat counter.
  const result = run([
    [open, rest(100)],
    [open, rest(5)],
    [open, rest(6)],
  ]);
  assert.deepEqual(result.firing, [RULE_ID]);
});

test("runbook and metrics docs explain the local-refusal gap and guard recovery", () => {
  const runbook = readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8");
  const heading = "#### Alert: REST admission breaker open";
  const start = runbook.indexOf(heading);
  assert.ok(start >= 0, "runbook must carry the alert section the rule links to");
  const section = runbook.slice(start, runbook.indexOf("\n#### ", start + heading.length));
  assert.match(section, /ticket/i);
  assert.match(section, new RegExp(GAUGE));
  // The diagnostic gap: local refusals precede any REST wire attempt, so the
  // request counter and its 429/5xx alerts cannot see this condition.
  assert.match(section, /before[^.]*\b(REST|HTTP|wire|request counter)\b/i);
  assert.match(section, /429|5xx|rest_requests/);
  // The existing guard's recovery: no reset endpoint or command; the breaker
  // closes when its rolling invalid-request window clears.
  assert.match(section, /no (manual )?(breaker[- ])?reset/i);
  assert.match(section, /rolling window|invalid[- ]request window|window (clears|expires)|closes (on its own|when)/i);

  const metrics = readFileSync(new URL("../../docs/metrics.md", import.meta.url), "utf8");
  assert.match(metrics, new RegExp(`\\| \`${RULE_ID}\`[^\\n]*ticket`, "i"),
    "metrics.md alert table must list the ticket rule");
  assert.match(metrics, new RegExp(`\`${RULE_ID}\`[^\\n]*3 consecutive`),
    "metrics.md must state the three-consecutive-samples contract");
});
