/**
 * Acceptance tests: ticket on sustained event-store consumer lag.
 *
 * Contract under test (written blind, before any implementation):
 * - New Worker alert rule `event_store_consumer_lag`, severity `ticket`,
 *   appended last to `RULES` and to the Rust `ALERT_RULE_IDS` list, with
 *   runbook anchor `runbook.md#alert-event-store-consumer-lag`.
 * - Input series `two_bot_event_store_consumer_lag_seconds`: the label-free
 *   f64 gauge from `crates/core/src/metrics.rs` (`0` when unobserved; the
 *   Rust setter already clamps non-finite/negative input to `0`). It is an
 *   optional-when-present scrape input, like the creator-map gauges, so a
 *   container old enough to lack it still scrapes. Fractional values are
 *   valid samples (it is a duration gauge, not a counter).
 * - Sustained means positive (`> 0`) in `EVENT_STORE_LAG_SAMPLES` (3)
 *   consecutive valid samples, matching the repo's other sustained rules
 *   (dispatch drops, pool saturation, receiver refusals). Magnitude and
 *   growth do not matter; any observed positive lag counts.
 * - `0`/unobserved never fires and is the only recovery proof: a valid `0`
 *   resets the streak and resolves a firing ticket (a process restart also
 *   returns the gauge to `0`, so restarts rebaseline the same way).
 * - Missing/invalid samples (absent family, duplicate, labeled, non-finite,
 *   negative) break the streak but never falsely resolve a firing ticket,
 *   and never let label values reach state, firing keys or messages.
 *
 * New symbols (`EVENT_STORE_LAG_SAMPLES`, the `eventStoreLagStreak` state
 * field) are read through untyped views so this file typechecks before the
 * implementation exists; the assertions, not the compiler, are the red.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as alertRules from "../src/alert-rules.ts";
import {
  EMPTY_STATE, RULES, evaluateMetrics, packetFilename, parseExposition,
  ruleFor, transitionMessages, type MetricsAlertState,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody, metricsLines } from "./fixtures/metrics.ts";

const RULE = "event_store_consumer_lag";
const SERIES = "two_bot_event_store_consumer_lag_seconds";
const line = (value: number | string) => `${SERIES} ${value}`;
const ev = (lines: string[], state: MetricsAlertState = EMPTY_STATE) =>
  evaluateMetrics(parseExposition(lines.join("\n")), state, 1_000_000);
const streakOf = (state: MetricsAlertState): unknown =>
  (state as unknown as Record<string, unknown>)["eventStoreLagStreak"];
const read = (file: string) => readFileSync(new URL(`../../${file}`, import.meta.url), "utf8");

/** Three consecutive positive samples: the smallest firing state. */
function firingState(): MetricsAlertState {
  let state = ev([line(30)]).state;
  state = ev([line(30)], state).state;
  const fired = ev([line(30)], state);
  assert.deepEqual(fired.firing, [RULE], "precondition: sustained lag fires");
  return fired.state;
}

test("rule catalog: lag rule is appended last, ticket severity, shared spelling with Rust", () => {
  const samples = (alertRules as unknown as Record<string, unknown>)["EVENT_STORE_LAG_SAMPLES"];
  assert.equal(samples, 3, "sustained means 3 consecutive valid samples, like the other sustained rules");
  const last = RULES[RULES.length - 1]!;
  assert.equal(last.id, RULE);
  assert.equal(last.severity, "ticket", "sustained lag tickets, never pages");
  assert.equal(last.runbook, "runbook.md#alert-event-store-consumer-lag");
  assert.match(last.summary, /lag/i);
  const rust = /pub const ALERT_RULE_IDS: \[&str; \d+\] = \[([\s\S]*?)\];/.exec(read("crates/core/src/evidence.rs"));
  assert.ok(rust);
  assert.deepEqual(RULES.map((r) => r.id), [...rust[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!));
  assert.equal(ruleFor(RULE)?.severity, "ticket");
  assert.equal(packetFilename(RULE, "2026-10-09T20-11-06Z"), `evidence-${RULE}-2026-10-09T20-11-06Z.json`);
});

test("runbook and metrics docs describe the lag ticket and its recovery", () => {
  const runbook = read("docs/runbook.md");
  assert.ok(runbook.includes("#### Alert: event-store consumer lag"), "runbook heading for #alert-event-store-consumer-lag");
  const metrics = read("docs/metrics.md");
  const seriesRow = metrics.split("\n").find((l) => l.startsWith(`| \`${SERIES}\` |`));
  assert.ok(seriesRow, "series row exists");
  assert.match(seriesRow, new RegExp(RULE), "series row names its alert rule");
  assert.doesNotMatch(seriesRow, /no alert rule consumes/, "series row no longer claims no rule consumes it");
  const alertRow = metrics.split("\n").find((l) => l.startsWith(`| \`${RULE}\` |`));
  assert.ok(alertRow, "alert-table row exists");
  assert.match(alertRow, /ticket/i);
  assert.ok(alertRow.includes("(runbook.md#alert-event-store-consumer-lag)"), "alert-table row links the runbook section");
});

test("scrape: lag series is optional-when-present and validated as a label-free duration", () => {
  assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[SERIES], {}, "registered optional with no labels");
  assert.equal(ALERT_INPUT_LABELS[SERIES], undefined, "never a required input: older containers still scrape");
  const body = metricsBody();
  assert.ok(body.includes(`${SERIES} 0`), "fixture emits the series");
  assert.ok(parseAlertScrape(body), "full body with the series parses");
  const older = metricsLines().filter((l) => !l.startsWith(SERIES)).join("\n");
  assert.ok(parseAlertScrape(older), "absent family still parses");
  const fractional = parseAlertScrape(body.replace(`${SERIES} 0`, `${SERIES} 1.5`));
  assert.ok(fractional, "fractional duration parses: the gauge is f64, not a counter");
  assert.equal(fractional.find((s) => s.name === SERIES)?.value, 1.5);
  for (const bad of ["NaN", "+Inf", "-Inf", "-1", "garbage"]) {
    assert.equal(parseAlertScrape(body.replace(`${SERIES} 0`, `${SERIES} ${bad}`)), null, `${bad} rejected`);
  }
  assert.equal(parseAlertScrape(body.replace(`${SERIES} 0`, `${SERIES}{channel="synthetic"} 5`)), null, "labeled sample rejected");
  assert.equal(parseAlertScrape(`${body}${SERIES} 5\n`), null, "duplicate sample rejected");
});

test("unobserved zero never fires, however long it lasts", () => {
  let state = EMPTY_STATE;
  for (let i = 0; i < 5; i++) {
    const result = ev([line(0)], state);
    assert.deepEqual(result.firing, [], "0 means unobserved, not lagging");
    state = result.state;
  }
  assert.equal(streakOf(state), 0);
});

test("absent family never fires", () => {
  let state = EMPTY_STATE;
  for (let i = 0; i < 5; i++) {
    const result = ev([], state);
    assert.deepEqual(result.firing, []);
    state = result.state;
  }
});

test("transient single-sample and two-sample lag stay quiet", () => {
  let state = ev([line(900)]).state;
  assert.deepEqual(state.firing, [], "one spike is not sustained");
  assert.equal(streakOf(state), 1);
  state = ev([line(900)], state).state;
  assert.deepEqual(state.firing, [], "two samples are not sustained yet");
  assert.equal(streakOf(state), 2);
  const recovered = ev([line(0)], state);
  assert.deepEqual(recovered.firing, []);
  assert.equal(streakOf(recovered.state), 0, "a valid zero resets the streak");
  assert.deepEqual(transitionMessages(state.firing, recovered.firing), [], "no transition noise for a transient");
});

test("sustained positive lag fires one ticket on the third consecutive valid sample", () => {
  let state = ev([line(45)]).state;
  assert.deepEqual(state.firing, [], "first positive sample does not fire");
  state = ev([line(45)], state).state;
  assert.deepEqual(state.firing, []);
  const third = ev([line(45)], state);
  assert.deepEqual(third.firing, [RULE], "exactly the lag ticket, no unrelated keys");
  assert.ok((streakOf(third.state) as number) >= 3);
  const fire = transitionMessages(state.firing, third.firing);
  assert.equal(fire.length, 1, "one transition message per firing");
  assert.match(fire[0]!, new RegExp(`ALERT ${RULE} \\(ticket\\):`));
  assert.ok(fire[0]!.includes("runbook.md#alert-event-store-consumer-lag"), "message links the runbook section");
  const fourth = ev([line(45)], third.state);
  assert.deepEqual(fourth.firing, [RULE], "ongoing lag keeps the ticket");
  assert.deepEqual(transitionMessages(third.firing, fourth.firing), [], "no duplicate alert while it keeps firing");
});

test("fractional positive lag counts: the bar is observed lag, not whole seconds", () => {
  let state = ev([line(0.5)]).state;
  state = ev([line(0.25)], state).state;
  assert.deepEqual(ev([line(1.5)], state).firing, [RULE]);
});

test("magnitude and direction do not matter, only sustained positivity", () => {
  let state = ev([line(3600)]).state;
  state = ev([line(60)], state).state;
  assert.deepEqual(ev([line(1)], state).firing, [RULE], "shrinking but still-positive lag is still sustained lag");
});

test("recovery: return to zero resolves once, then a fresh sustained run is required", () => {
  const firing = firingState();
  const recovered = ev([line(0)], firing);
  assert.deepEqual(recovered.firing, []);
  assert.equal(streakOf(recovered.state), 0);
  const resolved = transitionMessages(firing.firing, recovered.firing);
  assert.equal(resolved.length, 1);
  assert.match(resolved[0]!, new RegExp(`RESOLVED ${RULE}\\. Build: unknown\\. Logs: `));
  // Reset/rebaseline: after recovery (or a restart, whose gauge also reads
  // 0) one or two positive samples must not re-fire on the old streak.
  let state = recovered.state;
  state = ev([line(30)], state).state;
  state = ev([line(30)], state).state;
  assert.deepEqual(state.firing, [], "rebaselined: two positives after recovery stay quiet");
  assert.deepEqual(ev([line(30)], state).firing, [RULE], "the third consecutive positive fires again");
});

const invalidSamples: Record<string, string[]> = {
  "missing family": [],
  "NaN": [line("NaN")],
  "infinity": [line("+Inf")],
  "negative": [line(-1)],
  "duplicate": [line(30), line(30)],
  "labeled": [`${SERIES}{member="synthetic-member"} 30`],
};
for (const [name, lines] of Object.entries(invalidSamples)) {
  test(`invalid sample (${name}) breaks the streak but never falsely resolves`, () => {
    let partial = ev([line(30)]).state;
    partial = ev([line(30)], partial).state;
    assert.equal(streakOf(partial), 2);
    const interrupted = ev(lines, partial);
    assert.deepEqual(interrupted.firing, []);
    assert.equal(streakOf(interrupted.state), 0, "the gap breaks consecutiveness");
    let fresh = ev([line(30)], interrupted.state).state;
    assert.equal(streakOf(fresh), 1, "first valid sample after the gap restarts the streak");
    fresh = ev([line(30)], fresh).state;
    assert.deepEqual(fresh.firing, [], "pre-gap positives do not carry across the gap");
    assert.deepEqual(ev([line(30)], fresh).firing, [RULE]);

    const before = firingState();
    const unknown = ev(lines, before);
    assert.deepEqual(unknown.firing, [RULE], "missing/invalid data is not recovery evidence");
    assert.deepEqual(transitionMessages(before.firing, unknown.firing), []);
    const stillLagging = ev([line(30)], unknown.state);
    assert.deepEqual(stillLagging.firing, [RULE], "still-positive lag retains the ticket after a gap");
    const quiet = ev([line(0)], stillLagging.state);
    assert.deepEqual(quiet.firing, [], "only an observed zero resolves");
  });
}

test("state survives DO reconstruction and legacy storage without the streak field", () => {
  let state = ev([line(30)], JSON.parse(JSON.stringify(EMPTY_STATE)) as MetricsAlertState).state;
  state = ev([line(30)], JSON.parse(JSON.stringify(state)) as MetricsAlertState).state;
  assert.deepEqual(ev([line(30)], JSON.parse(JSON.stringify(state)) as MetricsAlertState).firing, [RULE]);
  // Legacy storage predates the field: it must read as a zero streak, so the
  // first positive sample after an upgrade never fires on its own.
  const legacy = { ...EMPTY_STATE } as MetricsAlertState;
  delete (legacy as unknown as Record<string, unknown>)["eventStoreLagStreak"];
  const first = ev([line(30)], legacy);
  assert.deepEqual(first.firing, []);
  assert.equal(streakOf(first.state), 1);
});

test("hostile labels never fire, enter state, or reach messages", () => {
  const hostile = [
    `${SERIES}{channel="synthetic-channel"} 600`,
    `${SERIES}{member="synthetic-member",content="synthetic-content"} 600`,
  ];
  let state = EMPTY_STATE;
  for (let i = 0; i < 5; i++) state = ev(hostile, state).state;
  assert.deepEqual(state.firing, []);
  assert.doesNotMatch(JSON.stringify(state), /synthetic-(channel|member|content)/);
  const firing = firingState();
  const held = ev(hostile, firing);
  assert.deepEqual(held.firing, [RULE], "labeled junk holds the ticket without resolving it");
  assert.doesNotMatch(JSON.stringify(held.state), /synthetic-(channel|member|content)/);
  assert.doesNotMatch(transitionMessages([], held.firing).join("\n"), /synthetic-(channel|member|content)/);
});
