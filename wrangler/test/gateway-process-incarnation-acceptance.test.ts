import { test } from "node:test";
import assert from "node:assert/strict";
import {
  EMPTY_STATE, evaluateMetrics, type MetricsAlertState, type Sample,
} from "../src/alert-rules.ts";
import { ALERT_INPUT_LABELS, ALERT_OPTIONAL_INPUT_LABELS, parseAlertScrape } from "../src/metrics-scrape.ts";
import { applyStagingFaultInjection, UNPAIRED_FAULT_VAR } from "../src/staging-fault.ts";
import { metricsLines } from "./fixtures/metrics.ts";

// Expectations come from the wire contract, not the identity decoder under test.
const FAMILY = "two_bot_process_incarnation";
const UNPAIRED = "gateway_unpaired_disconnect";
const A = "0123456789abcdef80000000ffffffff";
const B = "00000000000000010000000000000000";
const WORDS_A = [19088743, 2309737967, 2147483648, 4294967295];
const WORDS_B = [0, 1, 0, 0];
const NOW = 1_000_000;
type AttributedState = MetricsAlertState & { gatewayProcessIncarnation?: string };

function identity(words = WORDS_A): Sample[] {
  return words.map((value, part) => ({ name: FAMILY, labels: { part: String(part) }, value }));
}
function counters(disconnects: number, resumes = 1, ready = 1): Sample[] {
  return [
    { name: "two_bot_gateway_disconnects_total", labels: {}, value: disconnects },
    { name: "two_bot_gateway_resumes_total", labels: {}, value: resumes },
    { name: "two_bot_gateway_events_total", labels: { event: "READY" }, value: ready },
  ];
}
function samples(disconnects: number, resumes = 1, ready = 1, words = WORDS_A): Sample[] {
  return [...counters(disconnects, resumes, ready), ...identity(words)];
}
function evaluate(input: Sample[], prev: AttributedState = EMPTY_STATE, gatewayReady: boolean | undefined = true) {
  return evaluateMetrics(input, prev, NOW, gatewayReady);
}
function attributed(overrides: Partial<AttributedState> = {}): AttributedState {
  return {
    ...EMPTY_STATE, gatewayUnpairedSeen: true, gatewayDisconnects: 0, gatewayRecoveries: 2,
    gatewayProcessIncarnation: A, ...overrides,
  };
}
function firingState(): AttributedState {
  return attributed({
    gatewayDisconnects: 1, gatewayUnpaired: 1, gatewayUnpairedStreak: 3, firing: [UNPAIRED],
  });
}
function pairing(state: AttributedState) {
  return {
    gatewayDisconnects: state.gatewayDisconnects, gatewayRecoveries: state.gatewayRecoveries,
    gatewayUnpairedSeen: state.gatewayUnpairedSeen, gatewayUnpaired: state.gatewayUnpaired,
    gatewayUnpairedStreak: state.gatewayUnpairedStreak,
    gatewayProcessIncarnation: state.gatewayProcessIncarnation,
    firing: state.firing.filter((key) => key === UNPAIRED),
  };
}
function assertHeld(input: Sample[], prev: AttributedState) {
  assert.deepEqual(pairing(evaluate(input, prev).state), pairing(prev));
}
function withoutIdentity(state: AttributedState): AttributedState {
  const legacy = { ...state };
  delete legacy.gatewayProcessIncarnation;
  return legacy;
}
function exposition(rows: Sample[]): string {
  return rows.map(({ name, labels, value }) => {
    const fields = Object.entries(labels).map(([key, val]) => `${key}="${val}"`).join(",");
    return `${name}${fields ? `{${fields}}` : ""} ${value}`;
  }).join("\n");
}
function fullScrape(rows: Sample[]): string {
  // The shared fixture may gain identity coverage; supply this test's rows exactly once.
  return [...metricsLines().filter((line) => !line.startsWith(FAMILY)), exposition(rows), ""].join("\n");
}

// A counter-only reset check misses this replacement: both recovery sums are 2.
test("aliased healthy replacement stays quiet through forty flat samples", () => {
  let state = attributed(); // old process: disconnects=0, RESUMED=1, READY=1
  for (let tick = 0; tick < 40; tick++) {
    const next = evaluate(samples(2, 1, 1, WORDS_B), state);
    assert.ok(!next.firing.includes(UNPAIRED), `replacement must be quiet at sample ${tick + 1}`);
    state = next.state;
  }
  assert.equal(state.gatewayUnpaired, 0);
  assert.equal(state.gatewayUnpairedStreak, 0);
  assert.equal(state.gatewayProcessIncarnation, B);
});

test("a firing attributed process holds on the boundary, then resolves on a flat sample", () => {
  const boundary = evaluate(samples(2, 1, 1, WORDS_B), firingState());
  assert.ok(boundary.firing.includes(UNPAIRED), "a boundary is not a recovery sample");
  assert.equal(boundary.state.gatewayUnpaired, 0);
  assert.equal(boundary.state.gatewayUnpairedStreak, 0);
  assert.equal((boundary.state as AttributedState).gatewayProcessIncarnation, B);
  const flat = evaluate(samples(2, 1, 1, WORDS_B), boundary.state);
  assert.ok(!flat.firing.includes(UNPAIRED));
});

test("a replacement with identical counters is still a boundary", () => {
  const prev = firingState();
  const boundary = evaluate(samples(1, 1, 1, WORDS_B), prev);
  assert.ok(boundary.firing.includes(UNPAIRED));
  assert.equal(boundary.state.gatewayUnpaired, 0);
  assert.equal(boundary.state.gatewayUnpairedStreak, 0);
  assert.ok(!evaluate(samples(1, 1, 1, WORDS_B), boundary.state).firing.includes(UNPAIRED));
});

test("a boundary never manufactures a new page from a subthreshold old streak", () => {
  const prev = attributed({ gatewayDisconnects: 1, gatewayUnpaired: 1, gatewayUnpairedStreak: 2 });
  const next = evaluate(samples(2, 1, 1, WORDS_B), prev);
  assert.ok(!next.firing.includes(UNPAIRED));
  assert.equal(next.state.gatewayUnpairedStreak, 0);
});

for (const gatewayReady of [true, false, undefined]) {
  test(`stable incarnation outstanding loss fires third and stays; readiness=${gatewayReady}`, () => {
    let state = attributed();
    for (let tick = 1; tick <= 40; tick++) {
      const next = evaluateMetrics(samples(2), state, NOW + tick * 60, gatewayReady);
      assert.equal(next.firing.includes(UNPAIRED), tick >= 3, `same-process loss at sample ${tick}`);
      assert.equal(next.state.gatewayUnpaired, 1);
      assert.equal(next.state.gatewayUnpairedStreak, tick);
      state = next.state;
    }
  });
}

test("boundary attribution never changes the independent readiness rule", () => {
  const prev = firingState();
  prev.firing = [UNPAIRED, "gateway_not_ready"];
  prev.gatewayNotReadySeen = true;
  prev.gatewayNotReadyStreak = 3;
  const ready = evaluateMetrics(samples(2, 1, 1, WORDS_B), prev, NOW, true);
  assert.ok(ready.firing.includes(UNPAIRED), "ready is not recovery of an attributed loss");
  assert.ok(!ready.firing.includes("gateway_not_ready"));
  const down = evaluateMetrics(samples(2, 1, 1, WORDS_B), prev, NOW, false);
  assert.ok(down.firing.includes(UNPAIRED));
  assert.ok(down.firing.includes("gateway_not_ready"));
  const unattributed = evaluateMetrics(counters(8, 8, 8), prev, NOW, true);
  assert.ok(unattributed.firing.includes(UNPAIRED));
  assert.ok(!unattributed.firing.includes("gateway_not_ready"));
});

test("new disconnects after a quiet boundary get their own three-sample threshold", () => {
  let state = evaluate(samples(2, 1, 1, WORDS_B), attributed()).state;
  for (let tick = 1; tick <= 4; tick++) {
    const next = evaluate(samples(3, 1, 1, WORDS_B), state);
    assert.equal(next.firing.includes(UNPAIRED), tick >= 3);
    assert.equal(next.state.gatewayUnpairedStreak, tick);
    state = next.state;
  }
});

test("new disconnects immediately after a firing boundary hold the existing page", () => {
  const boundary = evaluate(samples(2, 1, 1, WORDS_B), firingState());
  const loss = evaluate(samples(3, 1, 1, WORDS_B), boundary.state);
  assert.ok(loss.firing.includes(UNPAIRED));
  assert.equal(loss.state.gatewayUnpairedStreak, 1);
  assert.ok(!evaluate(samples(3, 2, 1, WORDS_B), loss.state).firing.includes(UNPAIRED));
});

for (const [name, recovered] of [
  ["RESUMED", samples(7, 2, 1)], ["fresh READY", samples(7, 1, 2)],
] as const) {
  test(`same-process ${name} growth clears even multiple retries`, () => {
    const clear = evaluate(recovered, firingState());
    assert.ok(!clear.firing.includes(UNPAIRED));
    assert.equal(clear.state.gatewayUnpaired, 0);
    assert.equal(clear.state.gatewayUnpairedStreak, 0);
    assert.ok(!evaluate(recovered, clear.state).firing.includes(UNPAIRED));
  });
}

for (const [name, rows] of [
  ["disconnect decrease", samples(0)], ["recovery sum decrease", samples(1, 0, 1)],
] as const) {
  test(`counter-only restart semantics survive with stable identity: ${name}`, () => {
    const boundary = evaluate(rows, firingState());
    assert.ok(boundary.firing.includes(UNPAIRED));
    assert.equal(boundary.state.gatewayUnpaired, 0);
    assert.equal(boundary.state.gatewayUnpairedStreak, 0);
    assert.ok(!evaluate(rows, boundary.state).firing.includes(UNPAIRED));
  });
}

test("first valid identity and counters establish a non-firing baseline", () => {
  const next = evaluate(samples(99, 0, 0));
  assert.ok(!next.firing.includes(UNPAIRED));
  assert.equal(next.state.gatewayUnpaired, 0);
  assert.equal(next.state.gatewayUnpairedStreak, 0);
  assert.equal((next.state as AttributedState).gatewayProcessIncarnation, A);
});

test("canonical hex uses four numeric limbs in part order, not sample order", () => {
  const next = evaluate([...counters(0), ...identity().reverse()]);
  assert.equal((next.state as AttributedState).gatewayProcessIncarnation, A);
  assert.match((next.state as AttributedState).gatewayProcessIncarnation!, /^[0-9a-f]{32}$/);
});

test("first identity adoption processes a new loss instead of treating adoption as restart", () => {
  const legacy = withoutIdentity(attributed());
  const next = evaluate(samples(2), legacy);
  assert.equal(next.state.gatewayUnpaired, 1);
  assert.equal(next.state.gatewayUnpairedStreak, 1);
  assert.equal((next.state as AttributedState).gatewayProcessIncarnation, A);
});

test("adoption preserves a legacy pending streak and fires its third sample", () => {
  const legacy = withoutIdentity(attributed({ gatewayDisconnects: 2, gatewayUnpaired: 1, gatewayUnpairedStreak: 2 }));
  const next = evaluate(samples(2), legacy);
  assert.ok(next.firing.includes(UNPAIRED));
  assert.equal(next.state.gatewayUnpairedStreak, 3);
});

test("adoption alone cannot clear an already firing legacy page", () => {
  const next = evaluate(samples(1), withoutIdentity(firingState()));
  assert.ok(next.firing.includes(UNPAIRED));
  assert.equal(next.state.gatewayUnpaired, 1);
  assert.equal(next.state.gatewayUnpairedStreak, 4);
});

test("real recovery during legacy adoption still clears normally", () => {
  const next = evaluate(samples(7, 2, 1), withoutIdentity(firingState()));
  assert.ok(!next.firing.includes(UNPAIRED));
  assert.equal(next.state.gatewayUnpaired, 0);
});

test("legacy containers and legacy state keep counter-only pairing and restart behavior", () => {
  let state = evaluate(counters(0)).state;
  for (let tick = 1; tick <= 4; tick++) {
    const next = evaluate(counters(2), state);
    assert.equal(next.firing.includes(UNPAIRED), tick >= 3);
    state = next.state;
  }
  const restart = evaluate(counters(0, 0, 0), state);
  assert.ok(restart.firing.includes(UNPAIRED));
  assert.equal(restart.state.gatewayUnpaired, 0);
  assert.ok(!evaluate(counters(0, 0, 0), restart.state).firing.includes(UNPAIRED));
  assert.ok(!evaluate(counters(2, 2, 1), state).firing.includes(UNPAIRED));
  assert.equal((state as AttributedState).gatewayProcessIncarnation, undefined);
});

for (const stored of ["garbage", A.toUpperCase(), "0".repeat(32), A.slice(1), `${A}0`, "", 7, null]) {
  test(`malformed stored identity is unknown, never boundary proof: ${JSON.stringify(stored)}`, () => {
    const prev = { ...firingState(), gatewayProcessIncarnation: stored } as unknown as AttributedState;
    const next = evaluate(samples(2, 1, 1, WORDS_B), prev);
    assert.ok(next.firing.includes(UNPAIRED));
    assert.equal(next.state.gatewayUnpaired, 1);
    assert.equal(next.state.gatewayUnpairedStreak, 4);
    assert.equal((next.state as AttributedState).gatewayProcessIncarnation, B);
  });
}

const invalidIdentities: Record<string, Sample[]> = {
  absent: [],
  partial: identity().slice(0, 3),
  duplicate: [...identity(), identity()[0]!],
  "duplicate part replacing missing part": [identity()[0]!, identity()[0]!, ...identity().slice(2)],
  "extra part": [...identity(), { name: FAMILY, labels: { part: "4" }, value: 1 }],
  "extra label": identity().map((row, i) => i === 0 ? { ...row, labels: { part: "0", identity: A } } : row),
  "wrong label": identity().map((row, i) => i === 0 ? { ...row, labels: { word: "0" } } : row),
  "no label": identity().map((row, i) => i === 0 ? { ...row, labels: {} } : row),
  "noncanonical part": identity().map((row, i) => i === 0 ? { ...row, labels: { part: "00" } } : row),
  "all zero": identity([0, 0, 0, 0]),
};
for (const value of [NaN, Infinity, -Infinity, -1, 0.5, 4294967296, Number.MAX_SAFE_INTEGER + 1]) {
  invalidIdentities[`invalid limb ${value}`] = identity().map((row, i) => i === 0 ? { ...row, value } : row);
}

for (const [name, rows] of Object.entries(invalidIdentities)) {
  test(`direct evaluator holds attributed counters, streak, identity and firing on ${name}`, () => {
    // Recovery/decrease must not slip through a missing or untrusted attribution.
    assertHeld([...counters(8, 8, 8), ...rows], firingState());
    assertHeld([...counters(0, 0, 0), ...rows], firingState());
    assertHeld([...counters(8, 8, 8), ...rows], attributed({ gatewayUnpaired: 1, gatewayUnpairedStreak: 2 }));
  });
  if (name !== "absent") {
    test(`optional identity family is rejected by scrape validation on ${name}`, () => {
      assert.equal(parseAlertScrape(fullScrape(rows)), null);
    });
  }
}

for (const value of ["NaN", "+Inf", "-Inf", "1e999", "-1", "0.5", "4294967296", "9007199254740992", "garbage"]) {
  test(`scrape rejects invalid identity wire number ${value}`, () => {
    const text = fullScrape(identity()).replace('two_bot_process_incarnation{part="0"} 19088743', `two_bot_process_incarnation{part="0"} ${value}`);
    assert.equal(parseAlertScrape(text), null);
  });
}

test("a missing-identity hold preserves the last valid boundary for later recovery", () => {
  const prev = firingState();
  const held = evaluate(counters(8, 8, 8), prev);
  assert.deepEqual(pairing(held.state), pairing(prev));
  const recovered = evaluate(samples(8, 2, 1), held.state);
  assert.ok(!recovered.firing.includes(UNPAIRED));
  assert.equal((recovered.state as AttributedState).gatewayProcessIncarnation, A);
});

test("canonical identity survives JSON persistence and still recognizes replacement", () => {
  const written = evaluate(samples(0)).state;
  const restored: AttributedState = JSON.parse(JSON.stringify(written));
  assert.equal(restored.gatewayProcessIncarnation, A);
  const boundary = evaluate(samples(2, 1, 1, WORDS_B), restored);
  assert.equal(boundary.state.gatewayUnpaired, 0);
  assert.equal((boundary.state as AttributedState).gatewayProcessIncarnation, B);
});

test("an old-container scrape with identity absent remains valid", () => {
  assert.ok(parseAlertScrape(fullScrape([])));
  assert.equal(ALERT_INPUT_LABELS[FAMILY], undefined, "identity cannot become required on rollback");
  assert.deepEqual(ALERT_OPTIONAL_INPUT_LABELS[FAMILY], { part: ["0", "1", "2", "3"] });
});

for (const words of [WORDS_A, WORDS_B, [0, 0, 0, 4294967295], [4294967295, 4294967295, 4294967295, 4294967295]]) {
  test(`valid fixed identity limbs are accepted: ${words.join(",")}`, () => {
    const parsed = parseAlertScrape(fullScrape(identity(words).reverse()));
    assert.ok(parsed);
    assert.equal(parsed.filter((row) => row.name === FAMILY).length, 4);
  });
}

test("parsed scrapes carry an aliased replacement through the real evaluator", () => {
  const scrape = (disconnects: number, rows: Sample[]) => {
    const text = fullScrape(rows).replace("two_bot_gateway_disconnects_total 0", `two_bot_gateway_disconnects_total ${disconnects}`)
      .replace("two_bot_gateway_resumes_total 0", "two_bot_gateway_resumes_total 1")
      .replace('two_bot_gateway_events_total{event="READY"} 0', 'two_bot_gateway_events_total{event="READY"} 1');
    const parsed = parseAlertScrape(text);
    assert.ok(parsed);
    return parsed;
  };
  let state = evaluate(scrape(0, identity())).state;
  for (let tick = 0; tick < 40; tick++) {
    const next = evaluate(scrape(2, identity(WORDS_B)), state);
    assert.ok(!next.firing.includes(UNPAIRED), `validated replacement at sample ${tick + 1}`);
    state = next.state;
  }
});

const invalidCounters: Record<string, Sample[]> = {};
for (let index = 0; index < 3; index++) {
  const rows = counters(2);
  invalidCounters[`missing counter ${index}`] = rows.filter((_, i) => i !== index);
  invalidCounters[`duplicate counter ${index}`] = [...rows, rows[index]!];
  invalidCounters[`extra-label counter ${index}`] = rows.map((row, i) => i === index ? { ...row, labels: { ...row.labels, extra: "x" } } : row);
  for (const value of [NaN, Infinity, -1, 0.5, Number.MAX_SAFE_INTEGER + 1]) {
    invalidCounters[`counter ${index} value ${value}`] = rows.map((row, i) => i === index ? { ...row, value } : row);
  }
}
invalidCounters["unsafe recovery sum"] = counters(2, 4503599627370496, 4503599627370496);
for (const [name, rows] of Object.entries(invalidCounters)) {
  test(`bad counters cannot consume a replacement identity: ${name}`, () => {
    const prev = firingState();
    const bad = evaluate([...rows, ...identity(WORDS_B)], prev);
    assert.deepEqual(pairing(bad.state), pairing(prev));
    const boundary = evaluate(samples(2, 1, 1, WORDS_B), bad.state);
    assert.ok(boundary.firing.includes(UNPAIRED));
    assert.equal(boundary.state.gatewayUnpaired, 0);
    assert.equal(boundary.state.gatewayUnpairedStreak, 0);
    assert.equal((boundary.state as AttributedState).gatewayProcessIncarnation, B);
    assert.ok(!evaluate(samples(2, 1, 1, WORDS_B), boundary.state).firing.includes(UNPAIRED));
  });
}

test("bad counters cannot adopt the first identity either", () => {
  const prev = withoutIdentity(firingState());
  const bad = evaluate([...counters(NaN), ...identity()], prev);
  assert.deepEqual(pairing(bad.state), pairing(prev));
  const adopted = evaluate(samples(2), bad.state);
  assert.ok(adopted.firing.includes(UNPAIRED));
  assert.equal(adopted.state.gatewayUnpairedStreak, 4);
  assert.equal((adopted.state as AttributedState).gatewayProcessIncarnation, A);
});

test("staging overlay preserves attribution, injected loss fires, real recovery clears after disarm", () => {
  const env = { TWO_DEPLOY_ENV: "staging", [UNPAIRED_FAULT_VAR]: "1" };
  const input = samples(0);
  const original = structuredClone(input);
  let state = attributed();
  for (let tick = 1; tick <= 5; tick++) {
    const overlay = applyStagingFaultInjection(input, state, env);
    assert.deepEqual(overlay.filter((row) => row.name === FAMILY), identity());
    const next = evaluate(overlay, state);
    assert.equal(next.firing.includes(UNPAIRED), tick >= 3);
    state = next.state;
  }
  assert.deepEqual(input, original, "overlay must not mutate scraped identity or counters");
  // Hold the synthetic counter high enough to avoid testing a counter decrease
  // instead of the intended real-recovery path on disarm.
  const recovered = samples(state.gatewayDisconnects, 2, 1);
  const disarmed = applyStagingFaultInjection(recovered, state, { TWO_DEPLOY_ENV: "staging" });
  assert.equal(disarmed, recovered);
  const clear = evaluate(disarmed, state);
  assert.ok(!clear.firing.includes(UNPAIRED));
  assert.equal(clear.state.gatewayUnpaired, 0);
  assert.equal((clear.state as AttributedState).gatewayProcessIncarnation, A);
});
