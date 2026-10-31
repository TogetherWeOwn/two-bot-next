import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE, evaluateMetrics, interruptPrefixVerdictRefusals, parseExposition,
  type MetricsAlertState,
} from "../src/alert-rules.ts";

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

test("failed scrapes reset the verdict baseline and streak but preserve a firing ticket", () => {
  // Production path: a rejected or timed-out scrape stores
  // interruptPrefixVerdictRefusals(previous). Baseline, two rises, a failed
  // scrape, then one rise must not fire: the rises were not consecutive.
  let partial = ev(pair(20)).state;
  partial = ev(pair(21), partial).state;
  partial = ev(pair(22), partial).state;
  assert.equal(partial.prefixVerdictStreak, 2);
  const interrupted = interruptPrefixVerdictRefusals({ ...partial, firing: [] });
  assert.ok(!("prefixVerdictStreak" in interrupted), "the failed scrape breaks consecutiveness");
  assert.ok(!("prefixVerdictSeen" in interrupted), "the failed scrape drops the baseline");
  let fresh = ev(pair(23), interrupted).state;
  assert.equal(fresh.prefixVerdictSeen, true);
  assert.equal(fresh.prefixVerdictStreak, 0, "first valid sample after the gap is baseline-only");
  fresh = ev(pair(24), fresh).state;
  assert.equal(fresh.prefixVerdictStreak, 1);
  fresh = ev(pair(25), fresh).state;
  assert.deepEqual(fresh.firing, [], "pre-gap rises do not carry across the gap");
  assert.deepEqual(ev(pair(26), fresh).firing, [RULE_ID]);

  // A firing ticket survives the failed scrape: the gap is not recovery.
  const before = firingState();
  const held = interruptPrefixVerdictRefusals(before);
  assert.deepEqual(held.firing, [RULE_ID], "a failed scrape is not recovery evidence");
  assert.ok(!("prefixVerdictStreak" in held));
  const rebaselined = ev(pair(24), held);
  assert.deepEqual(rebaselined.firing, [RULE_ID], "the return sample re-baselines and holds the ticket");
  assert.deepEqual(ev(pair(24), rebaselined.state).firing, [], "only an observed flat sample resolves");
});

test("verdict interrupt touches only verdict fields and is chained on the failed-scrape path", () => {
  const previous = { ...firingState(), firing: [RULE_ID, "job_stale:rank"], poolStreak: 2 };
  const interrupted = interruptPrefixVerdictRefusals(previous);
  const { prefixVerdictRefused: _refused, prefixVerdictStreak: _streak, prefixVerdictSeen: _seen, ...expected } = previous;
  assert.deepEqual(interrupted, expected);
  assert.equal(previous.prefixVerdictStreak, 3, "previous state is not mutated");
  assert.deepEqual(interruptPrefixVerdictRefusals(EMPTY_STATE), EMPTY_STATE,
    "legacy state without verdict fields passes through unchanged");
  const index = readFileSync(new URL("../src/index.ts", import.meta.url), "utf8");
  assert.ok(index.includes("interruptPrefixVerdictRefusals("),
    "the keepalive failed-scrape path must interrupt verdict-growth evidence, not just sibling rules");
});
