import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  COMMUNITY_FACTS_DRAIN_REASONS, COMMUNITY_FACTS_DRAIN_SAMPLES, EMPTY_STATE, RULES,
  evaluateMetrics, interruptCommunityFactsDrain, parseExposition, ruleFor,
  transitionMessages, type MetricsAlertState,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsBody } from "./fixtures/metrics.ts";

const FAMILY = "two_bot_community_facts_drain_failures_total";
const KEY = "community_facts_drain_failures";
const line = (reason: string, count: number | string) => `${FAMILY}{reason="${reason}"} ${count}`;
const family = (error = 0, timeout = 0, other = 0) => [line("error", error), line("timeout", timeout), line("other", other)];
const ev = (lines: string[], state = EMPTY_STATE) => evaluateMetrics(parseExposition(lines.join("\n")), state, 1_000_000);
function firingState(): MetricsAlertState {
  let state = ev(family(20)).state;
  for (const count of [21, 22, 23]) state = ev(family(count), state).state;
  assert.deepEqual(state.firing, [KEY]);
  return state;
}

test("community-facts drain reasons match Rust, rule/evidence/doc pins agree, and the rule is a ticket", () => {
  const read = (file: string) => readFileSync(new URL(`../../${file}`, import.meta.url), "utf8");
  const reasons = /pub const COMMUNITY_FACTS_DRAIN_REASONS: &\[&str\] = &\[([\s\S]*?)\];/.exec(read("crates/core/src/metrics.rs"));
  assert.ok(reasons);
  assert.deepEqual(COMMUNITY_FACTS_DRAIN_REASONS, [...reasons[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!));
  assert.deepEqual(COMMUNITY_FACTS_DRAIN_REASONS, ["error", "timeout", "other"]);
  assert.equal(COMMUNITY_FACTS_DRAIN_SAMPLES, 3);
  assert.ok(RULES.map((r) => r.id).includes(KEY), "Worker RULES must carry the drain-failures rule");
  const evidence = /pub const ALERT_RULE_IDS: \[&str; \d+\] = \[([\s\S]*?)\];/.exec(read("crates/core/src/evidence.rs"));
  assert.ok(evidence);
  assert.ok([...evidence[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!).includes(KEY),
    "Rust ALERT_RULE_IDS must carry the same spelling");
  const rule = ruleFor(KEY);
  assert.ok(rule);
  assert.equal(rule.severity, "ticket", "sustained drain loss tickets, never pages");
  assert.equal(rule.runbook, "runbook.md#alert-community-facts-drain-failures");
  assert.match(rule.summary, /community-facts drain failures/);
  assert.ok(rule.summary.includes(`${COMMUNITY_FACTS_DRAIN_SAMPLES} consecutive`),
    "summary must state the consecutive-window bound");
  const runbook = read("docs/runbook.md");
  const section = runbook.split("#### Alert: community facts drain failures")[1]?.split("\n#### ")[0];
  assert.ok(section, "runbook must carry the drain-failures diagnostic section");
  assert.ok(section.includes(FAMILY), "runbook section must name the consumed counter family");
  assert.match(section, /ticket/i);
  const metricsRow = read("docs/metrics.md").split("\n").find((row) => row.includes(`\`${FAMILY}{reason}\``));
  assert.ok(metricsRow, "metrics contract row must exist");
  assert.ok(metricsRow.includes(`\`${KEY}\``), "metrics row must name the firing key");
  assert.match(metricsRow, /ticket/);
  assert.ok(metricsRow.includes("3 consecutive"), "metrics row must state the consecutive-window bound");
  assert.doesNotMatch(metricsRow, /Alert-threshold hook/, "stale hook-only claim must be replaced");
});

test("community-facts first sample is baseline only; three summed growth windows fire one ticket and flat resolves it once", () => {
  let result = ev(family(1000));
  assert.deepEqual(result.firing, [], "the first nonzero sample is only a baseline");
  assert.deepEqual(result.state.communityFactsDrainFailures, { error: 1000, timeout: 0, other: 0 });
  assert.equal(result.state.communityFactsDrainStreak, 0);
  result = ev(family(1001), result.state);
  assert.deepEqual(result.firing, []);
  assert.equal(result.state.communityFactsDrainStreak, 1);
  result = ev(family(1001, 1), result.state);
  assert.deepEqual(result.firing, [], "growth in a different fixed reason still only extends the summed streak");
  assert.equal(result.state.communityFactsDrainStreak, 2);
  result = ev(family(1001, 1, 1), result.state);
  assert.deepEqual(result.firing, [KEY], "summed error/timeout/other growth fires a single key");
  const fire = transitionMessages([], result.firing);
  assert.equal(fire.length, 1, "multiple reason series must not double-notify");
  assert.match(fire[0]!, new RegExp(`ALERT ${KEY} \\(ticket\\):`));
  assert.match(fire[0]!, /runbook\.md#alert-community-facts-drain-failures/);
  const rising = ev(family(1002, 2, 2), result.state);
  assert.deepEqual(rising.firing, [KEY]);
  assert.equal(rising.state.communityFactsDrainStreak, 3, "streak is capped");
  assert.deepEqual(transitionMessages(result.firing, rising.firing), [], "repeated growth does not re-notify");
  const flat = ev(family(1002, 2, 2), rising.state);
  assert.deepEqual(flat.firing, [], "a valid no-growth window is the recovery");
  assert.equal(flat.state.communityFactsDrainStreak, 0);
  assert.match(transitionMessages(rising.firing, flat.firing)[0]!,
    new RegExp(`RESOLVED ${KEY}\\. Build: unknown\\. Logs: `));
  assert.deepEqual(transitionMessages(flat.firing, ev(family(1002, 2, 2), flat.state).firing), [],
    "recovery emits exactly once");
});

test("community-facts isolated failures and single bursts stay quiet", () => {
  let state = ev(family(0)).state;
  state = ev(family(1000, 500), state).state;
  assert.deepEqual(state.firing, [], "a single large burst is not sustained");
  assert.equal(state.communityFactsDrainStreak, 1);
  state = ev(family(1000, 500), state).state;
  assert.equal(state.communityFactsDrainStreak, 0, "the flat window breaks the streak");
  state = ev(family(1001, 500), state).state;
  state = ev(family(1002, 500), state).state;
  assert.deepEqual(state.firing, [], "two growth windows after the break do not fire");
  state = ev(family(1002, 500), state).state;
  assert.deepEqual(state.firing, []);
  assert.equal(state.communityFactsDrainStreak, 0);
});

test("community-facts flat windows and counter resets break the streak; a reset after firing resolves", () => {
  let state = ev(family(0)).state;
  state = ev(family(1), state).state;
  state = ev(family(2), state).state;
  state = ev(family(2), state).state;
  assert.equal(state.communityFactsDrainStreak, 0);
  state = ev(family(3), state).state;
  state = ev(family(4), state).state;
  assert.deepEqual(state.firing, []);
  const reset = ev(family(0), state);
  assert.deepEqual(reset.firing, []);
  assert.equal(reset.state.communityFactsDrainStreak, 0);
  assert.deepEqual(reset.state.communityFactsDrainFailures, { error: 0, timeout: 0, other: 0 },
    "the reset sample re-baselines every fixed reason");
  state = reset.state;
  for (const n of [1, 2, 3]) {
    state = ev(family(n), state).state;
    assert.deepEqual(state.firing, n === 3 ? [KEY] : []);
  }
  const restarted = ev(family(0), state);
  assert.deepEqual(restarted.firing, [], "a valid reset window proves the old process is gone");
  assert.match(transitionMessages(state.firing, restarted.firing)[0]!,
    new RegExp(`RESOLVED ${KEY}\\. Build: unknown\\. Logs: `));
});

test("community-facts detects a process reset from any reason, even when another reason overtook its old count", () => {
  let state = ev(family(100, 0)).state;
  state = ev(family(101, 1), state).state;
  state = ev(family(102, 2), state).state;
  assert.equal(state.communityFactsDrainStreak, 2);
  const restarted = ev(family(0, 200), state);
  assert.deepEqual(restarted.firing, [], "the summed count rising across a reset is not a third growth window");
  assert.equal(restarted.state.communityFactsDrainStreak, 0);
  assert.deepEqual(restarted.state.communityFactsDrainFailures, { error: 0, timeout: 200, other: 0 });
  state = restarted.state;
  for (const n of [201, 202, 203]) {
    state = ev(family(0, n), state).state;
    assert.deepEqual(state.firing, n === 203 ? [KEY] : []);
  }
});

const invalidSamples: Record<string, string[]> = {
  "missing family": [],
  "partial family": [line("error", 24)],
  "NaN": [line("error", "NaN"), line("timeout", 0), line("other", 0)],
  "infinity": [line("error", "+Inf"), line("timeout", 0), line("other", 0)],
  "negative": family(-1),
  "fractional": family(1.5),
  "unsafe integer": [line("error", "9007199254740992"), line("timeout", 0), line("other", 0)],
  "duplicate reason": [line("error", 24), line("error", 24), line("timeout", 0), line("other", 0)],
  "extra label": [`${FAMILY}{reason="error",member="synthetic-member"} 24`, line("timeout", 0), line("other", 0)],
};
for (const [name, lines] of Object.entries(invalidSamples)) {
  test(`community-facts ${name} breaks growth but never falsely resolves`, () => {
    let partial = ev(family(20)).state;
    for (const n of [21, 22]) partial = ev(family(n), partial).state;
    partial = ev(lines, partial).state;
    assert.deepEqual(partial.firing, []);
    assert.deepEqual(partial.communityFactsDrainFailures, {}, "missing/invalid evidence drops the baseline");
    assert.equal(partial.communityFactsDrainStreak, 0);
    const baseline = ev(family(30), partial);
    assert.equal(baseline.state.communityFactsDrainStreak, 0, "the first valid sample after a gap is only a baseline");
    let fresh = baseline.state;
    for (const n of [31, 32, 33]) {
      fresh = ev(family(n), fresh).state;
      assert.deepEqual(fresh.firing, n === 33 ? [KEY] : []);
    }

    const before = firingState();
    const unknown = ev(lines, before);
    assert.deepEqual(unknown.firing, before.firing, "missing/invalid evidence preserves an active incident");
    assert.deepEqual(transitionMessages(before.firing, unknown.firing), [], "no false recovery during failed evidence");
    const returnBaseline = ev(family(30), unknown.state);
    assert.deepEqual(returnBaseline.firing, before.firing, "a new baseline is not a quiet window");
    const stillGrowing = ev(family(31), returnBaseline.state);
    assert.deepEqual(stillGrowing.firing, before.firing, "ongoing growth must not falsely resolve after a gap");
    const quiet = ev(family(31), stillGrowing.state);
    assert.match(transitionMessages(stillGrowing.firing, quiet.firing)[0]!,
      new RegExp(`RESOLVED ${KEY}\\. Build: unknown\\. Logs: `));
  });
}

test("community-facts state survives DO reconstruction and reads legacy storage without new fields", () => {
  assert.deepEqual(EMPTY_STATE.communityFactsDrainFailures, {});
  assert.equal(EMPTY_STATE.communityFactsDrainStreak, 0);
  const { communityFactsDrainFailures: _failures, communityFactsDrainStreak: _streak, ...legacy } = EMPTY_STATE;
  let state = ev(family(200), legacy).state;
  assert.deepEqual(state.firing, [], "legacy state starts unbaselined: the first valid sample never fires");
  for (const n of [201, 202, 203]) {
    state = ev(family(n), JSON.parse(JSON.stringify(state))).state;
    assert.deepEqual(state.firing, n === 203 ? [KEY] : []);
  }
  assert.deepEqual(ev(family(204), JSON.parse(JSON.stringify(state))).firing, state.firing);
});

test("community-facts failed scrapes invalidate the baseline without changing firing or other rule state", () => {
  const previous = { ...firingState(), firing: [KEY, "job_stale:rank"], poolStreak: 2 };
  const interrupted = interruptCommunityFactsDrain(previous);
  assert.deepEqual(interrupted, { ...previous, communityFactsDrainFailures: {}, communityFactsDrainStreak: 0 });
  assert.equal(previous.communityFactsDrainFailures?.["error"], 23, "previous state is not mutated");
  const index = readFileSync(new URL("../src/index.ts", import.meta.url), "utf8");
  assert.ok(index.includes("interruptCommunityFactsDrain("),
    "the keepalive failed-scrape path must interrupt drain-growth evidence, not just dispatch drops");
});

test("community-facts subjects and state stay fixed even with high-cardinality or hostile labels", () => {
  const hostile = [line("member-123", 10), line("__proto__", 10), line("constructor", 10), `${FAMILY} 10`];
  let state = EMPTY_STATE;
  for (let i = 0; i < 5; i++) state = ev(hostile, state).state;
  assert.deepEqual(state.firing, []);
  assert.deepEqual(state.communityFactsDrainFailures, {});
  assert.equal(state.communityFactsDrainStreak, 0);
  let mixed = ev([...family(0), line("member-123", 0)]).state;
  for (const n of [1, 2, 3]) mixed = ev([...family(0), line("member-123", n)], mixed).state;
  assert.deepEqual(mixed.firing, [], "growth in an unknown reason never counts and never becomes a subject");
  assert.equal(mixed.communityFactsDrainStreak, 0);
  assert.deepEqual(Object.keys(mixed.communityFactsDrainFailures ?? {}), ["error", "timeout", "other"],
    "stored state is bounded to the fixed reasons");
});

test("community-facts family is an optional scrape input: absent parses, present-but-malformed fails the scrape", () => {
  assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[FAMILY], { reason: ["error", "timeout", "other"] });
  assert.ok(!Object.hasOwn(ALERT_INPUT_LABELS, FAMILY),
    "an older rollback container lacking the family must not invalidate its whole scrape");
  const valid = metricsBody();
  assert.ok(valid.includes(`${FAMILY}{reason="error"} 0`), "fixture must emit the family from the Rust contract");
  const older = valid.split("\n").filter((row) => !row.startsWith(FAMILY)).join("\n");
  assert.ok(parseAlertScrape(older), "older exposition without the family still parses");
  const partial = valid.split("\n").filter((row) => !row.startsWith(`${FAMILY}{reason="timeout"}`)).join("\n");
  assert.ok(parseAlertScrape(partial), "a partially present optional family does not fail the whole scrape");
  for (const value of ["NaN", "+Inf", "-1", "0.5", "1e20"]) {
    assert.equal(parseAlertScrape(valid.replace(`${FAMILY}{reason="error"} 0`, `${FAMILY}{reason="error"} ${value}`)), null, value);
  }
  assert.equal(parseAlertScrape(valid.replace(`${FAMILY}{reason="error"} 0`, `${FAMILY} 0`)), null,
    "a present row without its reason label is malformed");
  assert.equal(parseAlertScrape(valid.replace(`${FAMILY}{reason="error"} 0`, `${FAMILY}{reason="error",member="x"} 0`)), null,
    "a present row with an extra label is malformed");
});
