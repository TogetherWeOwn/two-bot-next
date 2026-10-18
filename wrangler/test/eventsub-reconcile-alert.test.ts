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
 * Failed EventSub reconciliation passes raise one ticket.
 *
 * The Rust process records exactly one outcome per completed reconcile pass
 * (`two_bot_eventsub_reconcile_passes_total{outcome}`), alongside detailed
 * per-site error counters. The Worker observes only the pass outcome: one
 * failed-pass raise however many internal error counters moved, recovery
 * only from a later healthy pass, and no per-broadcaster/subscription/body
 * labels or subjects anywhere.
 */
const KEY = "eventsub_reconcile_failed";
const FAMILY = "two_bot_eventsub_reconcile_passes_total";

const passes = (healthy: number | string, pending: number | string, failed: number | string): string[] => [
  `${FAMILY}{outcome="healthy"} ${healthy}`,
  `${FAMILY}{outcome="pending"} ${pending}`,
  `${FAMILY}{outcome="failed"} ${failed}`,
];
/** Detailed reconcile error sites: present in the exposition, never alert inputs. */
const errors = (action: number, prior: number, unhealthy: number, failed: number): string[] => [
  `two_bot_eventsub_reconcile_errors_total{error_class="reconcile_action_failed"} ${action}`,
  `two_bot_eventsub_reconcile_errors_total{error_class="reconcile_prior_failed"} ${prior}`,
  `two_bot_eventsub_reconcile_errors_total{error_class="reconcile_unhealthy"} ${unhealthy}`,
  `two_bot_eventsub_reconcile_errors_total{error_class="reconcile_failed"} ${failed}`,
];
const ev = (lines: string[], state = EMPTY_STATE) => evaluateMetrics(parseExposition(lines.join("\n")), state, 1_000_000);

/** Baseline then one failed pass: the firing state most tests start from. */
function firingState(): MetricsAlertState {
  const baseline = ev(passes(10, 2, 7)).state;
  const fired = ev(passes(10, 2, 8), baseline);
  assert.deepEqual(fired.firing, [KEY]);
  return fired.state;
}

test("pass outcomes are optional-when-present and pinned to the fixed Rust outcomes", () => {
  const rust = readFileSync(new URL("../../crates/core/src/metrics.rs", import.meta.url), "utf8");
  const outcomes = /pub const EVENTSUB_RECONCILE_OUTCOMES: &\[&str\] = &\[([^\]]*)\];/.exec(rust);
  assert.ok(outcomes);
  const fixed = [...outcomes[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!);
  assert.deepEqual(fixed, ["healthy", "pending", "failed"]);
  // Optional, never required: an older container without the family must
  // still scrape (a required family would silence every rule on rollback).
  assert.equal(ALERT_INPUT_LABELS[FAMILY], undefined, "pass outcomes must not be a required scrape input");
  assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[FAMILY], { outcome: fixed });
  const rule = RULES.find((r) => r.id === KEY);
  assert.ok(rule, "missing rule eventsub_reconcile_failed");
  assert.equal(rule.severity, "ticket", "a failed reconcile pass tickets, never pages");
  assert.equal(rule.runbook, "runbook.md#alert-eventsub-reconcile-failed");
  assert.equal(packetFilename(KEY, "2026-10-09T20-11-06Z"), `evidence-${KEY}-2026-10-09T20-11-06Z.json`);
});

test("the first valid sample is only a baseline, even with failed passes already counted", () => {
  const first = ev(passes(10, 2, 7));
  assert.deepEqual(first.firing, []);
  assert.deepEqual(transitionMessages([], first.firing), []);
});

test("healthy-only and pending-only growth never fire", () => {
  let state = ev(passes(10, 2, 7)).state;
  state = ev(passes(11, 2, 7), state).state;
  assert.deepEqual(state.firing, [], "a healthy pass alone is not a failure");
  state = ev(passes(11, 5, 7), state).state;
  assert.deepEqual(state.firing, [], "verification-pending growth is not a failure");
  state = ev(passes(11, 5, 7), state).state;
  assert.deepEqual(state.firing, [], "flat counters stay quiet");
});

test("one failed pass raises exactly one ticket transition, however many error sites moved", () => {
  const baseline = ev([...passes(10, 2, 7), ...errors(3, 1, 2, 1)]).state;
  const fired = ev([...passes(10, 2, 8), ...errors(9, 2, 3, 4)], baseline);
  assert.deepEqual(fired.firing, [KEY]);
  const messages = transitionMessages(baseline.firing, fired.firing);
  assert.equal(messages.length, 1, "detailed error counters must not multiply the ticket");
  assert.match(messages[0]!, new RegExp(`ALERT ${KEY} \\(ticket\\):`));
  assert.equal(ruleFor(KEY)?.severity, "ticket");
});

test("repeated failed passes stay firing without re-alerting", () => {
  let state = firingState();
  for (const failedCount of [9, 10, 25]) {
    const next = ev(passes(10, 2, failedCount), state);
    assert.deepEqual(next.firing, [KEY]);
    assert.deepEqual(transitionMessages(state.firing, next.firing), [], "no second ALERT for the same incident");
    state = next.state;
  }
});

test("a flat failed counter and pending-only growth do not recover a firing ticket", () => {
  let state = firingState();
  for (const lines of [passes(10, 2, 8), passes(10, 2, 8), passes(10, 6, 8)]) {
    const next = ev(lines, state);
    assert.deepEqual(next.firing, [KEY], "no healthy pass yet: still firing");
    assert.deepEqual(transitionMessages(state.firing, next.firing), []);
    state = next.state;
  }
});

test("a healthy-pass increase with no failed-pass increase recovers exactly once", () => {
  const firing = firingState();
  const recovered = ev(passes(11, 2, 8), firing);
  assert.deepEqual(recovered.firing, []);
  const messages = transitionMessages(firing.firing, recovered.firing);
  assert.equal(messages.length, 1);
  assert.match(messages[0]!, new RegExp(`RESOLVED ${KEY}\\. Build: unknown\\. Logs: `));
  const quiet = ev(passes(12, 2, 8), recovered.state);
  assert.deepEqual(quiet.firing, []);
  assert.deepEqual(transitionMessages(recovered.firing, quiet.firing), [], "recovery is not repeated");
});

test("simultaneous healthy and failed deltas fire and never falsely recover", () => {
  const baseline = ev(passes(10, 2, 7)).state;
  const fired = ev(passes(11, 2, 8), baseline);
  assert.deepEqual(fired.firing, [KEY], "a failed delta in the window wins over the healthy one");
  const still = ev(passes(12, 2, 9), fired.state);
  assert.deepEqual(still.firing, [KEY], "healthy growth beside new failures is not recovery");
  assert.deepEqual(transitionMessages(fired.firing, still.firing), []);
});

test("counter resets rebaseline without fabricating events", () => {
  // Not firing: a restart sample fires nothing, and the next failed delta
  // measured from the new baseline fires once.
  let state = ev(passes(10, 2, 7)).state;
  const reset = ev(passes(0, 0, 0), state);
  assert.deepEqual(reset.firing, []);
  assert.deepEqual(transitionMessages(state.firing, reset.firing), []);
  state = ev(passes(0, 0, 1), reset.state).state;
  assert.deepEqual(state.firing, [KEY]);

  // Firing: the reset sample invents neither an ALERT nor a RESOLVED; the
  // ticket holds until a healthy pass lands in the new process.
  const firing = firingState();
  const held = ev(passes(1, 0, 0), firing);
  assert.deepEqual(held.firing, [KEY]);
  assert.deepEqual(transitionMessages(firing.firing, held.firing), []);
  const recovered = ev(passes(2, 0, 0), held.state);
  assert.deepEqual(recovered.firing, [], "the first post-restart healthy increase recovers");
});

test("a restart sample with failed passes already counted fires (failed startup pass)", () => {
  // Healthy went backwards, so this window is a rebaseline, not a delta —
  // but every count in the reset sample happened in the new process (the
  // Rust reconcile loop runs its startup pass before the first scrape), so
  // a failed count on the reset sample is a post-restart failure and fires.
  const state = ev(passes(10, 2, 7)).state;
  const restarted = ev(passes(3, 2, 99), state);
  assert.deepEqual(restarted.firing, [KEY], "the new process already counted failed passes");
  assert.deepEqual(ev(passes(3, 2, 100), restarted.state).firing, [KEY]);
});

test("a failed startup pass after a deploy tickets, then a healthy pass resolves it", () => {
  // The deploy/restart scenario: healthy baseline, then the new process
  // counts a failed startup pass before its first scrape.
  let state = ev(passes(25, 0, 0)).state;
  const restarted = ev(passes(0, 0, 1), state);
  assert.deepEqual(restarted.firing, [KEY], "the failed boot pass is ticketed, not swallowed");
  assert.deepEqual(transitionMessages(state.firing, restarted.firing).length, 1);
  const recovered = ev(passes(1, 0, 1), restarted.state);
  assert.deepEqual(recovered.firing, [], "the next healthy pass with no new failure resolves");
});

test("a restart sample with no failed passes holds instead of firing", () => {
  const state = ev(passes(10, 2, 7)).state;
  const restarted = ev(passes(3, 2, 0), state);
  assert.deepEqual(restarted.firing, [], "no failed pass in the new process: rebaseline only");
  assert.deepEqual(ev(passes(3, 2, 1), restarted.state).firing, [KEY]);
});

const invalidSamples: Record<string, string[]> = {
  "missing family (older container)": [],
  "missing one outcome": [`${FAMILY}{outcome="healthy"} 11`, `${FAMILY}{outcome="failed"} 8`],
  "NaN": passes("NaN", 2, 8),
  "infinity": passes(10, 2, "+Inf"),
  "negative": passes(10, -1, 8),
  "fractional": passes(10, 2, 8.5),
  "unsafe integer": passes(10, 2, "9007199254740992"),
  "duplicate outcome": [...passes(10, 2, 8), `${FAMILY}{outcome="failed"} 8`],
  "broadcaster label instead of a plain outcome": [
    `${FAMILY}{outcome="healthy"} 11`,
    `${FAMILY}{outcome="pending"} 2`,
    `${FAMILY}{outcome="failed",broadcaster="synthetic-broadcaster"} 8`,
  ],
  "unlabeled family row only": [`${FAMILY} 8`],
};
for (const [name, lines] of Object.entries(invalidSamples)) {
  test(`missing/invalid sample preserves active state and baseline: ${name}`, () => {
    const firing = firingState();
    const held = ev(lines, firing);
    assert.deepEqual(held.firing, [KEY], "missing evidence never resolves the ticket");
    assert.deepEqual(transitionMessages(firing.firing, held.firing), []);
    // The pre-gap baseline survives: a healthy increase measured against it
    // (10 -> 11 healthy, failed still 8) is the recovery evidence.
    assert.deepEqual(ev(passes(11, 2, 8), held.state).firing, []);

    const baseline = ev(passes(10, 2, 7)).state;
    const quiet = ev(lines, baseline);
    assert.deepEqual(quiet.firing, [], "missing evidence never fires either");
    assert.deepEqual(ev(passes(10, 2, 8), quiet.state).firing, [KEY], "the next valid failed delta still fires");
  });
}

test("hostile or unknown outcome rows never become subjects or extra keys", () => {
  const baseline = ev([
    ...passes(10, 2, 7),
    `${FAMILY}{outcome="__proto__"} 5`,
    `${FAMILY}{outcome="synthetic-subscription-id"} 5`,
  ]).state;
  assert.deepEqual(baseline.firing, []);
  const fired = ev([
    ...passes(10, 2, 8),
    `${FAMILY}{outcome="__proto__"} 9`,
    `${FAMILY}{outcome="synthetic-subscription-id"} 9`,
  ], baseline);
  assert.deepEqual(fired.firing, [KEY], "only the fixed ticket key, never a label-derived subject");
  for (const key of fired.firing) assert.ok(!key.includes(":"), "no per-broadcaster or per-subscription subject");
});

test("state survives DO reconstruction (JSON round-trip) mid-incident", () => {
  let state = ev(passes(10, 2, 7)).state;
  state = JSON.parse(JSON.stringify(state)) as MetricsAlertState;
  const fired = ev(passes(10, 2, 8), state);
  assert.deepEqual(fired.firing, [KEY]);
  state = JSON.parse(JSON.stringify(fired.state)) as MetricsAlertState;
  assert.deepEqual(ev(passes(10, 3, 8), state).firing, [KEY], "pending growth after reload still holds");
  state = JSON.parse(JSON.stringify(ev(passes(10, 3, 8), state).state)) as MetricsAlertState;
  assert.deepEqual(ev(passes(11, 3, 8), state).firing, [], "healthy increase after reload recovers");
});

test("legacy DO storage written before this rule baselines on its first sample", () => {
  // The stored shape from before this rule existed (no reconcile fields):
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
  const first = ev(passes(4, 1, 3), legacy);
  assert.deepEqual(first.firing, [], "pre-existing failure counts are a baseline, not an incident");
  assert.deepEqual(ev(passes(4, 1, 4), first.state).firing, [KEY]);
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
  // No broadcaster, subscription id, body or free-text label ever validates.
  assert.equal(
    parseAlertScrape(metricsBody().replace(`${FAMILY}{outcome="failed"} 0`, `${FAMILY}{outcome="failed",broadcaster="synthetic"} 0`)),
    null,
    "an extra label on a pass row must fail the scrape",
  );
  assert.equal(
    parseAlertScrape(metricsBody().replace(`${FAMILY}{outcome="failed"} 0`, `${FAMILY}{} 0`)),
    null,
    "a pass row without its fixed outcome must fail the scrape",
  );
});

test("runbook and metrics docs state the trigger, recovery and read-only diagnosis", () => {
  const read = (file: string) => readFileSync(new URL(`../../docs/${file}`, import.meta.url), "utf8");
  const runbook = read("runbook.md");
  const heading = /^#+ .*$/gm;
  const sections = [...runbook.matchAll(heading)];
  const slug = (title: string) => title.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-");
  const start = sections.findIndex((m) => slug(m[0].replace(/^#+ /, "")) === "alert-eventsub-reconcile-failed");
  assert.ok(start >= 0, "runbook has no alert-eventsub-reconcile-failed section");
  const from = sections[start]!.index!;
  const to = sections[start + 1]?.index ?? runbook.length;
  const section = runbook.slice(from, to);
  assert.match(section, /failed/i);
  assert.match(section, /ticket/i, "the runbook says this tickets instead of paging");
  assert.match(section, /healthy/i, "the runbook states the healthy-pass recovery");
  assert.match(section, /flat|pending/i, "the runbook states what does not count as recovery");
  assert.match(section, /eventsub-subscription-health-queries\.md/, "diagnosis reuses the existing read-only queries doc");
  assert.match(section, /read-only/i, "the diagnosis path is read-only (no new Helix behavior)");

  const metrics = read("metrics.md");
  assert.ok(metrics.includes(`\`${KEY}\``), "docs/metrics.md lists the rule in its alert table");
  assert.ok(!metrics.includes("alert rules land separately"), "the deferred-alert wording is retired once the rule lands");
});
