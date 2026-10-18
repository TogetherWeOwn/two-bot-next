import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE, RULES, evaluateMetrics, packetFilename, parseExposition, ruleFor, transitionMessages,
  type MetricsAlertState,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody, metricsLines } from "./fixtures/metrics.ts";

/**
 * A sustained failed share of go-live announcement posts raises one ticket.
 *
 * The Rust receiver counts every announcement post under
 * `two_bot_eventsub_posts_total{outcome}` (attempted / succeeded / failed /
 * other). The Worker observes only the window between consecutive valid
 * samples, with the same pattern as `rest_5xx_rate`
 * (docs/metrics.md alert table: bad > 10% of the window, window >= 10):
 * failed delta > 10% of attempted delta with at least 10 attempted raises
 * one **ticket**; low-volume windows never raise. The first valid sample is
 * baseline only. Missing/invalid samples preserve state and baseline.
 * Resets rebaseline. Only a judged window (floor met) at or below the line
 * resolves: flat or sub-floor windows keep an active ticket so a sustained
 * outage stays one ALERT instead of flapping. No broadcaster, body, token
 * or free-text label ever becomes a subject or message content.
 *
 * Forwarding off is rule-agnostic sender behavior pinned by the metrics
 * sender harness in container.test.ts ("keeps monitoring durable and
 * log-only"); this file pins the single transition message that path logs
 * or forwards for this rule.
 */
const KEY = "eventsub_post_failed_share";
const FAMILY = "two_bot_eventsub_posts_total";

const posts = (
  attempted: number | string, succeeded: number | string, failed: number | string, other: number | string,
): string[] => [
  `${FAMILY}{outcome="attempted"} ${attempted}`,
  `${FAMILY}{outcome="succeeded"} ${succeeded}`,
  `${FAMILY}{outcome="failed"} ${failed}`,
  `${FAMILY}{outcome="other"} ${other}`,
];
const ev = (lines: string[], state = EMPTY_STATE) => evaluateMetrics(parseExposition(lines.join("\n")), state, 1_000_000);

/** Baseline, then a 20-attempt window in which every post failed. */
function firingState(): MetricsAlertState {
  const baseline = ev(posts(100, 90, 5, 5)).state;
  const fired = ev(posts(120, 90, 25, 5), baseline);
  assert.deepEqual(fired.firing, [KEY]);
  return fired.state;
}

test("post outcomes are optional-when-present and pinned to the fixed Rust outcomes", () => {
  const rust = readFileSync(new URL("../../crates/core/src/metrics.rs", import.meta.url), "utf8");
  const outcomes = /pub const EVENTSUB_POST_OUTCOMES:[^=]*= \[([^\]]*)\];/.exec(rust);
  assert.ok(outcomes);
  const fixed = [...outcomes[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!);
  assert.deepEqual(fixed, ["attempted", "succeeded", "failed", "other"]);
  // Optional, never required: an older container without the family must
  // still scrape (a required family would silence every rule on rollback).
  assert.equal(ALERT_INPUT_LABELS[FAMILY], undefined, "post outcomes must not be a required scrape input");
  assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[FAMILY], { outcome: fixed });
  const rule = RULES.find((r) => r.id === KEY);
  assert.ok(rule, "missing rule eventsub_post_failed_share");
  assert.equal(rule.severity, "ticket", "a failed delivery share tickets, never pages");
  assert.equal(rule.runbook, "runbook.md#alert-eventsub-post-failed-share");
  assert.equal(packetFilename(KEY, "2026-10-17T00-00-00Z"), `evidence-${KEY}-2026-10-17T00-00-00Z.json`);
});

test("the first valid sample is only a baseline, even with every post so far failed", () => {
  const first = ev(posts(1000, 0, 1000, 0));
  assert.deepEqual(first.firing, []);
  assert.deepEqual(transitionMessages([], first.firing), []);
});

test("sustained 100% failure raises exactly one ticket transition and never re-alerts", () => {
  const baseline = ev(posts(50, 50, 0, 0)).state;
  const fired = ev(posts(70, 50, 20, 0), baseline);
  assert.deepEqual(fired.firing, [KEY]);
  const messages = transitionMessages(baseline.firing, fired.firing);
  assert.equal(messages.length, 1, "one ticket for the incident, however long it runs");
  assert.match(messages[0]!, new RegExp(`ALERT ${KEY} \\(ticket\\):`));
  assert.equal(ruleFor(KEY)?.severity, "ticket");

  let state = fired.state;
  for (const lines of [posts(95, 50, 45, 0), posts(120, 50, 70, 0)]) {
    const next = ev(lines, state);
    assert.deepEqual(next.firing, [KEY], "still 100% failing: still firing");
    assert.deepEqual(transitionMessages(state.firing, next.firing), [], "no second ALERT for the same incident");
    state = next.state;
  }
});

test("the share line mirrors rest_5xx_rate: >10% failed fires, 10% and below stays quiet", () => {
  const baseline = ev(posts(100, 100, 0, 0)).state;
  // 5 of the next 100 attempted failed (5%): ordinary noise, no ticket.
  assert.deepEqual(ev(posts(200, 195, 5, 0), baseline).firing, []);
  // Exactly 10 of 100 is not over the >10% line.
  assert.deepEqual(ev(posts(200, 190, 10, 0), baseline).firing, []);
  // 20 of 100 (20%) is over the line.
  assert.deepEqual(ev(posts(200, 180, 20, 0), baseline).firing, [KEY]);
  // A fully healthy window never fires.
  assert.deepEqual(ev(posts(200, 200, 0, 0), baseline).firing, []);
});

test("low-volume windows stay quiet even when every post in them failed", () => {
  const baseline = ev(posts(100, 100, 0, 0)).state;
  // Fewer than 10 attempted in the window cannot raise, at any share.
  assert.deepEqual(ev(posts(105, 100, 5, 0), baseline).firing, [], "5 attempted, all failed: below the floor");
  assert.deepEqual(ev(posts(109, 100, 9, 0), baseline).firing, [], "9 attempted, all failed: below the floor");
  // At the floor the same total failure does raise.
  assert.deepEqual(ev(posts(110, 100, 10, 0), baseline).firing, [KEY], "10 attempted, all failed: at the floor");
});

test("the denominator is attempted posts, not the other outcomes", () => {
  const baseline = ev(posts(100, 90, 5, 5)).state;
  // 2 failed of 10 attempted is 20%: fires even though succeeded counters
  // from earlier attempts landed in the same window. An outcome-sum
  // denominator (2 of 102) would wrongly stay quiet.
  assert.deepEqual(ev(posts(110, 190, 7, 5), baseline).firing, [KEY]);
  // 5 failed of 100 attempted is 5%: quiet even though nothing succeeded.
  // A failed/(succeeded+failed) denominator (5 of 5) would wrongly fire.
  assert.deepEqual(ev(posts(200, 90, 10, 5), baseline).firing, []);
  // Growth in the `other` outcome is not a delivery failure.
  assert.deepEqual(ev(posts(120, 90, 5, 25), baseline).firing, []);
});

test("a healthy full-volume window recovers exactly once", () => {
  const firing = firingState();
  const recovered = ev(posts(140, 110, 25, 5), firing);
  assert.deepEqual(recovered.firing, []);
  const messages = transitionMessages(firing.firing, recovered.firing);
  assert.equal(messages.length, 1);
  assert.match(messages[0]!, new RegExp(`RESOLVED ${KEY}\\. Build: unknown\\. Logs: `));
  const quiet = ev(posts(160, 130, 25, 5), recovered.state);
  assert.deepEqual(quiet.firing, []);
  assert.deepEqual(transitionMessages(recovered.firing, quiet.firing), [], "recovery is not repeated");
});

test("flat or sub-floor windows hold an active ticket but never raise one", () => {
  const firing = firingState();
  // No new attempts: the ticket stays open until a judged window clears it.
  assert.deepEqual(ev(posts(120, 90, 25, 5), firing).firing, [KEY]);
  // Three new attempts, all failed, is a low-volume window: it holds the
  // ticket instead of resolving it one sample after it fired.
  const held = ev(posts(123, 90, 28, 5), firing);
  assert.deepEqual(held.firing, [KEY]);
  assert.deepEqual(transitionMessages(firing.firing, held.firing), [], "holding is not a new ALERT");
  // Not firing stays not firing through the same windows.
  const baseline = ev(posts(100, 90, 5, 5)).state;
  assert.deepEqual(ev(posts(100, 90, 5, 5), baseline).firing, []);
  assert.deepEqual(ev(posts(103, 90, 8, 5), baseline).firing, []);
});

test("one failed post per sample over 30 samples is one ALERT and zero RESOLVED", () => {
  // A sustained total outage at go-live volume: every window is sub-floor
  // until attempts accumulate to the 10-attempt floor, then the judged
  // window fires and later judged windows stay hot. The ticket must never
  // resolve mid-outage.
  let state = ev(posts(10, 9, 1, 0)).state;
  let prevFiring: string[] = [];
  let alerts = 0;
  let resolved = 0;
  for (let n = 11; n <= 40; n++) {
    const next = ev(posts(n, 9, n - 9, 0), state);
    for (const message of transitionMessages(prevFiring, next.firing)) {
      if (message.includes(`ALERT ${KEY} (ticket):`)) alerts += 1;
      if (message.includes(`RESOLVED ${KEY}`)) resolved += 1;
    }
    prevFiring = next.firing;
    state = next.state;
  }
  assert.deepEqual(state.firing, [KEY], "still failing at sample 30: still firing");
  assert.equal(alerts, 1, "one ALERT for the whole outage");
  assert.equal(resolved, 0, "no RESOLVED mid-outage");
});

test("counter resets rebaseline without fabricating a window", () => {
  // Not firing: a restart sample fires nothing, and the next hot delta
  // measured from the new baseline fires once.
  let state = ev(posts(100, 90, 5, 5)).state;
  const reset = ev(posts(0, 0, 0, 0), state);
  assert.deepEqual(reset.firing, []);
  assert.deepEqual(transitionMessages(state.firing, reset.firing), []);
  state = ev(posts(10, 0, 10, 0), reset.state).state;
  assert.deepEqual(state.firing, [KEY]);

  // Firing: the reset sample is a new process baseline, not a hot window;
  // a fresh hot delta after it raises again.
  const firing = firingState();
  const restarted = ev(posts(1, 1, 0, 0), firing);
  assert.deepEqual(restarted.firing, []);
  assert.deepEqual(ev(posts(21, 1, 20, 0), restarted.state).firing, [KEY]);
});

test("any backwards counter means restart: a hot-looking window across it does not fire", () => {
  const state = ev(posts(100, 90, 5, 5)).state;
  // succeeded went backwards, so this sample rebaselines even though the
  // attempted/failed deltas alone look like a 90% failure share.
  const restarted = ev(posts(150, 10, 50, 5), state);
  assert.deepEqual(restarted.firing, []);
  assert.deepEqual(ev(posts(170, 10, 70, 5), restarted.state).firing, [KEY]);
});

const invalidSamples: Record<string, string[]> = {
  "missing family (older container)": [],
  "missing one outcome": [
    `${FAMILY}{outcome="attempted"} 140`,
    `${FAMILY}{outcome="succeeded"} 110`,
    `${FAMILY}{outcome="failed"} 25`,
  ],
  "NaN": posts("NaN", 90, 25, 5),
  "infinity": posts(120, 90, "+Inf", 5),
  "negative": posts(120, -1, 25, 5),
  "fractional": posts(120, 90, 25.5, 5),
  "unsafe integer": posts(120, 90, "9007199254740992", 5),
  "duplicate outcome": [...posts(120, 90, 25, 5), `${FAMILY}{outcome="failed"} 25`],
  "broadcaster label instead of a plain outcome": [
    `${FAMILY}{outcome="attempted"} 120`,
    `${FAMILY}{outcome="succeeded"} 90`,
    `${FAMILY}{outcome="failed",broadcaster="synthetic-broadcaster"} 25`,
    `${FAMILY}{outcome="other"} 5`,
  ],
  "unlabeled family row only": [`${FAMILY} 25`],
};
for (const [name, lines] of Object.entries(invalidSamples)) {
  test(`missing/invalid sample preserves active state and baseline: ${name}`, () => {
    const firing = firingState();
    const held = ev(lines, firing);
    assert.deepEqual(held.firing, [KEY], "missing evidence never resolves the ticket");
    assert.deepEqual(transitionMessages(firing.firing, held.firing), []);
    // The pre-gap baseline survives: a healthy window measured against it
    // (120 -> 140 attempted, failed still 25) is the recovery evidence.
    assert.deepEqual(ev(posts(140, 110, 25, 5), held.state).firing, []);

    const baseline = ev(posts(100, 90, 5, 5)).state;
    const quiet = ev(lines, baseline);
    assert.deepEqual(quiet.firing, [], "missing evidence never fires either");
    assert.deepEqual(ev(posts(120, 90, 25, 5), quiet.state).firing, [KEY], "the next valid hot window still fires");
  });
}

test("hostile or unknown outcome rows never become subjects or extra keys", () => {
  const baseline = ev([
    ...posts(100, 90, 5, 5),
    `${FAMILY}{outcome="__proto__"} 5`,
    `${FAMILY}{outcome="synthetic-broadcaster-id"} 5`,
  ]).state;
  assert.deepEqual(baseline.firing, []);
  const fired = ev([
    ...posts(120, 90, 25, 5),
    `${FAMILY}{outcome="__proto__"} 9`,
    `${FAMILY}{outcome="synthetic-broadcaster-id"} 9`,
  ], baseline);
  assert.deepEqual(fired.firing, [KEY], "only the fixed ticket key, never a label-derived subject");
  for (const key of fired.firing) assert.ok(!key.includes(":"), "no per-broadcaster or per-stream subject");
  const message = transitionMessages(baseline.firing, fired.firing)[0]!;
  assert.doesNotMatch(message, /synthetic-broadcaster-id|__proto__/, "label values never reach alert text");
});

test("state survives DO reconstruction (JSON round-trip) mid-incident", () => {
  let state = ev(posts(100, 90, 5, 5)).state;
  state = JSON.parse(JSON.stringify(state)) as MetricsAlertState;
  const fired = ev(posts(120, 90, 25, 5), state);
  assert.deepEqual(fired.firing, [KEY]);
  state = JSON.parse(JSON.stringify(fired.state)) as MetricsAlertState;
  assert.deepEqual(ev(posts(140, 90, 45, 5), state).firing, [KEY], "a second hot window after reload still fires");
  state = JSON.parse(JSON.stringify(ev(posts(140, 90, 45, 5), state).state)) as MetricsAlertState;
  assert.deepEqual(ev(posts(160, 110, 45, 5), state).firing, [], "a healthy window after reload recovers");
});

test("legacy DO storage written before this rule baselines on its first sample", () => {
  // The stored shape from before this rule existed (no post-share fields):
  // the evaluator must treat it as no baseline, never fire on sight.
  const legacy = JSON.parse(JSON.stringify({
    firing: [], jobErrorClasses: {}, rest429: 0, rest5xx: 0, restTotal: 0, poolStreak: 0,
    dbErrors: 0, sendBlocked: 0, sendBlockedStreak: 0, voiceOps: 0, voiceFailures: 0,
    voiceDeadLetters: 0, voiceOrphans: 0, gatewayMissed: 0, gatewayMissedSeen: false,
    checkpointFailures: 0, checkpointFailuresSeen: false, receiverRefusals: {},
    receiverRefusalsSeen: false, receiverRefusalStreaks: {}, gatewayDisconnects: 0,
    gatewayRecoveries: 0, gatewayUnpairedSeen: false, gatewayUnpaired: 0,
    gatewayUnpairedStreak: 0, dispatchDrops: {}, dispatchDropStreaks: {},
  })) as MetricsAlertState;
  const first = ev(posts(500, 0, 500, 0), legacy);
  assert.deepEqual(first.firing, [], "pre-existing failure counts are a baseline, not an incident");
  assert.deepEqual(ev(posts(520, 0, 520, 0), first.state).firing, [KEY]);
});

test("scrape validation: absent family parses, present family is fully validated", () => {
  const older = metricsLines().filter((line) => !line.startsWith(FAMILY)).join("\n") + "\n";
  assert.ok(parseAlertScrape(older), "an older exposition without the family still scrapes");

  const samples = parseAlertScrape(
    metricsBody().replace(`${FAMILY}{outcome="failed"} 0`, `${FAMILY}{outcome="failed"} 6`),
  );
  assert.ok(samples);
  assert.equal(samples.find((s) => s.name === FAMILY && s.labels.outcome === "failed")?.value, 6);

  for (const value of ["NaN", "+Inf", "-1", "0.5", "1e20"]) {
    assert.equal(
      parseAlertScrape(metricsBody().replace(`${FAMILY}{outcome="failed"} 0`, `${FAMILY}{outcome="failed"} ${value}`)),
      null,
      `malformed present value must fail the scrape: ${value}`,
    );
  }
  // No broadcaster, body, token or free-text label ever validates.
  assert.equal(
    parseAlertScrape(metricsBody().replace(`${FAMILY}{outcome="failed"} 0`, `${FAMILY}{outcome="failed",broadcaster="synthetic"} 0`)),
    null,
    "an extra label on a post row must fail the scrape",
  );
  assert.equal(
    parseAlertScrape(metricsBody().replace(`${FAMILY}{outcome="failed"} 0`, `${FAMILY}{} 0`)),
    null,
    "a post row without its fixed outcome must fail the scrape",
  );
});

test("runbook and metrics docs state the trigger, recovery and read-only diagnosis", () => {
  const read = (file: string) => readFileSync(new URL(`../../docs/${file}`, import.meta.url), "utf8");
  const runbook = read("runbook.md");
  const heading = /^#+ .*$/gm;
  const sections = [...runbook.matchAll(heading)];
  const slug = (title: string) => title.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-");
  const start = sections.findIndex((m) => slug(m[0].replace(/^#+ /, "")) === "alert-eventsub-post-failed-share");
  assert.ok(start >= 0, "runbook has no alert-eventsub-post-failed-share section");
  const from = sections[start]!.index!;
  const to = sections[start + 1]?.index ?? runbook.length;
  const section = runbook.slice(from, to);
  assert.match(section, /failed/i);
  assert.match(section, /attempted/i, "the runbook states the share denominator");
  assert.match(section, /ticket/i, "the runbook says this tickets instead of paging");
  assert.match(section, /resolv|recover|healthy/i, "the runbook states what recovery looks like");
  assert.match(section, /read-only/i, "the diagnosis path is read-only (no posting or ledger writes)");
  assert.match(section, /(stream-funnel\.md|eventsub-runbook\.md)/, "diagnosis reuses an existing delivery doc");

  const metrics = read("metrics.md");
  assert.ok(metrics.includes(`\`${KEY}\``), "docs/metrics.md lists the rule in its alert table");
});
