import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE, RULES, evaluateMetrics, packetFilename, parseExposition,
  ruleFor, runbookUrl, transitionMessages,
  type MetricsAlertState,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody, metricsLines, rustLabels } from "./fixtures/metrics.ts";

// Acceptance contract: a verdict outage refusing every prefix command must
// ticket instead of staying invisible. The producer already emits
// two_bot_gateway_prefix_trigger_refused_total{reason="verdict"|"other"};
// these tests pin the missing alert consumer. Only reason="verdict" counts:
// "other" is normal refusal volume. Written against existing evaluator
// exports only, so the file typechecks before the rule exists and fails by
// assertion until it is built.

const RULE_ID = "prefix_trigger_verdict_refusals";
const FAMILY = "two_bot_gateway_prefix_trigger_refused_total";
const NOW = 1_000_000;

const verdictLine = (count: number | string) => `${FAMILY}{reason="verdict"} ${count}`;
const otherLine = (count: number | string) => `${FAMILY}{reason="other"} ${count}`;
const pair = (verdict: number | string, other: number | string = 0) => [verdictLine(verdict), otherLine(other)];
const ev = (lines: string[], state: MetricsAlertState = EMPTY_STATE) =>
  evaluateMetrics(parseExposition(lines.join("\n")), state, NOW);

/** Drive baseline + three rises so the rule is firing; returns that state. */
function firingState(): MetricsAlertState {
  let state = ev(pair(20)).state;
  for (const count of [21, 22, 23]) state = ev(pair(count), state).state;
  assert.deepEqual(state.firing, [RULE_ID]);
  return state;
}

function headingAnchors(text: string): Set<string> {
  return new Set([...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-")));
}

test("catalog: rule id matches the Rust evidence list, preserves append position, ticket severity", () => {
  const evidence = readFileSync(new URL("../../crates/core/src/evidence.rs", import.meta.url), "utf8");
  const rustIds = /pub const ALERT_RULE_IDS: \[&str; \d+\] = \[([\s\S]*?)\];/.exec(evidence);
  assert.ok(rustIds, "ALERT_RULE_IDS not found in crates/core/src/evidence.rs");
  assert.deepEqual(RULES.map((r) => r.id), [...rustIds[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!));
  // The 28-rule historical prefix is pinned by event-store-lag-alert.test.ts.
  // Keep both append positions stable while allowing future rules after them.
  assert.equal(RULES[27]?.id, "event_store_consumer_lag");
  assert.equal(RULES[28]?.id, RULE_ID, "prefix refusals append after the existing 28 rules without displacing them");
  assert.equal(RULES.filter((r) => r.id === RULE_ID).length, 1, "prefix refusals are catalogued exactly once");
  const rule = ruleFor(RULE_ID);
  assert.ok(rule, `ruleFor(${RULE_ID}) must resolve`);
  assert.equal(rule.severity, "ticket", "sustained verdict refusals ticket, never page");
  assert.equal(
    packetFilename(RULE_ID, "2026-10-09T20-11-06Z"),
    `evidence-${RULE_ID}-2026-10-09T20-11-06Z.json`,
  );
});

test("runbook and metrics docs name the rule and its anchor", () => {
  const rule = ruleFor(RULE_ID);
  assert.ok(rule);
  assert.equal(rule.runbook.split("#")[1], "alert-prefix-trigger-verdict-refusals");
  const runbook = readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8");
  assert.ok(headingAnchors(runbook).has("alert-prefix-trigger-verdict-refusals"),
    "docs/runbook.md needs an 'Alert: prefix trigger verdict refusals' heading");
  const section = runbook.slice(runbook.indexOf("Alert: prefix trigger verdict refusals"));
  assert.match(section, /verdict/, "runbook section distinguishes a verdict outage from normal refusal volume");
  const metrics = readFileSync(new URL("../../docs/metrics.md", import.meta.url), "utf8");
  assert.ok(metrics.includes(`\`${RULE_ID}\``), "docs/metrics.md must name the alert rule for this family");
});

test("scrape validation: family is optional-when-present with the fixed Rust reason enum", () => {
  assert.deepEqual(rustLabels("PREFIX_TRIGGER_REFUSED_REASONS"), ["verdict", "other"]);
  assert.equal(Object.hasOwn(ALERT_INPUT_LABELS, FAMILY), false,
    "older containers without the family must still scrape: not a required input");
  assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[FAMILY], { reason: ["verdict", "other"] });
  const full = metricsBody();
  assert.ok(full.includes(`${FAMILY}{reason="verdict"} 0`), "fixture emits the verdict series");
  assert.ok(parseAlertScrape(full), "current exposition parses");
  const older = metricsLines().filter((line) => !line.startsWith(FAMILY)).join("\n");
  assert.ok(parseAlertScrape(older), "older exposition without the family still parses");
  const nanValue = full.replace(`${FAMILY}{reason="verdict"} 0`, `${FAMILY}{reason="verdict"} NaN`);
  assert.equal(parseAlertScrape(nanValue), null, "a present-but-nonfinite verdict row fails the scrape");
});

test("first valid sample is baseline only, even with a large nonzero counter", () => {
  assert.deepEqual(ev(pair(0)).firing, []);
  assert.deepEqual(ev(pair(10_000)).firing, []);
  // The verdict row alone is a valid sample for this rule; "other" is ignored.
  assert.deepEqual(ev([verdictLine(10_000)]).firing, []);
});

test("three consecutive rising verdict samples ticket exactly once; one or two stay quiet", () => {
  let state = ev(pair(100)).state;
  assert.deepEqual(state.firing, [], "baseline never fires");
  for (const [count, expected] of [[101, []], [102, []], [103, [RULE_ID]]] as const) {
    const result = ev(pair(count), state);
    assert.deepEqual(result.firing, expected, `sample ${count}`);
    state = result.state;
  }
  const fire = transitionMessages([], state.firing);
  assert.equal(fire.length, 1, "one ticket transition, not one per rise");
  assert.match(fire[0]!, new RegExp(`ALERT ${RULE_ID} \\(ticket\\):`));
  assert.ok(fire[0]!.includes(runbookUrl(ruleFor(RULE_ID)!)), "ticket carries the runbook deep link");
  // Continued rises hold the ticket without re-notifying.
  const rising = ev(pair(104), state);
  assert.deepEqual(rising.firing, [RULE_ID]);
  assert.deepEqual(transitionMessages(state.firing, rising.firing), []);
  // A single flat valid sample then resolves exactly once.
  const flat = ev(pair(104), rising.state);
  assert.deepEqual(flat.firing, []);
  const resolved = transitionMessages(rising.firing, flat.firing);
  assert.equal(resolved.length, 1);
  assert.match(resolved[0]!, new RegExp(`RESOLVED ${RULE_ID}\\. Build: unknown\\. Logs: `));
  assert.deepEqual(transitionMessages(flat.firing, ev(pair(104), flat.state).firing), []);
});

test("an isolated refusal and a single large burst stay quiet", () => {
  let state = ev(pair(0)).state;
  state = ev(pair(1), state).state;
  assert.deepEqual(state.firing, [], "one refusal is normal volume");
  state = ev(pair(1), state).state;
  assert.deepEqual(state.firing, []);
  let burst = ev(pair(0)).state;
  burst = ev(pair(1000), burst).state;
  assert.deepEqual(burst.firing, [], "one rising window is not sustained, however large");
  burst = ev(pair(2000), burst).state;
  assert.deepEqual(burst.firing, [], "two rising windows are not sustained either");
});

test("flat windows break the streak: three fresh rises are needed after a flat sample", () => {
  let state = ev(pair(0)).state;
  state = ev(pair(1), state).state;
  state = ev(pair(2), state).state;
  state = ev(pair(2), state).state;
  assert.deepEqual(state.firing, []);
  state = ev(pair(3), state).state;
  state = ev(pair(4), state).state;
  assert.deepEqual(state.firing, [], "only two rises since the flat window");
  assert.deepEqual(ev(pair(5), state).firing, [RULE_ID]);
});

test("a counter reset re-baselines, resolves a firing ticket, and needs three fresh rises", () => {
  const before = firingState();
  const reset = ev(pair(0), before);
  assert.deepEqual(reset.firing, [], "a backwards counter is a restart: no window, ticket resolves");
  assert.match(
    transitionMessages(before.firing, reset.firing)[0]!,
    new RegExp(`RESOLVED ${RULE_ID}\\. Build: unknown\\. Logs: `),
  );
  let state = reset.state;
  for (const n of [1, 2]) {
    state = ev(pair(n), state).state;
    assert.deepEqual(state.firing, [], `post-reset rise ${n} stays quiet`);
  }
  assert.deepEqual(ev(pair(3), state).firing, [RULE_ID]);
  // A reset mid-streak also discards the partial streak without firing.
  let partial = ev(pair(50)).state;
  partial = ev(pair(51), partial).state;
  partial = ev(pair(52), partial).state;
  partial = ev(pair(0), partial).state;
  partial = ev(pair(1), partial).state;
  assert.deepEqual(ev(pair(2), partial).firing, [], "the pre-reset rises do not count");
});

test("only reason=verdict counts: reason=other rises never ticket or advance the streak", () => {
  let state = ev(pair(0, 0)).state;
  for (const n of [1, 2, 3, 4, 5]) {
    state = ev(pair(0, n), state).state;
    assert.deepEqual(state.firing, [], `other=${n} with a flat verdict series stays quiet`);
  }
  // Verdict rises interleaved with "other" noise still need three verdict rises.
  state = ev(pair(0, 100)).state;
  state = ev(pair(1, 200), state).state;
  state = ev(pair(2, 300), state).state;
  assert.deepEqual(state.firing, []);
  assert.deepEqual(ev(pair(3, 400), state).firing, [RULE_ID]);
});

test("subjects stay fixed: unknown reasons and hostile labels never fire or enter messages", () => {
  const hostile = [
    `${FAMILY}{reason="member-123"} 10`,
    `${FAMILY}{reason="__proto__"} 10`,
    `${FAMILY}{reason="constructor"} 10`,
    `${FAMILY}{reason="Verdict"} 10`,
    `${FAMILY}{reason="https://example.invalid/member/123?body=synthetic"} 10`,
    `${FAMILY} 10`,
  ];
  let state = EMPTY_STATE;
  for (let i = 0; i < 5; i++) {
    const result = ev(hostile, state);
    assert.deepEqual(result.firing.filter((k) => k.startsWith(RULE_ID)), []);
    state = result.state;
  }
  for (const message of transitionMessages([], state.firing)) {
    assert.doesNotMatch(message, /example\.invalid|member\/123|body=synthetic|member-123/);
  }
});

const invalidSamples: Record<string, string[]> = {
  "absent family": [],
  "missing verdict row": [otherLine(24)],
  "NaN": [verdictLine("NaN"), otherLine(0)],
  "infinity": [verdictLine("+Inf"), otherLine(0)],
  "negative": [verdictLine(-1), otherLine(0)],
  "fractional": [verdictLine(1.5), otherLine(0)],
  "unsafe integer": [verdictLine("9007199254740992"), otherLine(0)],
  "duplicate verdict row": [verdictLine(24), verdictLine(24), otherLine(0)],
  "extra label": [`${FAMILY}{reason="verdict",member="synthetic-member"} 24`, otherLine(0)],
};
for (const [name, lines] of Object.entries(invalidSamples)) {
  test(`invalid sample (${name}) breaks growth but never falsely resolves`, () => {
    // Mid-streak: the gap discards the partial streak; the return sample is a
    // new baseline and three fresh rises are required.
    let partial = ev(pair(20)).state;
    for (const n of [21, 22]) partial = ev(pair(n), partial).state;
    partial = ev(lines, partial).state;
    assert.deepEqual(partial.firing, []);
    let fresh = ev(pair(30), partial).state;
    assert.deepEqual(fresh.firing, [], "first valid sample after a gap is baseline only");
    for (const n of [31, 32]) {
      fresh = ev(pair(n), fresh).state;
      assert.deepEqual(fresh.firing, [], `rise to ${n} after the gap stays quiet`);
    }
    assert.deepEqual(ev(pair(33), fresh).firing, [RULE_ID]);

    // While firing: the gap holds the ticket with no RESOLVED message. The
    // return sample re-baselines (not a quiet window), ongoing growth holds,
    // and only a later flat valid sample resolves.
    const before = firingState();
    const unknown = ev(lines, before);
    assert.deepEqual(unknown.firing, [RULE_ID], "missing evidence must not clear the ticket");
    assert.deepEqual(transitionMessages(before.firing, unknown.firing), []);
    const returnBaseline = ev(pair(30), unknown.state);
    assert.deepEqual(returnBaseline.firing, [RULE_ID], "a new baseline is not a quiet window");
    const stillGrowing = ev(pair(31), returnBaseline.state);
    assert.deepEqual(stillGrowing.firing, [RULE_ID], "ongoing growth must not falsely resolve after a gap");
    const quiet = ev(pair(31), stillGrowing.state);
    assert.deepEqual(quiet.firing, []);
    assert.match(
      transitionMessages(stillGrowing.firing, quiet.firing)[0]!,
      new RegExp(`RESOLVED ${RULE_ID}\\. Build: unknown\\. Logs: `),
    );
  });
}

test("state survives DO reconstruction and reads legacy storage without the new fields", () => {
  // A mid-streak state round-tripped through JSON keeps counting.
  let state = ev(pair(200)).state;
  state = ev(pair(201), JSON.parse(JSON.stringify(state))).state;
  state = ev(pair(202), JSON.parse(JSON.stringify(state))).state;
  assert.deepEqual(state.firing, []);
  assert.deepEqual(ev(pair(203), JSON.parse(JSON.stringify(state))).firing, [RULE_ID]);

  // Storage written before this rule existed has none of its fields: the
  // first sample is a baseline, never a fabricated multi-window rise.
  const legacy = {
    firing: [] as string[], rest429: 0, restTotal: 0, poolStreak: 0, dbErrors: 0,
    sendBlocked: 0, sendBlockedStreak: 0, voiceOps: 0, voiceFailures: 0,
    voiceDeadLetters: 0, voiceOrphans: 0, gatewayMissed: 0, gatewayMissedSeen: false,
    receiverRefusals: {}, receiverRefusalsSeen: false, receiverRefusalStreaks: {},
  } as MetricsAlertState;
  const first = ev(pair(7), legacy);
  assert.deepEqual(first.firing, []);
  const second = ev(pair(8), first.state);
  assert.deepEqual(second.firing, []);
  const third = ev(pair(9), second.state);
  assert.deepEqual(third.firing, []);
  assert.deepEqual(ev(pair(10), third.state).firing, [RULE_ID]);
});

test("the new rule does not disturb unrelated rules sharing the same scrape", () => {
  const lines = (n: number) => [
    ...pair(n),
    `two_bot_job_consecutive_failures{job="counter"} 3`,
    `two_bot_send_admissions_total{outcome="blocked"} 0`,
  ];
  let state = ev(lines(0)).state;
  assert.deepEqual(state.firing, ["job_consecutive_failures:counter"]);
  state = ev(lines(1), state).state;
  state = ev(lines(2), state).state;
  assert.deepEqual(state.firing, ["job_consecutive_failures:counter"],
    "two verdict rises add nothing beside the unrelated firing rule");
  const fired = ev(lines(3), state);
  assert.deepEqual([...fired.firing].sort(), ["job_consecutive_failures:counter", RULE_ID].sort());
});
