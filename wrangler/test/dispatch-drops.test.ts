import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  DISPATCH_DROP_SAMPLES, DISPATCH_LANES, EMPTY_STATE, RULES, evaluateMetrics,
  interruptDispatchDrops, parseExposition, ruleFor, transitionMessages,
  type MetricsAlertState,
} from "../src/alert-rules.ts";

const line = (lane: string, count: number | string) => `two_bot_dispatch_drops_total{lane="${lane}"} ${count}`;
const ev = (lines: string[], state = EMPTY_STATE) => evaluateMetrics(parseExposition(lines.join("\n")), state, 1_000_000);
function firingState(lane = "messages"): MetricsAlertState {
  let state = ev([line(lane, 20)]).state;
  for (const count of [21, 22, 23]) state = ev([line(lane, count)], state).state;
  assert.deepEqual(state.firing, [`dispatch_drops:${lane}`]);
  return state;
}

test("dispatch alert lanes match Rust and rule ids match the evidence catalog", () => {
  const read = (file: string) => readFileSync(new URL(`../../${file}`, import.meta.url), "utf8");
  const lanes = /pub const DISPATCH_LANES: &\[&str\] = &\[([\s\S]*?)\];/.exec(read("crates/core/src/metrics.rs"));
  assert.ok(lanes);
  assert.deepEqual(DISPATCH_LANES, [...lanes[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!));
  const rules = /pub const ALERT_RULE_IDS: \[&str; \d+\] = \[([\s\S]*?)\];/.exec(read("crates/core/src/evidence.rs"));
  assert.ok(rules);
  assert.deepEqual(RULES.map((r) => r.id), [...rules[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!));
  assert.equal(DISPATCH_DROP_SAMPLES, 3);
});

for (const lane of DISPATCH_LANES) {
  test(`dispatch ${lane} establishes a baseline, fires on the third rise and resolves when flat`, () => {
    const key = `dispatch_drops:${lane}`;
    let state = ev([line(lane, 1000)]).state;
    assert.deepEqual(state.firing, [], "the first nonzero sample is only a baseline");
    for (const [count, expected] of [[1001, []], [1002, []], [1003, [key]]] as const) {
      const result = ev([line(lane, count)], state);
      assert.deepEqual(result.firing, expected);
      state = result.state;
    }
    assert.equal(ruleFor(key)?.severity, "ticket", "reactions must not page either");
    const fire = transitionMessages([], state.firing);
    assert.equal(fire.length, 1);
    assert.match(fire[0]!, new RegExp(`ALERT ${key} \\(ticket\\):`));
    assert.match(fire[0]!, /not proof of gateway packet loss/);
    const rising = ev([line(lane, 1004)], state);
    assert.deepEqual(transitionMessages(state.firing, rising.firing), []);
    assert.equal(rising.state.dispatchDropStreaks?.[lane], 3, "streak is capped");
    const flat = ev([line(lane, 1004)], rising.state);
    assert.deepEqual(flat.firing, []);
    assert.equal(flat.state.dispatchDropStreaks?.[lane], 0);
    assert.deepEqual(transitionMessages(rising.firing, flat.firing), [`two-bot-next RESOLVED ${key}.`]);
    assert.deepEqual(transitionMessages(flat.firing, ev([line(lane, 1004)], flat.state).firing), []);
  });
}

test("dispatch lanes track independent streaks and do not add unrelated keys", () => {
  let state = ev(DISPATCH_LANES.map((lane) => line(lane, 0))).state;
  for (const n of [1, 2, 3]) {
    state = ev(DISPATCH_LANES.map((lane) => line(lane, lane === "messages" ? n : lane === "reactions" ? Math.max(0, n - 1) : 0)), state).state;
  }
  assert.deepEqual(state.firing, ["dispatch_drops:messages"]);
  assert.equal(state.dispatchDropStreaks?.["reactions"], 2);
  state = ev(DISPATCH_LANES.map((lane) => line(lane, lane === "messages" || lane === "reactions" ? 3 : 0)), state).state;
  assert.deepEqual(state.firing, ["dispatch_drops:reactions"], "one flat lane resolves while the other reaches three rises");
});

test("dispatch flat windows, bursts and counter resets interrupt growth", () => {
  let state = ev([line("busy", 0)]).state;
  state = ev([line("busy", 1000)], state).state;
  assert.deepEqual(state.firing, [], "a single large burst is not sustained");
  state = ev([line("busy", 1001)], state).state;
  state = ev([line("busy", 1001)], state).state;
  assert.equal(state.dispatchDropStreaks?.["busy"], 0);
  state = ev([line("busy", 1002)], state).state;
  state = ev([line("busy", 1003)], state).state;
  assert.deepEqual(state.firing, []);
  const reset = ev([line("busy", 0)], state);
  assert.deepEqual(reset.firing, []);
  assert.equal(reset.state.dispatchDropStreaks?.["busy"], 0);
  state = reset.state;
  for (const n of [1, 2, 3]) {
    state = ev([line("busy", n)], state).state;
    assert.deepEqual(state.firing, n === 3 ? ["dispatch_drops:busy"] : []);
  }
  const restarted = ev([line("busy", 0)], state);
  assert.deepEqual(transitionMessages(state.firing, restarted.firing), ["two-bot-next RESOLVED dispatch_drops:busy."]);
});

test("dispatch detects a process reset from any lane, even when another lane overtook its old count", () => {
  let state = ev([line("messages", 100), line("reactions", 0)]).state;
  state = ev([line("messages", 101), line("reactions", 1)], state).state;
  state = ev([line("messages", 102), line("reactions", 2)], state).state;
  const restarted = ev([line("messages", 0), line("reactions", 10)], state);
  assert.deepEqual(restarted.firing, [], "reactions must not reach a third rise across a process reset");
  assert.equal(restarted.state.dispatchDropStreaks?.["messages"], 0);
  assert.equal(restarted.state.dispatchDropStreaks?.["reactions"], 0);
  state = restarted.state;
  for (const n of [11, 12, 13]) {
    state = ev([line("messages", 0), line("reactions", n)], state).state;
    assert.deepEqual(state.firing, n === 13 ? ["dispatch_drops:reactions"] : []);
  }
});

const invalidSamples: Record<string, string[]> = {
  "missing lane": [],
  "NaN": [line("messages", "NaN")],
  "infinity": [line("messages", "+Inf")],
  "negative": [line("messages", -1)],
  "fractional": [line("messages", 1.5)],
  "unsafe integer": [line("messages", "9007199254740992")],
  "duplicate lane": [line("messages", 24), line("messages", 24)],
  "extra label": ['two_bot_dispatch_drops_total{lane="messages",member="synthetic-member"} 24'],
};
for (const [name, lines] of Object.entries(invalidSamples)) {
  test(`dispatch ${name} breaks growth but never falsely resolves`, () => {
    let partial = ev([line("messages", 20)]).state;
    for (const n of [21, 22]) partial = ev([line("messages", n)], partial).state;
    partial = ev(lines, partial).state;
    assert.deepEqual(partial.firing, []);
    assert.equal(partial.dispatchDrops?.["messages"], undefined);
    const baseline = ev([line("messages", 30)], partial);
    assert.equal(baseline.state.dispatchDropStreaks?.["messages"], 0);
    let fresh = baseline.state;
    for (const n of [31, 32, 33]) {
      fresh = ev([line("messages", n)], fresh).state;
      assert.deepEqual(fresh.firing, n === 33 ? ["dispatch_drops:messages"] : []);
    }

    const before = firingState();
    const unknown = ev(lines, before);
    assert.deepEqual(unknown.firing, before.firing);
    assert.deepEqual(transitionMessages(before.firing, unknown.firing), []);
    const returnBaseline = ev([line("messages", 30)], unknown.state);
    assert.deepEqual(returnBaseline.firing, before.firing, "a new baseline is not a quiet window");
    const stillGrowing = ev([line("messages", 31)], returnBaseline.state);
    assert.deepEqual(stillGrowing.firing, before.firing, "ongoing growth must not falsely resolve after a gap");
    const quiet = ev([line("messages", 31)], stillGrowing.state);
    assert.deepEqual(transitionMessages(stillGrowing.firing, quiet.firing), ["two-bot-next RESOLVED dispatch_drops:messages."]);
  });
}

test("dispatch missing data in one lane does not interrupt another", () => {
  let state = ev([line("messages", 0), line("reactions", 0)]).state;
  state = ev([line("messages", 1), line("reactions", 1)], state).state;
  state = ev([line("messages", 2)], state).state;
  state = ev([line("messages", 3), line("reactions", 100)], state).state;
  assert.deepEqual(state.firing, ["dispatch_drops:messages"]);
  assert.equal(state.dispatchDropStreaks?.["reactions"], 0);
});

test("dispatch state survives DO reconstruction and reads legacy storage without new fields", () => {
  const { dispatchDrops: _drops, dispatchDropStreaks: _streaks, ...legacy } = EMPTY_STATE;
  let state = ev([line("registry", 200)], legacy).state;
  assert.deepEqual(state.firing, []);
  for (const n of [201, 202, 203]) {
    state = ev([line("registry", n)], JSON.parse(JSON.stringify(state))).state;
    assert.deepEqual(state.firing, n === 203 ? ["dispatch_drops:registry"] : []);
  }
  assert.deepEqual(ev([line("registry", 204)], JSON.parse(JSON.stringify(state))).firing, state.firing);
});

test("dispatch failed scrapes invalidate baselines without changing firing or other rule state", () => {
  const previous = { ...firingState(), firing: ["dispatch_drops:messages", "job_stale:rank"], poolStreak: 2 };
  const interrupted = interruptDispatchDrops(previous);
  assert.deepEqual(interrupted, { ...previous, dispatchDrops: {}, dispatchDropStreaks: {} });
  assert.equal(previous.dispatchDrops?.["messages"], 23, "previous state is not mutated");
});

test("dispatch subjects and state stay fixed even with high-cardinality or hostile labels", () => {
  const hostile = [line("member-123", 10), line("__proto__", 10), line("constructor", 10), line("other", 10), "two_bot_dispatch_drops_total 10"];
  let state = EMPTY_STATE;
  for (let i = 0; i < 5; i++) state = ev(hostile, state).state;
  assert.deepEqual(state.firing, []);
  assert.deepEqual(state.dispatchDrops, {});
  assert.deepEqual(state.dispatchDropStreaks, {});
  const valid = ev(DISPATCH_LANES.map((lane) => line(lane, 0)), state);
  assert.deepEqual(Object.keys(valid.state.dispatchDrops ?? {}), DISPATCH_LANES);
});
